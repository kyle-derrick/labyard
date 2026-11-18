//! 宿主侧配置（`/etc/labyard/config.toml`）。
//!
//! 全部有默认值：文件缺失也能启动，但 `doctor` 会警告"没配 node"。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub pve: PveCfg,
    pub server: ServerCfg,
    pub paths: PathsCfg,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PveCfg {
    /// 单节点部署；多节点时这里写要操作的那一个
    pub node: String,
    /// 外部网桥（wan 的落点）
    pub wan_bridge: String,
    /// vmid 起始号
    pub vmid_base: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerCfg {
    pub bind: String,
    pub port: u16,
    /// 自签证书路径；两者都存在才启用 TLS
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// cookie 签名密钥的存放位置（首次启动自动生成）
    pub secret_file: PathBuf,
    /// 会话有效期（小时）
    pub session_hours: i64,
    /// 是否允许无认证访问（仅测试用，默认 false）
    pub allow_anonymous: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathsCfg {
    pub db: PathBuf,
    pub locks: PathBuf,
    pub tmp: PathBuf,
    /// 用户上传/保存的 spec 目录
    pub specs: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pve: PveCfg::default(),
            server: ServerCfg::default(),
            paths: PathsCfg::default(),
        }
    }
}

impl Default for PveCfg {
    fn default() -> Self {
        Self {
            node: "pve".into(),
            wan_bridge: "vmbr0".into(),
            vmid_base: 100,
        }
    }
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".into(),
            port: 8443,
            tls_cert: None,
            tls_key: None,
            secret_file: PathBuf::from("/var/lib/labyard/secret.key"),
            session_hours: 8,
            allow_anonymous: false,
        }
    }
}

impl Default for PathsCfg {
    fn default() -> Self {
        Self {
            db: PathBuf::from("/var/lib/labyard/labyard.db"),
            locks: PathBuf::from("/var/lib/labyard/locks"),
            tmp: PathBuf::from("/var/lib/labyard/tmp"),
            specs: PathBuf::from("/var/lib/labyard/specs"),
        }
    }
}

impl Config {
    /// 读配置；文件不存在时返回默认值并给出提示（不失败，否则首次启动就卡住）。
    pub fn load(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => (c, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("配置文件解析失败，已用默认值启动: {e}")),
                ),
            },
            Err(_) => (
                Config::default(),
                Some(format!("配置文件 {} 不存在，已用默认值启动", path.display())),
            ),
        }
    }

    pub fn tls_enabled(&self) -> bool {
        self.server.tls_cert.is_some() && self.server.tls_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.pve.node, "pve");
        assert_eq!(c.pve.wan_bridge, "vmbr0");
        assert_eq!(c.server.port, 8443);
        assert!(!c.tls_enabled());
        assert!(!c.server.allow_anonymous);
    }

    #[test]
    fn partial_toml_overrides_only_given_fields() {
        let c: Config = toml::from_str(
            r#"
[pve]
node = "node1"
"#,
        )
        .unwrap();
        assert_eq!(c.pve.node, "node1");
        assert_eq!(c.pve.wan_bridge, "vmbr0", "未写的字段应保持默认");
    }

    #[test]
    fn unknown_field_rejected() {
        let r: Result<Config, _> = toml::from_str("[pve]\nnod = \"x\"\n");
        assert!(r.is_err(), "拼错字段名必须报错");
    }
}
