//! PVE 交互层：**只用原生命令**，不走 HTTP API、不需要 token。
//!
//! 分工是固定的：
//! - **读**：`pvesh get <path> --output-format json`，结构化、字段稳定
//! - **写**：`pct` / `pct set` / `pct exec` / `ip` / `systemctl`，与管理员手工操作一致
//!
//! 为什么不走 API：`pct exec`、写 `/etc/pve/lxc/*.conf`、`systemctl start labmirror@`、
//! `tc mirred` 这四件事 API 全都做不到。本地运行时用 API 只会多一条通道、多一套凭证。
//!
//! **读的纪律**：状态一律走 `GET /cluster/resources?type=vm` 一次取全，
//! 绝不在循环里 `pct config` —— `/etc/pve` 是单线程 FUSE，N+1 次读会把整个节点拖慢。

use crate::core::runner::{CommandSpec, Output, RunError, Runner};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// 从 `/etc/network/interfaces` 里整段删掉某个网桥的 awk 程序（`-v n=<桥名>`）。
///
/// 为什么要按块结构删、而不是按行数删：一个段的行数并不固定（配了管理地址
/// 会多一行 `up ip addr replace …`），按固定行数砍会在没有那一行时连带吃掉
/// 下一段的首行——那是别人的配置。这里只认三种行：段首 `auto <n>`、段内的
/// 缩进行、以及结尾的空行；遇到别的就立刻收手。
///
/// 抽成常量是为了让单测能真的跑它（见 `unpersist_awk_removes_only_its_block`）。
const BRIDGE_UNPERSIST_AWK: &str = r#"$0 == "auto " n { skip = 1; next }
skip && $0 ~ /^[ \t]*$/ { skip = 0; next }
skip && ($0 ~ /^[ \t]/ || $0 == "iface " n " inet manual") { next }
skip { skip = 0 }
{ print }"#;

/// 比较 PVE 存的 `net<N>` 串与我们期望的三项是否一致。
///
/// 只比 `name` / `bridge` / `ip`：`hwaddr`、`type`、`firewall` 我们不管，也不能
/// 因为它们在就重写一遍——重写等于热插拔，容器里那块网卡会被换成新接口，
/// 运行时配的地址当场丢失（真机现象：清完网桥后网关容器 eth0 空地址，
/// 整片实验网断掉，而步骤状态还是"已完成"，没人去补）。
///
/// `ip` 缺省视为 `dhcp`（PVE 的默认值），`name` 缺省视为 `eth<idx>`。
fn net_spec_matches(have: &str, idx: u32, name: &str, bridge: &str, ip: &str) -> bool {
    let field = |k: &str| {
        have.split(',')
            .filter_map(|kv| kv.split_once('='))
            .find(|(kk, _)| kk.trim() == k)
            .map(|(_, v)| v.trim().to_string())
    };
    let have_name = field("name").unwrap_or_else(|| format!("eth{idx}"));
    let have_ip = field("ip").unwrap_or_else(|| "dhcp".to_string());
    have_name == name
        && field("bridge").as_deref() == Some(bridge)
        && have_ip == ip
}

pub struct Pve {
    runner: Arc<dyn Runner>,
    pub node: String,
    /// `path + args` → (取到的时间, 有效期, 原始 JSON)。
    ///
    /// 为什么要缓存：`/etc/pve` 是单线程 FUSE，每次 `pvesh get` 都是 fork 一个
    /// 进程 + 一次 FUSE 往返（实测 0.3–1.7 秒）。而**同一次页面刷新**里
    /// `/api/hosts`、`/api/topology`、`/api/doctor` 问的是同一批东西，
    /// 各跑一遍纯属浪费——真机上量出来的 1.0 秒、1.0 秒、6.7 秒全在这儿。
    ///
    /// 存 `Value` 而不是 `T`：`get_json` 是泛型的，缓存不能按类型分装。
    cache: Mutex<HashMap<String, (Instant, Duration, serde_json::Value)>>,
    /// 后台刷新队列。见 `get_json_view`：过期缓存还给它一个"先返回旧值"的
    /// 机会，真正的 `pvesh` 由 `start_refresher` 起的那个任务慢慢跑。
    /// 队列容量无上限是因为生产端只会在"缓存过期 + 有人点开"时投递，
    /// 而消费端一条一条跑，重复的 key 会被 `inflight` 挡掉。
    refresh_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<RefreshJob>>>,
    /// 正在后台刷新的 key。没有它，用户连点五下就会排出五个 `pvesh`，
    /// 每个都是 ~1 秒的 perl 进程——那就等于把缓存省下的内存又还回去了。
    inflight: Mutex<std::collections::HashSet<String>>,
    /// `pct exec` 串行闸。单个 `pct exec`（lxc-attach）实测峰值 ~176 MB，全部
    /// 记在 labpanel 的 cgroup 上；编排批内并发 3 个 + preview 的 pvesh 就是
    /// OOM 的直接来源。所有 exec 走这一个闸，容量 1 = 全局串行：峰值从
    /// 3×176+ 压到 1×176+，代价是 bootstrap 阶段墙钟时间变长（可接受）。
    exec_gate: Arc<tokio::sync::Semaphore>,
}

/// 不同类型的读，新鲜度要求不一样。宁可短，也不要"看起来是新的"。
///
/// - 容器/VM 状态：8 秒。写操作（电源、apply、retire）之后会显式
///   `clear_cache()`，所以"点了没反应"不会发生；而 8 秒能让拓扑页与主机页
///   在同一次浏览里共用一次查询——这两个页面问的是同一份 `cluster/resources`。
/// - 版本、节点、网桥：5 分钟。这些是改 PVE 配置才会变的东西。
/// - 存储：2 分钟（可能会挂/卸存储，别太懒）。
/// - 模板清单：5 分钟（`pveam` 要解析本地索引，是这堆读里最慢的一项）。
///
/// 每个 `pvesh` 都是 1 个 perl 进程、约 1 秒、charged 60–80 MB，所以缓存
/// 不只是省时间，也是在少压 `/etc/pve` 这个单线程 FUSE。
fn cache_ttl(path: &str) -> Duration {
    if path.starts_with("/cluster/resources") {
        Duration::from_secs(8)
    } else if path.ends_with("/storage") {
        Duration::from_secs(120)
    } else if path == "/version" || path == "/nodes" || path.ends_with("/network") {
        Duration::from_secs(300)
    } else if path.ends_with("/aplinfo") {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(15)
    }
}

/// 过期缓存还能"先拿去给人看"的上限。
///
/// 超过这个年龄就不再兜旧值，宁可让请求去等——一个五分钟前的容器列表
/// 已经不该被当成现状了。
const STALE_MAX: Duration = Duration::from_secs(300);

/// 一次后台刷新请求。用 `key` 回来清 `inflight`。
struct RefreshJob {
    key: String,
    path: String,
    extra: Vec<String>,
}

/// 缓存键：路径 + 参数。参数是空的就不留尾空格，免得同一件事两个键。
fn cache_key(path: &str, extra: &[&str]) -> String {
    if extra.is_empty() {
        path.to_string()
    } else {
        format!("{path} {}", extra.join(" "))
    }
}

/// 解析 `pct config` 里的一行挂载点：`mp0: local-lvm:vm-102-disk-1,mp=/data,size=2G`
/// → (序号, 挂载点, 容量 GB, 卷名)。
///
/// 冒号后面那个空格必须吃掉：不 trim 会把卷名解析成 `" local-lvm:..."`，
/// 拼回命令行就得多一个前导空格，`pct set` 直接 255（真机踩过）。
fn parse_mp_line(line: &str) -> Option<(u32, String, Option<u64>, String)> {
    let rest = line.trim().strip_prefix("mp")?;
    let (idx, val) = rest.split_once(':')?;
    let idx = idx.parse::<u32>().ok()?;
    let mut mount = String::new();
    let mut size_gb = None;
    let mut volid = String::new();
    for kv in val.split(',') {
        let kv = kv.trim();
        if let Some(m) = kv.strip_prefix("mp=") {
            mount = m.to_string();
        } else if let Some(sz) = kv.strip_prefix("size=") {
            size_gb = parse_pve_size(sz);
        } else if kv.contains(':') && volid.is_empty() {
            volid = kv.to_string();
        }
    }
    Some((idx, mount, size_gb, volid))
}

/// PVE 的容量写法（`2G`、`512M`、纯字节数）→ GB。
fn parse_pve_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(v) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        return v.trim().parse::<u64>().ok();
    }
    if let Some(v) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        // 向上取整：512M 也要算 1G 才不会误判"容量已够"而跳过扩容
        return v.trim().parse::<u64>().ok().map(|m| (m + 1023) / 1024);
    }
    if let Some(v) = s.strip_suffix('T').or_else(|| s.strip_suffix('t')) {
        return v.trim().parse::<u64>().ok().map(|t| t * 1024);
    }
    s.parse::<u64>().ok().map(|b| (b + (1 << 30) - 1) / (1 << 30))
}

