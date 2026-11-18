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

use crate::core::runner::{CommandSpec, Output, Runner};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct Pve {
    runner: Arc<dyn Runner>,
    pub node: String,
    /// 外部网桥（wan 的落点），来自 config.toml
    pub wan_bridge: String,
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
    Other(String),
}

impl Pve {
    pub fn new(runner: Arc<dyn Runner>, node: impl Into<String>, wan_bridge: impl Into<String>) -> Self {
        Self {
            runner,
            node: node.into(),
            wan_bridge: wan_bridge.into(),
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

    /// 跑一条写命令，返回完整输出（调用方自己判断退出码）。
    async fn cmd(
        &self,
        key: &str,
        spec: CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<Output, PveError> {
        Ok(self.runner.run(key, &spec, None, cancel, true).await?)
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

    pub async fn bridges(&self, cancel: &CancellationToken) -> Result<Vec<Iface>, PveError> {
        let v: Vec<Iface> = self
            .get_json(&format!("/nodes/{}/network", self.node), &[], cancel)
            .await?;
        Ok(v.into_iter().filter(|i| i.kind == "bridge").collect())
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
    pub async fn ct_config(
        &self,
        vmid: u32,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, PveError> {
        self.get_json(&format!("/nodes/{}/lxc/{vmid}/config", self.node), &[], cancel)
            .await
    }

    // ───────────────────────── 写操作 ─────────────────────────

    /// 建桥：`ip link add ... type bridge` + 持久化到 `/etc/network/interfaces`。
    /// 幂等：已存在则跳过。
    pub async fn ensure_bridge(
        &self,
        name: &str,
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
        password: Option<&str>,
        ssh_keys: &[String],
        cancel: &CancellationToken,
    ) -> Result<(), PveError> {
        let exists = self.cluster_vms(cancel).await?.iter().any(|r| {
            r.is_lxc() && r.vmid_parsed() == Some(vmid)
        });

        if !exists {
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
                format!("name=eth0,bridge={bridge},ip=dhcp,type=veth"),
                "--unprivileged".into(),
                "1".into(),
                "--features".into(),
                "nesting=1".into(),
                "--onboot".into(),
                "1".into(),
            ];
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
        } else {
            // 已存在：只更新会漂移的字段，一条命令合并，避免 N 次 pct set 打 pmxcfs
            let mut args: Vec<String> = vec!["set".into(), vmid.to_string()];
            args.extend(["--cores".into(), cores.to_string()]);
            args.extend(["--memory".into(), memory.to_string()]);
            args.extend(["--hostname".into(), hostname.into()]);
            self.cmd(
                "ct-update",
                CommandSpec::new("pct", args).with_timeout(Duration::from_secs(120)),
                cancel,
            )
            .await?;
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
