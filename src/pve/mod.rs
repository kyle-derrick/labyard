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
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Pve {
    runner: Arc<dyn Runner>,
    pub node: String,
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
}

impl Pve {
    /// `wan_bridge` 不在这里存一份：它只属于 `config.pve`，两处并存就是两个真相源。
    pub fn new(runner: Arc<dyn Runner>, node: impl Into<String>) -> Self {
        Self {
            runner,
            node: node.into(),
        }
    }

    /// `pvesh get ... --output-format json`
    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        extra: &[&str],
        cancel: &CancellationToken,
    ) -> Result<T, PveError> {
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
        serde_json::from_str(text).map_err(|source| PveError::Json {
            cmd: format!("pvesh get {path}"),
            source,
        })
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
        let display = spec.display();
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

    pub async fn nodes(&self, cancel: &CancellationToken) -> Result<Vec<String>, PveError> {
        #[derive(Deserialize)]
        struct N {
            #[serde(default)]
            node: String,
        }
        let v: Vec<N> = self.get_json("/nodes", &[], cancel).await?;
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
# 宿主管理地址：Web 终端与调试从宿主 L2 直达容器，不经网关转发，
# 所以网关的入向丢弃规则对这些流量不生效（isolation 语义不变。
# mirr 是纯二层桥，mgmt 为空时跳过）
if [ -n "{mgmt_ip}" ]; then
  ip addr replace {mgmt_ip} dev {name}
fi
# 持久化（PVE 9 用 /etc/network/interfaces，追加式，重复执行不重复写）
if ! grep -q "^iface {name} " /etc/network/interfaces 2>/dev/null; then
  cat >> /etc/network/interfaces <<EOF

auto {name}
iface {name} inet manual
	bridge-ports
	bridge-stp off
	bridge-fd 0
EOF
  echo "persisted {name}"
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

    /// 建 CT。已存在则改为 `pct set` 更新配置（幂等的关键）。
    ///
    /// 三件事必须在这里做掉，否则后面的步骤全是空转：
    /// 1. `--start 1`——`pct create` 默认建出来是**停的**，而下一步就是 `pct exec`
    /// 2. 网关要有第二块网卡 `eth1` 上联，否则网关脚本里的 `ip link set eth1 up` 直接失败
    /// 3. 内网口用 `ip=manual`——内网桥上没有 DHCP 服务，写 `ip=dhcp` 只会白白等超时，
    ///    地址由后面的 net 步骤静态配
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
        mirr_bridge: Option<&str>,
        password: Option<&str>,
        ssh_keys: &[String],
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let existing = self
            .cluster_vms(cancel)
            .await?
            .into_iter()
            .find(|r| r.is_lxc() && r.vmid_parsed() == Some(vmid));

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
                    "--net0".into(),
                    format!("name=eth0,bridge={bridge},ip=manual,type=veth"),
                    "--unprivileged".into(),
                    "1".into(),
                    "--features".into(),
                    "nesting=1".into(),
                    "--onboot".into(),
                    "1".into(),
                    "--start".into(),
                    "1".into(),
                ];
                if let Some(up) = uplink_bridge {
                    args.push("--net1".into());
                    // 上联网卡走 DHCP：wan 桥通常有 DHCP，或用户自己配好再改
                    args.push(format!("name=eth1,bridge={up},ip=dhcp,type=veth"));
                }
                if let Some(mirr) = mirr_bridge {
                    // analysis 的抓包口：纯二层、无 IP（ip=manual 不配地址）
                    args.push("--net2".into());
                    args.push(format!("name=eth2,bridge={mirr},ip=manual,type=veth"));
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
                // 已存在：只更新会漂移的字段，一条命令合并，避免 N 次 pct set 打 pmxcfs
                let mut args: Vec<String> = vec!["set".into(), vmid.to_string()];
                args.extend(["--cores".into(), cores.to_string()]);
                args.extend(["--memory".into(), memory.to_string()]);
                args.extend(["--hostname".into(), hostname.into()]);
                if let Some(up) = uplink_bridge {
                    // 老容器可能是上一版建的，只有一块网卡——补上而不是重建
                    args.push("--net1".into());
                    args.push(format!("name=eth1,bridge={up},ip=dhcp,type=veth"));
                }
                if let Some(mirr) = mirr_bridge {
                    args.push("--net2".into());
                    args.push(format!("name=eth2,bridge={mirr},ip=manual,type=veth"));
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
                }
            }
        }
        Ok(())
    }

    /// 容器内执行。返回完整输出。
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
        self.cmd(
            key,
            CommandSpec::new("pct", args).with_timeout(timeout),
            cancel,
        )
        .await
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
        // 用 base64 传输，避免引号/换行在多层转义里出错
        let b64 = base64_encode(script.as_bytes());
        let inner = format!(
            "set -e; echo {b64} | base64 -d > /tmp/labyard-step.sh; chmod +x /tmp/labyard-step.sh; /tmp/labyard-step.sh"
        );
        self.ct_exec(key, vmid, &["sh", "-c", &inner], timeout, cancel)
            .await
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
                PveError::Run(crate::core::runner::RunError::NonZero {
                    cmd: format!("ip -4 addr show {iface}"),
                    code: None,
                })
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
    /// 只清配置段。用 begin/end 行匹配整段删除，段格式见 ensure_bridge。
    pub async fn destroy_bridge(
        &self,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let script = format!(r#"set -e
ip link del {name} 2>/dev/null && echo "bridge {name} deleted" || echo "bridge {name} absent"
# 从 interfaces 里删掉持久化段（auto {name} 起到本段空行止）
if grep -q "^iface {name} " /etc/network/interfaces 2>/dev/null; then
  sed -i "/^auto {name}$/,+5d" /etc/network/interfaces
  ifreload -a 2>/dev/null || true
fi
echo done"#);
        self.runner
            .run(
                &format!("bridge-destroy:{name}"),
                &CommandSpec::new("sh", vec!["-c".into(), script]).with_timeout(Duration::from_secs(30)),
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