fn from_value<T: DeserializeOwned>(path: &str, v: serde_json::Value) -> Result<T, PveError> {
    serde_json::from_value(v).map_err(|source| PveError::Json {
        cmd: format!("pvesh get {path}"),
        source,
    })
}

/// `pvesh get /version`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Version {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub repoid: String,
}

/// `/cluster/resources?type=vm` 的一项（LXC 与 QEMU 混在一起）
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ClusterResource {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub vmid: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub maxmem: Option<u64>,
    #[serde(default)]
    pub maxcpu: Option<f64>,
    #[serde(default)]
    pub uptime: Option<u64>,
}

impl ClusterResource {
    /// `lxc/100` → `Some(100)`
    pub fn vmid_parsed(&self) -> Option<u32> {
        self.vmid.or_else(|| {
            self.id
                .split_once('/')
                .and_then(|(_, n)| n.parse::<u32>().ok())
        })
    }

    pub fn is_lxc(&self) -> bool {
        self.id.starts_with("lxc/")
    }
}

/// `/nodes/{n}/network` 的一项
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Iface {
    #[serde(default)]
    pub iface: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub cidr: Option<String>,
    #[serde(default)]
    pub active: Option<i32>,
    #[serde(default)]
    pub bridge_ports: Option<String>,
    #[serde(default)]
    pub comments: Option<String>,
}

/// `/nodes/{n}/storage` 的一项
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Storage {
    #[serde(default)]
    pub storage: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub avail: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub active: Option<i32>,
}

impl Storage {
    pub fn supports(&self, what: &str) -> bool {
        self.content
            .as_deref()
            .map(|c| c.split(',').any(|x| x.trim() == what))
            .unwrap_or(false)
    }

    /// 支持快照的类型（厚 LVM 不支持）
    pub fn supports_snapshot(&self) -> bool {
        matches!(self.kind.as_str(), "zfspool" | "dir" | "btrfs" | "lvmthin")
    }
}

/// `/nodes/{n}/aplinfo` 的一项
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AplTemplate {
    #[serde(default)]
    pub template: String,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub os: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PveError {
    #[error("命令失败: {0}")]
    Run(#[from] crate::core::runner::RunError),
    #[error("`{cmd}` 输出不是合法 JSON: {source}")]
    Json {
        cmd: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("PVE 版本 {0} 低于要求（9.0），拒绝继续")]
    TooOld(String),
    #[error("{0}")]
    Bad(String),
    #[error("`{cmd}` 被取消")]
    Cancelled { cmd: String },
}

impl Pve {
    /// `wan_bridge` 不在这里存一份：它只属于 `config.pve`，两处并存就是两个真相源。
    pub fn new(runner: Arc<dyn Runner>, node: impl Into<String>) -> Self {
        Self {
            runner,
            node: node.into(),
            cache: Mutex::new(HashMap::new()),
            refresh_tx: Mutex::new(None),
            inflight: Mutex::new(std::collections::HashSet::new()),
            exec_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// 把全部读缓存**标记为过期**（而不是删掉）。写操作（建/删/关机、apply）
    /// 之后调一次。
    ///
    /// 为什么不是 `clear()`：删了之后第一次打开主机页/体检页又是冷启动
    /// （实测 1.0 秒 / 4.2 秒），而用户刚点完 apply 正是最想马上看一眼的时候。
    /// 标记过期就两全：严格读（执行器）立刻重取，展示读先给旧值、后台补新。
    pub fn clear_cache(&self) {
        if let Ok(mut m) = self.cache.lock() {
            // 有效期清零 = 一律算过期；真正的 `ttl` 在下次取值时重新写入。
            for (_, (_, ttl, _)) in m.iter_mut() {
                *ttl = Duration::ZERO;
            }
        }
    }

    /// `pvesh get ... --output-format json`。**过期就必须等**——执行器
    /// 用这条路径判断"容器在不在"，拿旧快照去做决定会越做越错。
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        extra: &[&str],
        cancel: &CancellationToken,
    ) -> Result<T, PveError> {
        let value = self.get_json_mode(path, extra, cancel, false).await?;
        from_value(path, value)
    }

    /// 同上，但**过期缓存先给旧值**，同时把一个刷新任务丢进后台队列。
    ///
    /// 只给"给人看"的接口用（主机页、体检页）。真机上这两页的冷启动分别是
    /// 1.0 秒和 4.2 秒——慢的不是计算，是等 perl 进程和 `/etc/pve` 的
    /// 单线程 FUSE 往返。可这些数据本来就是"几秒前的现状"，让人盯着转圈
    /// 去换那几秒的新鲜度不值当：先画出来，下一秒再悄悄换成新的。
    async fn get_json_view<T: DeserializeOwned>(
        &self,
        path: &str,
        extra: &[&str],
        cancel: &CancellationToken,
    ) -> Result<T, PveError> {
        let value = self.get_json_mode(path, extra, cancel, true).await?;
        from_value(path, value)
    }

    async fn get_json_mode(
        &self,
        path: &str,
        extra: &[&str],
        cancel: &CancellationToken,
        allow_stale: bool,
    ) -> Result<serde_json::Value, PveError> {
        let key = cache_key(path, extra);
        // 锁不跨 await：命中判断在一段同步代码里做完，拿不到锁就当作没缓存
        // （缓存坏了不该让请求失败）。
        let hit = self.cache.lock().ok().and_then(|m| m.get(&key).cloned());
        if let Some((at, ttl, v)) = hit {
            let age = at.elapsed();
            if age < ttl {
                return Ok(v);
            }
            if allow_stale && age < STALE_MAX {
                self.queue_refresh(&key, path, extra);
                return Ok(v);
            }
        }
        self.fetch_value(path, extra, cancel).await
    }

    /// 真的去问 PVE，并把结果写进缓存。首次读取与后台刷新共用这一条。
    async fn fetch_value(
        &self,
        path: &str,
        extra: &[&str],
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, PveError> {
        let mut args: Vec<String> = vec![
            "get".into(),
            path.into(),
            "--output-format".into(),
            "json".into(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        let spec = CommandSpec::new("pvesh", args).with_timeout(Duration::from_secs(60));
        let out = self.runner.run("pvesh", &spec, None, cancel, true).await?;
        let text = out.stdout.trim();
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|source| PveError::Json {
                cmd: format!("pvesh get {path}"),
                source,
            })?;
        if let Ok(mut m) = self.cache.lock() {
            // 键空间是"路径 + 参数"，正常只有几十条；给个上限防病态增长
            if m.len() >= 128 {
                m.clear();
            }
            m.insert(
                cache_key(path, extra),
                (Instant::now(), cache_ttl(path), value.clone()),
            );
        }
        Ok(value)
    }

    /// 把一次"补新"排进后台，同一个 key 同时只会有一个在跑。
    fn queue_refresh(&self, key: &str, path: &str, extra: &[&str]) {
        let Some(tx) = self.refresh_tx.lock().ok().and_then(|g| g.clone()) else {
            // 没起后台任务（CLI 路径、单测）就当作没有这回事。
            return;
        };
        let Ok(mut flying) = self.inflight.lock() else {
            return;
        };
        if !flying.insert(key.to_string()) {
            return;
        }
        drop(flying);
        let job = RefreshJob {
            key: key.to_string(),
            path: path.to_string(),
            extra: extra.iter().map(|s| s.to_string()).collect(),
        };
        if tx.send(job).is_err() {
            if let Ok(mut f) = self.inflight.lock() {
                f.remove(key);
            }
        }
    }

    /// 起后台刷新线程。**一个**，串行跑——`pvesh` 是 perl 进程，并发几个就会
    /// 把 systemd 的 MemoryMax 顶穿（真机上 OOM 过）。
    pub fn start_refresher(self: &Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RefreshJob>();
        if let Ok(mut g) = self.refresh_tx.lock() {
            *g = Some(tx);
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let extra: Vec<&str> = job.extra.iter().map(|s| s.as_str()).collect();
                let cancel = CancellationToken::new();
                // 失败了就把缓存留着让它自然过老——下次点开还会再试一次，
                // 不必在这里重试到天荒地老。
                let _ = me.fetch_value(&job.path, &extra, &cancel).await;
                if let Ok(mut f) = me.inflight.lock() {
                    f.remove(&job.key);
                }
            }
        });
    }

    /// 开机预热：服务刚起来时"体检页/主机页"的缓存是空的，第一次点开
    /// 就得等 1 秒到 4 秒。启动后空闲时按顺序把它们问一遍，等管理员
    /// 真去点的时候已经有值了。
    pub fn warm(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let net = format!("/nodes/{}/network", me.node);
            let st = format!("/nodes/{}/storage", me.node);
            let jobs: Vec<(String, Vec<&'static str>)> = vec![
                ("/version".to_string(), vec![]),
                ("/nodes".to_string(), vec![]),
                (net, vec![]),
                (st, vec![]),
                ("/cluster/resources".to_string(), vec!["--type", "vm"]),
            ];
            for (path, extra) in jobs {
                // 每条之间让出一点时间：这是背景活，不该和用户刚发起的
                // 请求抢 `pvesh`。
                tokio::time::sleep(Duration::from_millis(300)).await;
                let cancel = CancellationToken::new();
                let _ = me.fetch_value(&path, &extra, &cancel).await;
            }
        });
    }

    /// 跑一条会改变状态的命令。**非零退出码一律当失败。**
    ///
    /// 这是整个命令层唯一一处能保证"命令真的成功了"的地方。之前这里把
    /// `Output` 原样返回、指望调用方自己看 `code`，但实际上没有一个调用方看——
    /// 结果 `pct create` 因存储满失败时会被记成成功，run 报"全部完成"，
    /// 而容器根本不存在。这类错误在真机上极难排查，所以必须在这里拦死。
    ///
    /// 需要容忍非零的调用（探测类）留在 `get_json` 和上面的只读方法里，
    /// 它们本来就有各自的语义。
    async fn cmd(
        &self,
        key: &str,
        spec: CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<Output, PveError> {
        let display = spec.brief();
        let out = self.runner.run(key, &spec, None, cancel, true).await?;
        match out.code {
            Some(0) => Ok(out),
            Some(code) => Err(RunError::NonZero {
                cmd: display,
                code: Some(code),
            }
            .into()),
            // 没有退出码 = 被信号杀掉。runner 里的超时/取消已经转成错误了，
            // 走到这里说明是别的信号，同样不能当成功。
            None => Err(RunError::NonZero {
                cmd: display,
                code: None,
            }
            .into()),
        }
    }

    // ───────────────────────── 只读探测 ─────────────────────────

    pub async fn version(&self, cancel: &CancellationToken) -> Result<Version, PveError> {
        self.get_json("/version", &[], cancel).await
    }

    /// `version` 的展示版：过期先给旧值。只给体检页用。
    pub async fn version_view(&self, cancel: &CancellationToken) -> Result<Version, PveError> {
        self.get_json_view("/version", &[], cancel).await
    }

    /// 严格的版本门槛：低于 9.0 直接拒绝（不做 8.x 兼容层）。
    pub async fn require_pve9(&self, cancel: &CancellationToken) -> Result<Version, PveError> {
        let v = self.version(cancel).await?;
        let major: u32 = v
            .version
            .split('.')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if major < 9 {
            return Err(PveError::TooOld(v.version.clone()));
        }
        Ok(v)
    }

    /// 一次拿全部 VM/CT 状态。**所有状态查询都必须走这里**，不要 N+1。
    pub async fn cluster_vms(&self, cancel: &CancellationToken) -> Result<Vec<ClusterResource>, PveError> {
        self.get_json("/cluster/resources", &["--type", "vm"], cancel)
            .await
    }

    /// `cluster_vms` 的展示版：过期先给旧值，后台补新。**执行器别用**。
    pub async fn cluster_vms_view(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<ClusterResource>, PveError> {
        self.get_json_view("/cluster/resources", &["--type", "vm"], cancel)
            .await
    }

    /// 节点名列表。只有体检页用，所以走"过期先给旧值"的读法。
    pub async fn nodes(&self, cancel: &CancellationToken) -> Result<Vec<String>, PveError> {
        #[derive(Deserialize)]
        struct N {
            #[serde(default)]
            node: String,
        }
        let v: Vec<N> = self.get_json_view("/nodes", &[], cancel).await?;
        Ok(v.into_iter().map(|n| n.node).collect())
    }

    /// PVE 配置视图里的桥。**只用于 doctor 展示**——存在性判定别用它：
    /// `ip link add type bridge` 建出来的桥在这里 `type` 是 `unknown`
    /// （PVE 只认 /etc/network/interfaces 里写全参数的段），真机实测
    /// 用它判存在，桥每轮都被误判"已消失"而重跑。
    pub async fn bridges(&self, cancel: &CancellationToken) -> Result<Vec<Iface>, PveError> {
        let v: Vec<Iface> = self
            .get_json(&format!("/nodes/{}/network", self.node), &[], cancel)
            .await?;
        Ok(v.into_iter().filter(|i| i.kind == "bridge").collect())
    }

    /// `bridges` 的展示版：过期先给旧值。只给体检页用。
    pub async fn bridges_view(&self, cancel: &CancellationToken) -> Result<Vec<Iface>, PveError> {
        let v: Vec<Iface> = self
            .get_json_view(&format!("/nodes/{}/network", self.node), &[], cancel)
            .await?;
        Ok(v.into_iter().filter(|i| i.kind == "bridge").collect())
    }

    /// `/etc/network/interfaces` 里持久化的网桥名。
    ///
    /// 只删内核接口是不够的：PVE 重启（或 `ifreload -a`）会照这个文件把桥重建
    /// 出来。真机踩过——`labmain` 用 `ip link del` 删掉，重启后又回来了，
    /// 因为文件里还留着 `auto labmain` 段。所以"有没有残留"要连这张表一起看，
    /// 否则界面上看不见、清也清不掉。
    pub async fn persisted_bridges(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, PveError> {
        let out = self
            .runner
            .run(
                "probe:ifaces",
                &CommandSpec::new(
                    "sed",
                    vec![
                        "-n".to_string(),
                        r"s/^iface \([^ ]*\) inet .*/\1/p".to_string(),
                        "/etc/network/interfaces".to_string(),
                    ],
                ),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// 每个桥的当前用法：地址、挂了多少成员口。
    ///
    /// 用来回答"这个桥还有人在用吗"——规格删了但桥没删，内核里照样躺着，
    /// 占着名字和地址。只看名字无法判断，必须同时看 IP 与成员。
    pub async fn bridge_usage(&self, cancel: &CancellationToken) -> Result<Vec<(String, String, usize)>, PveError> {
        let script = r#"for b in $(ip -o link show type bridge | awk -F': ' '{print $2}'); do
  ip=$(ip -4 -o addr show "$b" 2>/dev/null | awk '{print $4}' | paste -sd, -)
  n=$(ip -o link show master "$b" 2>/dev/null | wc -l)
  printf '%s|%s|%s\n' "$b" "$ip" "$n"
done"#;
        let out = self
            .runner
            .run(
                "probe:bridge-usage",
                &CommandSpec::new("sh", vec!["-c", script]),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| {
                let mut it = l.trim().split('|');
                let name = it.next()?.to_string();
                let ips = it.next().unwrap_or("").to_string();
                let n = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(0);
                Some((name, ips, n))
            })
            .collect())
    }

    /// 内核实际生效面：`ip -o link show type bridge`。桥的本质是内核接口，
    /// /etc/network/interfaces 只是持久化——"存在不存在"看这里才不会说谎。
    pub async fn kernel_bridges(&self, cancel: &CancellationToken) -> Result<Vec<String>, PveError> {
        let out = self
            .runner
            .run(
                "probe:bridges",
                &CommandSpec::new("ip", vec!["-o", "link", "show", "type", "bridge"]),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| {
                // `4: lablab: <BROADCAST,...>` → 取第 2 段去冒号
                l.split(':').nth(1).map(|s| s.trim().to_string())
            })
            .collect())
    }

    pub async fn storages(&self, cancel: &CancellationToken) -> Result<Vec<Storage>, PveError> {
        self.get_json(&format!("/nodes/{}/storage", self.node), &[], cancel)
            .await
    }

    /// `storages` 的展示版：过期先给旧值。只给体检页用。
    pub async fn storages_view(&self, cancel: &CancellationToken) -> Result<Vec<Storage>, PveError> {
        self.get_json_view(&format!("/nodes/{}/storage", self.node), &[], cancel)
            .await
    }

    pub async fn aplinfo(&self, cancel: &CancellationToken) -> Result<Vec<AplTemplate>, PveError> {
        self.get_json(&format!("/nodes/{}/aplinfo", self.node), &[], cancel)
            .await
    }

    /// 某存储上的模板文件（`vztmpl`）。
    pub async fn templates_on(
        &self,
        storage: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, PveError> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct C {
            volid: String,
            format: String,
        }
        let v: Vec<C> = self
            .get_json(
                &format!("/nodes/{}/storage/{}/content", self.node, storage),
                &["--content", "vztmpl"],
                cancel,
            )
            .await?;
        Ok(v.into_iter()
            .filter(|c| c.format == "tzst" || c.format == "tgz" || c.volid.contains("vztmpl"))
            .map(|c| c.volid)
            .collect())
    }

    /// 单个 CT 的配置。**只在需要细节时调用，且结果应缓存 10 秒**。
    // ───────────────────────── 写操作 ─────────────────────────

    /// 建桥：`ip link add ... type bridge` + 持久化到 `/etc/network/interfaces`。
    /// 幂等：已存在则跳过。
    pub async fn ensure_bridge(
        &self,
        name: &str,
        mgmt_ip: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let script = format!(
            r#"set -e
if ip link show {name} >/dev/null 2>&1; then
  echo "bridge {name} already exists"
else
  ip link add name {name} type bridge
  ip link set {name} up
  echo "created {name}"
fi
# 宿主管理地址：Web 终端与调试从宿主 L2 直达容器，不经网关转发，所以
# 网关的入向丢弃规则对这些流量不生效（isolation 语义不变。mirr 是纯二层
# 桥，mgmt 为空时跳过）。replace 保证漂移自愈。
if [ -n "{mgmt_ip}" ]; then
  ip addr replace {mgmt_ip} dev {name}
fi
# 持久化（PVE 9 用 /etc/network/interfaces，追加式，重复执行不重复写）。
# 管理地址不能只写在内核里：宿主重启或 networking 刷新一次就没了，而
# bridge 步骤因为"已完成且参数未变"不会再跑，症状是"终端 502、宿主
# ping 不通容器"。所以地址必须落成接口块里的 `up` 行，开机即恢复。
if ! grep -q "^iface {name} " /etc/network/interfaces 2>/dev/null; then
  if [ -n "{mgmt_ip}" ]; then
    cat >> /etc/network/interfaces <<EOF

auto {name}
iface {name} inet manual
	bridge-ports
	bridge-stp off
	bridge-fd 0
	up ip addr replace {mgmt_ip} dev {name}
EOF
  else
    cat >> /etc/network/interfaces <<EOF

auto {name}
iface {name} inet manual
	bridge-ports
	bridge-stp off
	bridge-fd 0
EOF
  fi
  echo "persisted {name}"
elif [ -n "{mgmt_ip}" ] && ! grep -q "up ip addr replace {mgmt_ip} dev {name}" /etc/network/interfaces; then
  # 老格式迁移：块已存在但没有地址恢复行——插到块内 bridge-fd 之后。
  # 用 awk 而不是 sed：网段里有 /（10.77.0.254/24），sed 的 s/// 分隔符
  # 会被它打断；awk 的替换文本走 -v 变量，没有分隔符问题。
  awk -v up="\tup ip addr replace {mgmt_ip} dev {name}" '
    !done && $0 == "iface {name} inet manual" {{ blk = 1; print; next }}
    blk && $0 == "\tbridge-fd 0" {{ print; print up; blk = 0; done = 1; next }}
    {{ print }}
  ' /etc/network/interfaces > /etc/network/interfaces.labyard.tmp \
    && mv /etc/network/interfaces.labyard.tmp /etc/network/interfaces
  echo "migrated {name} mgmt address"
fi
"#
        );
        self.cmd(
            "bridge",
            CommandSpec::new("sh", ["-c", &script]).with_timeout(Duration::from_secs(30)),
            cancel,
        )
        .await?;
        Ok(())
    }

    /// 下载模板（`pveam download`），已存在则跳过。
    pub async fn ensure_template(
        &self,
        storage: &str,
        template: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let existing = self.templates_on(storage, cancel).await?;
        if existing.iter().any(|v| v.contains(template)) {
            return Ok(());
        }
        // 模板名可能是 `debian-13-standard` 这种简写，需要先查可用列表拿完整名
        let list = self.aplinfo(cancel).await?;
        let full = list
            .iter()
            .find(|t| t.template.contains(template))
            .map(|t| t.template.clone())
            .unwrap_or_else(|| template.to_string());
        self.cmd(
            "template-download",
            CommandSpec::new(
                "pveam",
                ["download", storage, &full],
            )
            .with_timeout(Duration::from_secs(1800)),
            cancel,
        )
        .await?;
        Ok(())
    }

    /// 数据盘收敛：把规格里的 `disks` 落成 mp0、mp1……
    ///
    /// 规则很克制，因为一步做错就是删数据：
    /// - 挂载点一致、容量够 → 什么都不做（幂等）
    /// - 挂载点一致、容量变大 → 扩容（PVE 允许在线扩 mp 卷）
    /// - 挂载点不存在 → 新建
    /// - 同序号但挂载点不同 → **报错**，不覆盖。这种多半是规格里插了一行
    ///   导致序号错位，静默覆盖等于把 A 盘的数据挂到 B 目录上。
    ///
    /// 不做缩容、不做删除：那两件事必须由人来决定（数据没了就真没了）。
    pub async fn ensure_disks(
        &self,
        vmid: u32,
        disks: &[crate::core::spec::ResolvedDisk],
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        if disks.is_empty() {
            return Ok(());
        }
        let out = self
            .runner
            .run(
                &format!("ct-config:{vmid}"),
                &CommandSpec::new("pct", vec!["config".into(), vmid.to_string()])
                    .with_timeout(Duration::from_secs(20)),
                None,
                cancel,
                true,
            )
            .await?;

        // `mp0: local-lvm:vm-102-disk-1,mp=/data,size=2G`
        let mut cur: std::collections::HashMap<u32, (String, Option<u64>, String)> =
            std::collections::HashMap::new();
        for line in out.stdout.lines() {
            if let Some((idx, mount, size, volid)) = parse_mp_line(line) {
                cur.insert(idx, (mount, size, volid));
            }
        }

        for (i, d) in disks.iter().enumerate() {
            let idx = i as u32;
            let want = d.size as u64;
            match cur.get(&idx) {
                Some((mount, size, _)) if mount == &d.mount => {
                    let have = size.unwrap_or(0);
                    if have >= want {
                        continue; // 幂等：已经够大
                    }
                    // 扩容要用 `pct resize`，不能用 `pct set -mpX volid,size=`：
                    // 后者只把 LVM 卷撑大，容器里的 ext4 还是旧容量，`df` 一点没变；
                    // 而未特权容器里拿不到 /dev/mapper 的块设备节点，进容器
                    // `resize2fs` 会 "No such file or directory"。
                    // `pct resize` 在宿主侧连卷带文件系统一起扩，真机实测一步到位。
                    self.cmd(
                        &format!("ct-mp{idx}:grow"),
                        CommandSpec::new(
                            "pct",
                            vec![
                                "resize".into(),
                                vmid.to_string(),
                                format!("mp{idx}"),
                                format!("{want}G"),
                            ],
                        )
                        .with_timeout(Duration::from_secs(300)),
                        cancel,
                    )
                    .await?;
                }
                Some((mount, _, _)) => {
                    return Err(PveError::Bad(
                        format!(
                            "CT {vmid} 的 mp{idx} 已经挂在 `{mount}`，规格里却要求 `{}`。\
                             为了不把数据挂错地方，这里不覆盖——请先把规格里的数据盘顺序调整回去，\
                             或手工 `pct set {vmid} -mp{idx} ...` 处理掉旧挂载点。",
                            d.mount
                        ),
                    ));
                }
                None => {
                    self.cmd(
                        &format!("ct-mp{idx}:add"),
                        CommandSpec::new(
                            "pct",
                            vec![
                                "set".into(),
                                vmid.to_string(),
                                format!("-mp{idx}"),
                                format!("{}:{},mp={}", d.storage, want, d.mount),
                            ],
                        )
                        .with_timeout(Duration::from_secs(180)),
                        cancel,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// 容器现有的网卡配置（`net0` → `name=eth0,bridge=lablab,…`）。
    ///
    /// 两处要用：
    /// 1. 网卡列表变短了（监控机不再抓某个网络）→ 多出来的 `netN` 必须删掉，
    ///    否则容器里留着一块谁都不认的口，镜像脚本还在往上灌流量。
    /// 2. 判断"这次到底有没有变"——见 `ensure_ct`：不变就不下发 `--netN`，
    ///    因为对运行中的容器改网卡是**热插拔**，会把容器里的 eth 换成新接口。
    pub async fn ct_net_specs(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<(u32, String)>, PveError> {
        let out = self
            .runner
            .run(
                &format!("ct-config:{vmid}"),
                &CommandSpec::new("pct", vec!["config".into(), vmid.to_string()])
                    .with_timeout(Duration::from_secs(20)),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| {
                // `net0: name=eth0,bridge=lablab,hwaddr=...`
                let l = l.trim_start();
                let rest = l.strip_prefix("net")?;
                let (idx, val) = rest.split_once(':')?;
                Some((idx.parse::<u32>().ok()?, val.trim().to_string()))
            })
            .collect())
    }

    /// 容器里**实际存在**的 eth* 网口数量（读 /sys/class/net，不问配置）。
    ///
    /// 用来发现"配置改了但 netns 没重建"——`pct set --netN` 对运行中的容器
    /// 不生效，这是最容易被忽略的一条：`pct config` 看着对，`ip addr` 里没有。
    /// 查询失败返回 0，调用方据此**不做**重启（不能因为一次 exec 失败就重启用户容器）。
    pub async fn ct_running_nics(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<usize, PveError> {
        let out = self
            .runner
            .run(
                &format!("ct-nics:{vmid}"),
                &CommandSpec::new(
                    "pct",
                    vec![
                        "exec".into(),
                        vmid.to_string(),
                        "--".into(),
                        "sh".into(),
                        "-c".into(),
                        "ls /sys/class/net | grep -c '^eth'".into(),
                    ],
                )
                .with_timeout(Duration::from_secs(15)),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out.stdout.trim().parse::<usize>().unwrap_or(0))
    }

    /// 建 CT。已存在则改为 `pct set` 更新配置（幂等的关键）。
    ///
    /// 三件事必须在这里做掉，否则后面的步骤全是空转：
    /// 1. `--start 1`——`pct create` 默认建出来是**停的**，而下一步就是 `pct exec`
    /// 2. 网关要有第二块网卡 `eth1` 上联，否则网关脚本里的 `ip link set eth1 up` 直接失败
    /// 3. 内网口用 `ip=manual`——内网桥上没有 DHCP 服务，写 `ip=dhcp` 只会白白等超时，
    ///    地址由后面的 net 步骤静态配
    ///
    /// 网卡布局（顺序即 eth 编号）：
    /// - `eth0`：所在 zone 的桥（外网 zone 是 wan 桥，且 `ip=dhcp`）
    /// - `eth1`：只有网关有，上联网桥
    /// - `eth1..N`：监控机的抓包口，一个被监控网络一块（与解析层给的
    ///   `ResolvedMirror::to_iface` 严格同序，测试里有断言）
    #[allow(clippy::too_many_arguments)]
    pub async fn ensure_ct(
        &self,
        vmid: u32,
        hostname: &str,
        cores: u32,
        memory: u32,
        disk: u32,
        storage: &str,
        template_vol: &str,
        bridge: &str,
        uplink_bridge: Option<&str>,
        uplink_ip: Option<&str>,
        capture_bridges: &[String],
        dhcp: bool,
        password: Option<&str>,
        ssh_keys: &[String],
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let existing = self
            .cluster_vms(cancel)
            .await?
            .into_iter()
            .find(|r| r.is_lxc() && r.vmid_parsed() == Some(vmid));

        // 期望的网卡列表：顺序即 eth 编号，建机与改机共用一份，避免两条路径
        // 各写一遍而慢慢漂开。
        let mut nets: Vec<(String, String, String)> = Vec::new();
        let mgmt_ip = if dhcp { "dhcp" } else { "manual" };
        nets.push(("eth0".into(), bridge.to_string(), mgmt_ip.to_string()));
        if let Some(up) = uplink_bridge {
            // 嵌套 zone 的上联桥没有 DHCP 服务，必须给静态地址；
            // 只有上联是真实外网桥时才走 dhcp。
            nets.push(("eth1".into(), up.to_string(), uplink_ip.unwrap_or("dhcp").to_string()));
        }
        for b in capture_bridges {
            nets.push((
                format!("eth{}", nets.len()),
                b.clone(),
                "manual".to_string(),
            ));
        }

        match existing {
            None => {
                let mut args: Vec<String> = vec![
                    "create".into(),
                    vmid.to_string(),
                    template_vol.into(),
                    "--hostname".into(),
                    hostname.into(),
                    "--cores".into(),
                    cores.to_string(),
                    "--memory".into(),
                    memory.to_string(),
                    "--rootfs".into(),
                    format!("{storage}:{disk}"),
                    "--unprivileged".into(),
                    "1".into(),
                    "--features".into(),
                    "nesting=1".into(),
                    "--onboot".into(),
                    "1".into(),
                    "--start".into(),
                    "1".into(),
                ];
                for (i, (name, br, ip)) in nets.iter().enumerate() {
                    args.push(format!("--net{i}"));
                    args.push(format!("name={name},bridge={br},ip={ip},type=veth"));
                }
                if let Some(p) = password {
                    args.push("--password".into());
                    args.push(p.into());
                }
                if !ssh_keys.is_empty() {
                    args.push("--ssh-public-keys".into());
                    args.push(ssh_keys.join("\n"));
                }
                self.cmd(
                    "ct-create",
                    CommandSpec::new("pct", args).with_timeout(Duration::from_secs(600)),
                    cancel,
                )
                .await?;
            }
            Some(row) => {
                // 运行时网口数：改 netN 会热插拔容器网卡，改完要核对口数对不对。
                let runtime_nics = self.ct_running_nics(vmid, cancel).await.unwrap_or(0);
                let have = self.ct_net_specs(vmid, cancel).await.unwrap_or_default();
                // 已存在：只更新会漂移的字段，一条命令合并，避免 N 次 pct set 打 pmxcfs
                let mut args: Vec<String> = vec!["set".into(), vmid.to_string()];
                args.extend(["--cores".into(), cores.to_string()]);
                args.extend(["--memory".into(), memory.to_string()]);
                args.extend(["--hostname".into(), hostname.into()]);
                // 网卡只在**真的变了**的时候才下发。对运行中的容器改 netN 是
                // 热插拔：容器里的 eth 会被换成新接口，原有地址与路由当场消失。
                // 以前这里无条件下发，于是"什么都没改的一次 apply"也能把网关
                // 配瘫——而 net 步骤状态还是已完成，没人会去补地址。
                let mut net_changed = false;
                for (i, (name, br, ip)) in nets.iter().enumerate() {
                    let same = have
                        .iter()
                        .find(|(n, _)| *n == i as u32)
                        .map(|(n, cur)| net_spec_matches(cur, *n, name, br, ip))
                        .unwrap_or(false);
                    if same {
                        continue;
                    }
                    net_changed = true;
                    args.push(format!("--net{i}"));
                    args.push(format!("name={name},bridge={br},ip={ip},type=veth"));
                }
                // 多出来的网卡删掉。改短（监控机少抓一个网络）时不做这一步，
                // 容器里会留一块仍然挂着旧抓包桥的口，而平台上已经看不见它。
                for (i, _) in have.iter().filter(|(i, _)| (*i as usize) >= nets.len()) {
                    net_changed = true;
                    args.push("--delete".into());
                    args.push(format!("net{i}"));
                }
                self.cmd(
                    "ct-update",
                    CommandSpec::new("pct", args).with_timeout(Duration::from_secs(120)),
                    cancel,
                )
                .await?;

                // 停着的容器必须拉起来。`--start` 只对新建生效，
                // 手工 `pct stop` 过的容器如果这里不管，后面 `pct exec` 会全部失败。
                if row.status.as_deref() != Some("running") {
                    self.ct_action(vmid, "start", cancel).await?;
                } else if runtime_nics > 0 && runtime_nics != nets.len() {
                    // 网口数量对不上：配置里有的口容器里没有（或反过来）。
                    // 重启一次让 netns 按新配置重建；开机时 labyard-net.service
                    // 会把地址重放回来。
                    self.ct_action(vmid, "reboot", cancel).await?;
                } else if net_changed {
                    // 口数没变但配置变了：热插拔已经发生，容器里的地址没了。
                    // 开机脚本不会再跑，只能立刻重放一次——否则这台容器从此
                    // "配置看着对，就是连不上"。
                    let _ = self.ct_replay_net(vmid, cancel).await;
                }
            }
        }
        Ok(())
    }

    /// 重放容器内已写好的地址配置——就是开机时 `labyard-net.service` 跑的那个脚本。
    ///
    /// 用在改过 `netN` 之后：热插拔把 eth 换成了新接口，地址随之消失，而开机
    /// 脚本只在上电时跑一次。脚本由 net 步骤落盘，还没写（首次 apply）时这里
    /// 什么都不做，不会报错。
    pub async fn ct_replay_net(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let script = "if [ -r /usr/local/lib/labyard/labyard-net.sh ]; then \
                      sh /usr/local/lib/labyard/labyard-net.sh; fi";
        self.ct_exec(
            "ct-net:replay",
            vmid,
            &["sh", "-c", script],
            Duration::from_secs(60),
            cancel,
        )
        .await?;
        Ok(())
    }

    /// 容器内执行。返回完整输出。
    /// 容器起来了但 DHCP 没拿到地址时补一脚。
    ///
    /// 真机现象：外网监控机的 eth0 是 `ip=dhcp`，容器里的 `ifup` 在 veth 就绪
    /// 之前就跑过一遍，之后没人再试——容器一直只有 `lo`，面板上写着 `dhcp`
    /// 却连 `ping` 都发不出去（`Network is unreachable`）。这里对仍然没有 v4
    /// 地址的口补一次 `dhclient -1`；没有该命令就跳过，不把容器弄失败。
    pub async fn ct_ensure_dhcp(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let script = r#"for i in $(ls /sys/class/net | grep -E '^eth'); do ip -4 -o addr show "$i" 2>/dev/null | grep -q "inet " || { command -v dhclient >/dev/null 2>&1 && dhclient -1 -q "$i" 2>/dev/null; }; done"#;
        self.ct_exec(
            "dhcp:ensure",
            vmid,
            &["sh", "-c", script],
            Duration::from_secs(25),
            cancel,
        )
        .await?;
        Ok(())
    }

    pub async fn ct_exec(
        &self,
        key: &str,
        vmid: u32,
        argv: &[&str],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Output, PveError> {
        let mut args: Vec<String> = vec!["exec".into(), vmid.to_string(), "--".into()];
        args.extend(argv.iter().map(|s| s.to_string()));
        // 串行闸：排队等令牌。等闸期间也响应取消，避免 stop 后还挂着队列。
        let permit = tokio::select! {
            p = self.exec_gate.clone().acquire_owned() => p,
            _ = cancel.cancelled() => {
                return Err(PveError::Cancelled { cmd: format!("pct exec {vmid}") });
            }
        };
        let out = self
            .cmd(
                key,
                CommandSpec::new("pct", args).with_timeout(timeout),
                cancel,
            )
            .await;
        drop(permit);
        out
    }

    /// 把一段脚本 push 进容器并执行。
    pub async fn push_and_run(
        &self,
        key: &str,
        vmid: u32,
        script: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Output, PveError> {
        // 用 base64 传输，避免引号/换行在多层转义里出错。
        // 临时文件名带 key：同一台容器上并发跑两条脚本（gateway + vpn）时，
        // 固定路径会让后一条覆盖前一条，症状是"脚本内容对但行为不对"。
        let b64 = base64_encode(script.as_bytes());
        let safe: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .take(40)
            .collect();
        let file = format!("/tmp/labyard-step-{safe}.sh");
        let inner = format!(
            "set -e; echo {b64} | base64 -d > {file}; chmod +x {file}; {file}; rc=$?; rm -f {file}; exit $rc"
        );
        self.ct_exec(key, vmid, &["sh", "-c", &inner], timeout, cancel)
            .await
    }

    /// 把一段脚本装成容器内的开机 oneshot 单元，并立刻执行一次。
    ///
    /// 为什么必须落成单元：容器一重启（retire→revive、`pct reboot`、宿主
    /// 重启），`ip addr` 配的地址和 nft 的规则表**全部消失**，而 net /
    /// gateway 这两步是哈希门控的——apply 认为"已经做过"直接跳过。结果是
    /// 重启后整个 zone 静默失联（地址没了），或者网关静默 fail-open
    /// （规则表没了 = 默认放行，平台还显示"策略已下发"）。
    ///
    /// 落成单元之后，重启由 systemd 自愈；单元里的脚本与本步骤共用同一份
    /// 生成器输出，不会出现"开机跑的是旧规则"。
    ///
    /// `unit` 不含 `.service` 后缀，形如 `labyard-net`。
    pub async fn ct_boot_script(
        &self,
        key: &str,
        vmid: u32,
        unit: &str,
        description: &str,
        script: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Output, PveError> {
        let b64 = base64_encode(script.as_bytes());
        // ExecStart 显式走 `/bin/sh`：脚本是生成器吐出的纯命令片段，没有
        // shebang，直接当可执行文件起会 `Exec format error`（真机踩过：
        // 单元 installed+enabled 但一启动就 203/EXEC，容器里连地址都没有）。
        let unit_body = format!(
            "[Unit]\n\
             Description={description}\n\
             After=network-pre.target\n\
             Before=network.target\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             RemainAfterExit=yes\n\
             ExecStart=/bin/sh /usr/local/lib/labyard/{unit}.sh\n\
             \n\
             [Install]\n\
             WantedBy=multi-user.target\n"
        );
        let b64_unit = base64_encode(unit_body.as_bytes());
        // 先写临时文件再 mv：写到一半被打断时不会留下半截脚本，而开机时
        // systemd 恰好读到它就是"容器起不来 + 没有任何提示"。
        let inner = format!(
            "set -e\n\
             install -d /usr/local/lib/labyard\n\
             echo {b64} | base64 -d > /usr/local/lib/labyard/{unit}.sh.new\n\
             chmod 0755 /usr/local/lib/labyard/{unit}.sh.new\n\
             mv /usr/local/lib/labyard/{unit}.sh.new /usr/local/lib/labyard/{unit}.sh\n\
             echo {b64_unit} | base64 -d > /etc/systemd/system/{unit}.service.new\n\
             mv /etc/systemd/system/{unit}.service.new /etc/systemd/system/{unit}.service\n\
             systemctl daemon-reload\n\
             systemctl enable {unit}.service >/dev/null 2>&1 || true\n\
             systemctl restart {unit}.service"
        );
        self.ct_exec(key, vmid, &["sh", "-c", &inner], timeout, cancel)
            .await
    }

    /// 读容器内文件（VPN 客户端配置、证书等）。读不到就是错误——调用方
    /// 要返回给用户的是"文件不存在"，而不是一份空的下载。
    pub async fn ct_read(
        &self,
        vmid: u32,
        path: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PveError> {
        let out = self
            .ct_exec(
                &format!("read:{vmid}"),
                vmid,
                &["cat", path],
                Duration::from_secs(20),
                cancel,
            )
            .await?;
        Ok(out.stdout)
    }

    /// 确保容器有 `/dev/net/tun`（VPN 必需）。返回 true 表示这次新加了设备。
    ///
    /// PVE 的 `dev0: /dev/net/tun` 会同时写 cgroup 白名单与 bind mount，
    /// 两者都只在容器**启动时**生效——所以新加设备必须重启容器，否则
    /// 容器里永远看不到 /dev/net/tun，而 `pct set` 自己不会告诉你这件事。
    pub async fn ensure_tun(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<bool, PveError> {
        let out = self
            .runner
            .run(
                &format!("tun-check:{vmid}"),
                &CommandSpec::new("pct", vec!["config".into(), vmid.to_string()])
                    .with_timeout(Duration::from_secs(20)),
                None,
                cancel,
                true,
            )
            .await?;
        let has = out
            .stdout
            .lines()
            .any(|l| l.trim_start().starts_with("dev0:") && l.contains("/dev/net/tun"));
        if has {
            return Ok(false);
        }
        self.cmd(
            "tun-add",
            CommandSpec::new(
                "pct",
                vec![
                    "set".into(),
                    vmid.to_string(),
                    "--dev0".into(),
                    "/dev/net/tun".into(),
                ],
            )
            .with_timeout(Duration::from_secs(60)),
            cancel,
        )
        .await?;
        // 重启让设备生效。停着的容器直接 start（reboot 对停着的容器会报错）
        match self.ct_status(vmid, cancel).await? {
            Some(st) if st == "running" => self.ct_action(vmid, "reboot", cancel).await?,
            _ => self.ct_action(vmid, "start", cancel).await?,
        }
        Ok(true)
    }

    /// 起/停/删。
    pub async fn ct_action(
        &self,
        vmid: u32,
        action: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let args = match action {
            "destroy" => vec!["destroy".to_string(), vmid.to_string(), "--purge".into()],
            other => vec![other.to_string(), vmid.to_string()],
        };
        self.cmd(
            &format!("ct-{action}"),
            CommandSpec::new("pct", args).with_timeout(Duration::from_secs(300)),
            cancel,
        )
        .await?;
        Ok(())
    }

    /// 往容器里装 ttyd 并起 systemd 单元（Web 终端的数据面）。
    ///
    /// 二进制从宿主 `/usr/local/bin/ttyd` 推入——它是静态链接的单文件，
    /// 不依赖容器内任何库。宿主没有这个文件时返回 Ok(false)，由调用方
    /// 决定怎么说（终端是增强功能，不该炸掉基础配置）。
    ///
    /// base path 用 `/terminal/<zone>/<host>`：ttyd 生成页面里的绝对 URL
    /// 会带上这个前缀，正好与外层反代的路径一致，反代不需要改写任何内容。
    pub async fn ensure_terminal(
        &self,
        vmid: u32,
        zone: &str,
        host: &str,
        bind_ip: &str,
        cancel: &CancellationToken,
    ) -> Result<bool, PveError> {
        const HOST_TTYD: &str = "/usr/local/bin/ttyd";
        if !std::path::Path::new(HOST_TTYD).exists() {
            return Ok(false);
        }

        // 容器里没有才推（1.3 MB，每次 bootstrap 都推一遍是浪费）
        let has = self
            .runner
            .run(
                &format!("ttyd-check:{vmid}"),
                &CommandSpec::new("pct", vec!["exec".into(), vmid.to_string(), "--".into(), "test".into(), "-x".into(), "/usr/local/bin/ttyd".into()]),
                None,
                cancel,
                true,
            )
            .await?;
        if has.code != Some(0) {
            self.cmd(
                "ttyd-push",
                CommandSpec::new(
                    "pct",
                    vec![
                        "push".into(),
                        vmid.to_string(),
                        HOST_TTYD.into(),
                        "/usr/local/bin/ttyd".into(),
                        "--perms".into(),
                        "0755".into(),
                    ],
                )
                .with_timeout(Duration::from_secs(120)),
                cancel,
            )
            .await?;
        }

        let base = format!("/terminal/{zone}/{host}");
        let unit = format!(
            r#"[Unit]
Description=labyard Web terminal (ttyd)
After=network.target

[Service]
ExecStart=/usr/local/bin/ttyd -p 7681 -i {bind_ip} -b {base} -W -t fontSize=14 /bin/bash -l
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
"#
        );
        let b64 = base64_encode(unit.as_bytes());
        let script = format!(
            "set -e; echo {b64} | base64 -d > /etc/systemd/system/labyard-ttyd.service.new;              if cmp -s /etc/systemd/system/labyard-ttyd.service.new /etc/systemd/system/labyard-ttyd.service; then                rm -f /etc/systemd/system/labyard-ttyd.service.new; echo unchanged;              else                mv /etc/systemd/system/labyard-ttyd.service.new /etc/systemd/system/labyard-ttyd.service;                systemctl daemon-reload; systemctl enable --now labyard-ttyd;                systemctl restart labyard-ttyd; echo updated;              fi; systemctl is-active labyard-ttyd || true"
        );
        self.ct_exec(
            "ttyd-unit",
            vmid,
            &["sh", "-c", &script],
            Duration::from_secs(60),
            cancel,
        )
        .await?;
        Ok(true)
    }

    /// 所有**运行中**容器的 IPv4，一次拿全：`vmid → ["10.77.0.1/24", …]`。
    ///
    /// 用一条命令顺序 `pct exec`，而不是每台各起一次调用：apply 开始时只查
    /// 一遍，十几台容器也就一两秒。停着的容器不会出现在结果里——"没在跑"
    /// 和"地址丢了"是两种状态，不能混成一个空列表。
    ///
    /// 查出来作什么：判断网络步骤是否真的还成立。地址是运行时配的，容器重启
    /// 或网卡热插拔之后就没了，而步骤记录还写着 done——只信记录，平台会一直
    /// 以为网络是好的。
    pub async fn ct_addrs_all(
        &self,
        cancel: &CancellationToken,
    ) -> Result<std::collections::HashMap<u32, Vec<String>>, PveError> {
        let script = r#"for c in $(pct list | awk 'NR>1 && $2=="running" {print $1}'); do
  a=$(pct exec "$c" -- ip -4 -o addr show 2>/dev/null | awk '{print $4}' | paste -sd, -)
  printf '%s|%s\n' "$c" "$a"
done"#;
        let out = self
            .runner
            .run(
                "probe:ct-addrs",
                &CommandSpec::new("sh", vec!["-c", script]).with_timeout(Duration::from_secs(60)),
                None,
                cancel,
                true,
            )
            .await?;
        let mut map = std::collections::HashMap::new();
        for line in out.stdout.lines() {
            let Some((vmid, addrs)) = line.trim().split_once('|') else {
                continue;
            };
            let Ok(vmid) = vmid.trim().parse::<u32>() else {
                continue;
            };
            let list: Vec<String> = addrs
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            map.insert(vmid, list);
        }
        Ok(map)
    }

    /// 容器某块网卡的实际 IPv4（从容器内部读，而不是查 spec）。
    ///
    /// 为什么不从规格算：终端这类"操作现成环境"的功能不该依赖 Web 保存的
    /// 规格存在（CLI 建的环境同样要能开终端），也不该假设地址没被手工改过。
    pub async fn ct_ip(
        &self,
        vmid: u32,
        iface: &str,
        cancel: &CancellationToken,
    ) -> Result<String, PveError> {
        let out = self
            .runner
            .run(
                &format!("ct-ip:{vmid}"),
                &CommandSpec::new(
                    "pct",
                    vec![
                        "exec".into(),
                        vmid.to_string(),
                        "--".into(),
                        "ip".into(),
                        "-4".into(),
                        "-o".into(),
                        "addr".into(),
                        "show".into(),
                        iface.into(),
                    ],
                )
                .with_timeout(Duration::from_secs(20)),
                None,
                cancel,
                true,
            )
            .await?;
        // 输出形如 `2: eth0    inet 10.77.0.20/24 brd ...`：
        // 找 "inet" 的下一个 token，切掉前缀
        out.stdout
            .split_whitespace()
            .skip_while(|t| *t != "inet")
            .nth(1)
            .map(|cidr| cidr.split('/').next().unwrap_or(cidr).to_string())
            .ok_or_else(|| {
                // 命令本身是成功的（退出码 0），只是网卡上没有 IPv4。以前这里
                // 伪造一个 `code: None` 的 NonZero，界面显示"退出码 None"——
                // 等于让人去查一个不存在的失败。真正该说的是：地址没配上。
                PveError::Bad(format!(
                    "容器 {vmid} 的 {iface} 上没有 IPv4 地址：地址可能还没配好\
                     （刚重启的容器需要等网络配置重放），确认容器已启动后重试"
                ))
            })
    }

    /// 容器快照列表（PVE 的 `current` 是活动节点不是快照，滤掉）。
    pub async fn snapshots_of(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, PveError> {
        #[derive(Deserialize)]
        struct Snap {
            #[serde(default)]
            name: String,
        }
        let snaps: Vec<Snap> = self
            .get_json(
                &format!("/nodes/{}/lxc/{vmid}/snapshot", self.node),
                &[],
                cancel,
            )
            .await
            .unwrap_or_default();
        Ok(snaps
            .into_iter()
            .map(|s| s.name)
            .filter(|n| n != "current" && !n.is_empty())
            .collect())
    }

    /// 回滚到快照并重新启动（pct rollback 会把容器停下——恢复到快照点
    /// 而不重启的话，用户看到的还是"容器停了"，像坏了）。
    pub async fn rollback_snapshot(
        &self,
        vmid: u32,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        self.cmd(
            "snapshot-rollback",
            CommandSpec::new(
                "pct",
                [
                    "rollback".to_string(),
                    vmid.to_string(),
                    name.to_string(),
                ],
            )
            .with_timeout(Duration::from_secs(600)),
            cancel,
        )
        .await?;
        // 快照点如果是 running 打的，回滚后 PVE 保持 stopped——起来它
        let _ = self.ct_action(vmid, "start", cancel).await;
        Ok(())
    }

    /// 删除快照。
    pub async fn del_snapshot(
        &self,
        vmid: u32,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        self.cmd(
            "snapshot-delete",
            CommandSpec::new(
                "pct",
                [
                    "delsnapshot".to_string(),
                    vmid.to_string(),
                    name.to_string(),
                ],
            )
            .with_timeout(Duration::from_secs(300)),
            cancel,
        )
        .await?;
        Ok(())
    }

    /// 容器状态：Some("running"/"stopped"/...) 或 None（不存在）。
    /// destroy 用它决定要不要先 stop，不能对着停着的容器 pct stop——
    /// 那会退出非零，把"本来就停着"当成失败。
    pub async fn ct_status(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, PveError> {
        let out = self
            .runner
            .run(
                &format!("status:{vmid}"),
                &CommandSpec::new("pct", vec!["status".into(), vmid.to_string()]),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .find_map(|l| l.strip_prefix("status: "))
            .map(|s| s.trim().to_string()))
    }

    /// 删网桥 + 清掉 /etc/network/interfaces 里持久化的段。幂等：桥不存在
    /// 只清配置段。清理残留网桥也走这里——只 `ip link del` 的话，宿主重启
    /// 或 `ifreload -a` 会把桥从配置里重建回来，症状是"删了又出现"。
    ///
    /// **刻意不调 `ifreload -a`**：它会把全机接口重载一遍，包括管理口 vmbr0；
    /// 挂在被重载桥上的容器口会短暂失链，容器里的 dhclient 随即把租约丢掉
    /// （真机见过：清桥之后网关容器 eth0/eth1 地址全空、整片实验网断掉）。
    /// 内核已 `ip link del`、配置段已删，没有任何需要 reload 的东西。
    pub async fn destroy_bridge(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let script = format!(
            r#"set -e
if ip link show {name} >/dev/null 2>&1; then
  ip link del {name} || true
  echo "bridge {name} deleted"
else
  echo "bridge {name} absent"
fi
# 删持久化段（auto <name> 起到段尾）。只删接口不删配置的话，重启会重建。
if grep -q "^auto {name}$" /etc/network/interfaces 2>/dev/null; then
  awk -v n="{name}" '{awk}' /etc/network/interfaces > /etc/network/interfaces.labyard.tmp
  cat /etc/network/interfaces.labyard.tmp > /etc/network/interfaces
  rm -f /etc/network/interfaces.labyard.tmp
  echo "unpersisted {name}"
fi
echo done"#,
            awk = BRIDGE_UNPERSIST_AWK
        );
        self.runner
            .run(
                &format!("bridge-destroy:{name}"),
                &CommandSpec::new("sh", vec!["-c".into(), script])
                    .with_timeout(Duration::from_secs(30)),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(())
    }

    /// 桥的成员口列表（不含桥自身）。
    pub async fn bridge_members(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<String>, PveError> {
        let out = self
            .runner
            .run(
                &format!("bridge-members:{name}"),
                &CommandSpec::new("ip", vec!["-o", "link", "show", "master", name]),
                None,
                cancel,
                true,
            )
            .await?;
        Ok(out
            .stdout
            .lines()
            .filter_map(|l| l.split(':').nth(1).map(|s| s.trim().to_string()))
            .collect())
    }

    /// 快照。已存在同名则跳过。
    pub async fn ensure_snapshot(
        &self,
        vmid: u32,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        #[derive(Deserialize)]
        struct Snap {
            #[serde(default)]
            name: String,
        }
        let snaps: Vec<Snap> = self
            .get_json(
                &format!("/nodes/{}/lxc/{vmid}/snapshot", self.node),
                &[],
                cancel,
            )
            .await
            .unwrap_or_default();
        if snaps.iter().any(|s| s.name == name) {
            return Ok(());
        }
        self.cmd(
            "snapshot",
            CommandSpec::new(
                "pct",
                ["snapshot".to_string(), vmid.to_string(), name.to_string()],
            )
            .with_timeout(Duration::from_secs(600)),
            cancel,
        )
        .await?;
        Ok(())
    }
}

/// 标准库没有 base64，这里手写一个最小的编码器（只用于脚本传输，不追求速度）。
pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp_line_parsing_handles_the_leading_space() {
        // 真机踩过：冒号后的空格没吃掉，卷名带着前导空格拼回命令行 → pct set 255
        let (idx, mount, size, volid) =
            parse_mp_line("mp0: local-lvm:vm-102-disk-1,mp=/data,size=2G").unwrap();
        assert_eq!(idx, 0);
        assert_eq!(mount, "/data");
        assert_eq!(size, Some(2));
        assert_eq!(volid, "local-lvm:vm-102-disk-1");
        assert!(!volid.starts_with(' '), "卷名不能带前导空格");

        // 顺序不同也要能认（PVE 的字段顺序不保证）
        let (i2, m2, s2, v2) = parse_mp_line("mp3: mp=/var/lib/x,local:3,size=512M").unwrap();
        assert_eq!((i2, m2.as_str(), s2, v2.as_str()), (3, "/var/lib/x", Some(1), "local:3"));
        assert!(parse_mp_line("rootfs: local-lvm:vm-100-disk-0,size=2G").is_none());
    }

    /// 真机踩过：`labmain` 用 `ip link del` 删了，重启后又回来，因为
    /// `/etc/network/interfaces` 里 `auto labmain` 段还在。这段 awk 就是用来
    /// 删那个段的——删错一格就会把别人的配置带走，所以直接跑一遍。
    #[test]
    fn unpersist_awk_removes_only_its_block() {
        // 用真机的实际形状：labmain 有管理地址（多一行 up），labzt 没有。
        let input = "\
auto lo
iface lo inet loopback

auto labmain
iface labmain inet manual
\tbridge-ports
\tbridge-stp off
\tbridge-fd 0
\tup ip addr replace 10.10.1.254/24 dev labmain

auto labzt
iface labzt inet manual
\tbridge-ports
\tbridge-stp off
\tbridge-fd 0

";
        let expect = "\
auto lo
iface lo inet loopback

auto labzt
iface labzt inet manual
\tbridge-ports
\tbridge-stp off
\tbridge-fd 0

";
        assert_eq!(run_unpersist_awk(input, "labmain"), expect);

        // 没有 up 行、且两段之间不空行时，绝不能吃掉下一段的首行
        // （这正是原来 sed 的 `+5d` 会出错的场景）
        let tight = "auto laba\niface laba inet manual\n\tbridge-fd 0\nauto labb\niface labb inet manual\n\tbridge-fd 0\n";
        let got = run_unpersist_awk(tight, "laba");
        assert_eq!(got, "auto labb\niface labb inet manual\n\tbridge-fd 0\n");
    }

    /// 单测里真跑一次 awk，返回结果。awk 与系统无关（只用 POSIX 特性）。
    fn run_unpersist_awk(input: &str, name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("labyard-awk-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("interfaces");
        std::fs::write(&src, input).unwrap();
        let out = std::process::Command::new("awk")
            .arg("-v")
            .arg(format!("n={name}"))
            .arg(BRIDGE_UNPERSIST_AWK)
            .arg(&src)
            .output()
            .expect("awk 应该存在");
        assert!(out.status.success(), "awk 失败: {}", String::from_utf8_lossy(&out.stderr));
        let _ = std::fs::remove_dir_all(&dir);
        String::from_utf8(out.stdout).unwrap()
    }

    /// `net_spec_matches` 决定"要不要下发 --netN"。判错的代价是热插拔掉容器
    /// 网卡、地址全丢，所以三个方向都要钉住：该重写的必须判 false，不该动的
    /// 必须判 true。
    #[test]
    fn net_spec_comparison_ignores_volatile_fields() {
        // 只有 hwaddr/type/firewall 不同 → 相等，不该重写
        assert!(net_spec_matches(
            "name=eth0,bridge=lablab,ip=manual,hwaddr=BC:24:11:AA:BB:CC,type=veth,firewall=1",
            0,
            "eth0",
            "lablab",
            "manual"
        ));
        // 桥变了 → 必须重写
        assert!(!net_spec_matches(
            "name=eth0,bridge=lablab,type=veth",
            0,
            "eth0",
            "wan",
            "manual"
        ));
        // ip 从 manual 变 dhcp → 必须重写
        assert!(!net_spec_matches(
            "name=eth0,bridge=wan,ip=manual,type=veth",
            0,
            "eth0",
            "wan",
            "dhcp"
        ));
        // 缺省 name/ip：PVE 的默认是 eth<idx> 与 dhcp
        assert!(net_spec_matches("bridge=wan,type=veth", 1, "eth1", "wan", "dhcp"));
        // 名字对不上（槽位顺序变了）→ 必须重写
        assert!(!net_spec_matches(
            "name=eth1,bridge=lablab,type=veth",
            0,
            "eth0",
            "lablab",
            "manual"
        ));
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn storage_content_parsing() {
        let s = Storage {
            content: Some("rootdir,vztmpl,iso".into()),
            kind: "dir".into(),
            ..Default::default()
        };
        assert!(s.supports("vztmpl"));
        assert!(!s.supports("images"));
        assert!(s.supports_snapshot());

        let thick = Storage {
            kind: "lvm".into(),
            ..Default::default()
        };
        assert!(!thick.supports_snapshot(), "厚 LVM 不支持快照");
    }

    #[test]
    fn cluster_resource_id_parsing() {
        let r = ClusterResource {
            id: "lxc/105".into(),
            ..Default::default()
        };
        assert_eq!(r.vmid_parsed(), Some(105));
        assert!(r.is_lxc());

        let q = ClusterResource {
            id: "qemu/200".into(),
            ..Default::default()
        };
        assert_eq!(q.vmid_parsed(), Some(200));
        assert!(!q.is_lxc());
    }
}
