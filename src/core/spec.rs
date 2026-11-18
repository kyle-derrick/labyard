//! YAML 规格与三层合并（defaults → zone → host）。
//!
//! 设计要点：
//! - `deny_unknown_fields`：写错字段名要当场报错，不能静默忽略（IaC 的经典事故）
//! - 合并是**逐字段覆盖**，不是整块替换：host 只写 `memory` 时，其余继承 zone 与 defaults
//! - zone 通过 `uplink` 递归引用另一个 zone，构成 DAG；环在 `topo_order` 里被拒绝

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 顶层规格。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    #[serde(default)]
    pub defaults: Defaults,
    pub zones: BTreeMap<String, ZoneSpec>,
}

/// 全局默认值。全部 Option，因为"没写"和"写了空值"要区分。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub cores: Option<u32>,
    pub memory: Option<u32>,
    /// 根盘大小，单位 GB
    pub disk: Option<u32>,
    pub template: Option<String>,
    pub storage: Option<String>,
    /// 模板（vztmpl）放哪个存储。**默认跟随 `storage`**——模板和 rootfs 在同一个
    /// 存储里的环境（dir 型、NFS 一体）零配置。rootfs 用 lvmthin/zfspool 之类
    /// 不支持 vztmpl 内容的存储时必须显式分开写（PVE 约定放在 `local`）。
    /// 真机验证教训：把模板下载打到 lvmthin 上，`pveam download` 直接 255。
    pub template_storage: Option<String>,
    pub node: Option<String>,
    pub dns: Option<Vec<String>>,
    pub ssh_keys: Option<Vec<String>>,
    /// 容器 root 密码；未写则只允许 SSH key
    pub password: Option<String>,
    pub bridge_prefix: Option<String>,
    pub vmid_base: Option<u32>,
}

/// 一个 zone = 一个网桥 + 一组主机 + 一个上联。
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneSpec {
    /// `"wan"` 表示直连外部网桥；其他值是另一个 zone 的名字
    pub uplink: String,
    pub description: Option<String>,
    /// IPv4 网段，CIDR
    pub v4: String,
    #[serde(default)]
    pub v6: V6Mode,
    /// 网关是否做地址转换。**默认开**——实验网的靶机要靠网关出网装包，
    /// 默认关掉会让每份 spec 都得显式写 `nat: true`，那是反的。
    /// 设 false 用于上游已经路由了整个网段的情形。
    #[serde(default = "default_true")]
    pub nat: bool,
    pub bridge: Option<String>,
    pub cores: Option<u32>,
    pub memory: Option<u32>,
    pub disk: Option<u32>,
    pub template: Option<String>,
    pub storage: Option<String>,
    pub template_storage: Option<String>,
    pub node: Option<String>,
    pub dns: Option<Vec<String>>,
    pub ssh_keys: Option<Vec<String>>,
    pub password: Option<String>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSpec>,
}

/// IPv6 策略。默认 `Auto`（发 ULA /64 + NAT66）。
///
/// 序列化形态是**标量**：`"auto"` / `"off"` / `"fd00:1234::/64"`，
/// 而不是 `{prefix: "..."}` 这种对象——YAML 里写对象太啰嗦。
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub enum V6Mode {
    #[default]
    Auto,
    Off,
    /// 显式前缀，如 "fd00:1234:5678:1::/64"
    Prefix(String),
}

impl<'de> Deserialize<'de> for V6Mode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Bool(bool),
            Str(String),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Bool(true) => V6Mode::Auto,
            Repr::Bool(false) => V6Mode::Off,
            Repr::Str(s) if s.eq_ignore_ascii_case("auto") => V6Mode::Auto,
            Repr::Str(s) if s.eq_ignore_ascii_case("off") || s.eq_ignore_ascii_case("none") => {
                V6Mode::Off
            }
            Repr::Str(s) => V6Mode::Prefix(s),
        })
    }
}

/// 一台主机。全部资源字段可覆盖上层。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostSpec {
    pub role: Role,
    /// IPv4 主机位，如 "1" 或 "10"；不写则按角色默认分配
    pub ip: Option<String>,
    /// IPv6 接口标识；不写则按 vmid 派生
    pub ipv6: Option<String>,
    pub cores: Option<u32>,
    pub memory: Option<u32>,
    pub disk: Option<u32>,
    pub template: Option<String>,
    pub storage: Option<String>,
    pub template_storage: Option<String>,
    pub node: Option<String>,
    pub dns: Option<Vec<String>>,
    pub ssh_keys: Option<Vec<String>>,
    pub password: Option<String>,
    /// 固定 vmid（不写则由 DB 按 vmid_base 顺序分配）
    pub vmid: Option<u32>,
    /// 要安装的 provisioner 目录名（`assets/provisioners/<name>/install.sh`）
    pub provisioner: Option<String>,
    /// 是否加入镜像链路（作为源或目标）
    #[serde(default)]
    pub mirror: MirrorRole,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Gateway,
    Analysis,
    Server,
    Client,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Analysis => "analysis",
            Role::Server => "server",
            Role::Client => "client",
        }
    }

    /// 默认主机位（用户没写 `ip` 时用）
    pub fn default_host_octet(self) -> u8 {
        match self {
            Role::Gateway => 1,
            Role::Analysis => 10,
            Role::Server => 20,
            Role::Client => 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MirrorRole {
    #[default]
    None,
    Source,
    Target,
}

/// 合并完成后的主机：所有字段都是确定值。
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedHost {
    pub name: String,
    pub zone: String,
    pub role: Role,
    pub vmid: Option<u32>,
    pub cores: u32,
    pub memory: u32,
    pub disk: u32,
    pub template: String,
    /// rootfs 落点
    pub storage: String,
    /// 模板所在存储；默认跟随 storage
    pub template_storage: String,
    pub node: Option<String>,
    pub dns: Vec<String>,
    pub ssh_keys: Vec<String>,
    pub password: Option<String>,
    /// 完整 IPv4 + 前缀长度
    pub ipv4: String,
    /// `/64` 前缀（关闭时为 None）
    pub ipv6_prefix: Option<String>,
    pub provisioner: Option<String>,
    pub mirror: MirrorRole,
}

/// 合并完成后的 zone。
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedZone {
    pub name: String,
    pub bridge: String,
    /// 上联：None 表示直接接外部网桥（wan）
    pub uplink: Option<String>,
    /// 上联网桥名（wan 时是用户配置的外部桥，否则是上游 zone 的桥）
    pub uplink_bridge: Option<String>,
    pub v4_cidr: String,
    pub v6_prefix: Option<String>,
    pub nat: bool,
    pub hosts: Vec<ResolvedHost>,
}

/// 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("YAML 解析失败: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("zone `{0}` 的 uplink `{1}` 不存在")]
    UnknownUplink(String, String),
    #[error("zone 之间存在环: {}", .0.join(" → "))]
    Cycle(Vec<String>),
    #[error("zone `{0}` 缺少必填项 {1}（defaults 里也没给）")]
    MissingField(String, String),
    #[error("zone `{0}` 的网段 `{1}` 不是合法的 IPv4 CIDR")]
    BadCidr(String, String),
    #[error("zone `{0}` 主机 `{1}` 的 IP `{2}` 不在网段 `{3}` 内")]
    IpOutOfRange(String, String, String, String),
    #[error("zone `{0}` 内主机 `{2}` 与 `{3}` 都占用 .{1}，地址冲突")]
    DuplicateIp(String, u8, String, String),
    #[error("zone 名 `{0}` 超过 10 字符，网桥名会超出 IFNAMSIZ(15) 限制")]
    ZoneNameTooLong(String),
    #[error("vmid 冲突: {0} 被 `{1}` 和 `{2}` 同时占用")]
    VmidConflict(u32, String, String),
}

impl Spec {
    pub fn parse(text: &str) -> Result<Self, SpecError> {
        Ok(serde_yaml_ng::from_str(text)?)
    }

    /// 解析成可直接执行的形态：合并默认值、展开地址、拓扑排序 zone。
    pub fn resolve(&self, wan_bridge: &str) -> Result<Vec<ResolvedZone>, SpecError> {
        let order = self.topo_order()?;
        let mut out = Vec::with_capacity(order.len());
        let mut used_vmids: HashMap<u32, String> = HashMap::new();

        // zone 的 v6 前缀在这里统一派生，保证同一份 spec 每次得到同一个前缀
        let mut v6_prefixes: HashMap<String, String> = HashMap::new();

        for zone_name in &order {
            let z = &self.zones[zone_name];
            if zone_name.chars().count() > 10 {
                return Err(SpecError::ZoneNameTooLong(zone_name.clone()));
            }

            let bridge = z
                .bridge
                .clone()
                .unwrap_or_else(|| format!("lab{}", sanitize(zone_name)));

            let (v4_net, v4_prefix) = parse_v4_cidr(zone_name, &z.v4)?;

            let uplink = if z.uplink == "wan" {
                None
            } else {
                Some(z.uplink.clone())
            };
            if let Some(up) = &uplink {
                if !self.zones.contains_key(up) {
                    return Err(SpecError::UnknownUplink(zone_name.clone(), up.clone()));
                }
            }
            let uplink_bridge = match &uplink {
                None => Some(wan_bridge.to_string()),
                Some(up) => Some(
                    self.zones[up]
                        .bridge
                        .clone()
                        .unwrap_or_else(|| format!("lab{}", sanitize(up))),
                ),
            };

            let v6_prefix = match &z.v6 {
                V6Mode::Off => None,
                V6Mode::Prefix(p) => Some(p.clone()),
                V6Mode::Auto => {
                    let p = v6_prefixes
                        .entry(zone_name.clone())
                        .or_insert_with(|| ula_prefix(zone_name))
                        .clone();
                    Some(p)
                }
            };

            let mut hosts = Vec::with_capacity(z.hosts.len());
            // 同一 zone 内主机号必须唯一。用手里的 HashSet 而不是事后扫一遍，
            // 是因为要能报出"谁跟谁撞了"。
            let mut used_octets: HashMap<u8, String> = HashMap::new();
            for (host_name, h) in &z.hosts {
                let cores = h
                    .cores
                    .or(z.cores)
                    .or(self.defaults.cores)
                    .ok_or_else(|| SpecError::MissingField(zone_name.clone(), "cores".into()))?;
                let memory = h
                    .memory
                    .or(z.memory)
                    .or(self.defaults.memory)
                    .ok_or_else(|| SpecError::MissingField(zone_name.clone(), "memory".into()))?;
                let disk = h
                    .disk
                    .or(z.disk)
                    .or(self.defaults.disk)
                    .ok_or_else(|| SpecError::MissingField(zone_name.clone(), "disk".into()))?;
                let template = h
                    .template
                    .clone()
                    .or_else(|| z.template.clone())
                    .or_else(|| self.defaults.template.clone())
                    .ok_or_else(|| SpecError::MissingField(zone_name.clone(), "template".into()))?;
                let storage = h
                    .storage
                    .clone()
                    .or_else(|| z.storage.clone())
                    .or_else(|| self.defaults.storage.clone())
                    .ok_or_else(|| SpecError::MissingField(zone_name.clone(), "storage".into()))?;
                // 模板存储：显式指定 > 跟随 rootfs 存储。这里**不做**"存储是否
                // 支持 vztmpl"的判断——那要查 PVE，是 doctor 的职责（见 doctor
                // 的"模板存储"检查项），解析层只做三层合并。
                let template_storage = h
                    .template_storage
                    .clone()
                    .or_else(|| z.template_storage.clone())
                    .or_else(|| self.defaults.template_storage.clone())
                    .unwrap_or_else(|| storage.clone());

                // 主机号：写了就校验，没写就按角色默认位往后找第一个空的。
                // 早先的实现直接用角色默认位，**同一个 zone 里两台 server 会拿到
                // 同一个 .20**，两台机器抢一个 IP，而 YAML 看起来完全正常。
                let host_octet = match &h.ip {
                    Some(s) => {
                        let o: u8 = s
                            .parse::<u32>()
                            .ok()
                            .filter(|v| (1..=254).contains(v))
                            .ok_or_else(|| {
                                SpecError::BadCidr(zone_name.clone(), format!("ip {s}"))
                            })? as u8;
                        o
                    }
                    None => {
                        let mut o = h.role.default_host_octet();
                        while used_octets.contains_key(&o) {
                            o = o.saturating_add(1);
                            if o > 254 {
                                return Err(SpecError::IpOutOfRange(
                                    zone_name.clone(),
                                    host_name.clone(),
                                    "（无可用主机号）".into(),
                                    z.v4.clone(),
                                ));
                            }
                        }
                        o
                    }
                };
                // 0 是网络地址、255 是广播地址，都不能给主机用
                if host_octet == 0 || host_octet == 255 {
                    return Err(SpecError::IpOutOfRange(
                        zone_name.clone(),
                        host_name.clone(),
                        format!("{v4_net}.{host_octet}"),
                        z.v4.clone(),
                    ));
                }
                if let Some(prev) = used_octets.insert(host_octet, host_name.clone()) {
                    return Err(SpecError::DuplicateIp(
                        zone_name.clone(),
                        host_octet,
                        prev,
                        host_name.clone(),
                    ));
                }
                let ipv4 = format!("{}.{}/{}", v4_net, host_octet, v4_prefix);

                let qualified = format!("{}:{}", zone_name, host_name);
                if let Some(v) = h.vmid {
                    if let Some(prev) = used_vmids.insert(v, qualified.clone()) {
                        return Err(SpecError::VmidConflict(v, prev, qualified));
                    }
                }

                hosts.push(ResolvedHost {
                    name: host_name.clone(),
                    zone: zone_name.clone(),
                    role: h.role,
                    vmid: h.vmid,
                    cores,
                    memory,
                    disk,
                    template,
                    storage,
                    template_storage,
                    node: h
                        .node
                        .clone()
                        .or_else(|| z.node.clone())
                        .or_else(|| self.defaults.node.clone()),
                    dns: h
                        .dns
                        .clone()
                        .or_else(|| z.dns.clone())
                        .or_else(|| self.defaults.dns.clone())
                        .unwrap_or_default(),
                    ssh_keys: h
                        .ssh_keys
                        .clone()
                        .or_else(|| z.ssh_keys.clone())
                        .or_else(|| self.defaults.ssh_keys.clone())
                        .unwrap_or_default(),
                    password: h
                        .password
                        .clone()
                        .or_else(|| z.password.clone())
                        .or_else(|| self.defaults.password.clone()),
                    ipv4,
                    ipv6_prefix: v6_prefix.clone(),
                    provisioner: h.provisioner.clone(),
                    mirror: h.mirror,
                });
            }

            out.push(ResolvedZone {
                name: zone_name.clone(),
                bridge,
                uplink,
                uplink_bridge,
                v4_cidr: z.v4.clone(),
                v6_prefix,
                nat: z.nat,
                hosts,
            });
        }

        Ok(out)
    }

    /// 拓扑排序：被依赖的 zone 排在前面（上游先建）。
    pub fn topo_order(&self) -> Result<Vec<String>, SpecError> {
        let mut order = Vec::new();
        let mut state: HashMap<&str, u8> = HashMap::new(); // 0=未访问 1=访问中 2=完成

        fn visit<'a>(
            name: &'a str,
            zones: &'a BTreeMap<String, ZoneSpec>,
            state: &mut HashMap<&'a str, u8>,
            order: &mut Vec<String>,
            stack: &mut Vec<String>,
        ) -> Result<(), SpecError> {
            match state.get(name).copied().unwrap_or(0) {
                2 => return Ok(()),
                1 => {
                    let mut cyc: Vec<String> = stack.clone();
                    cyc.push(name.to_string());
                    return Err(SpecError::Cycle(cyc));
                }
                _ => {}
            }
            state.insert(name, 1);
            if let Some(z) = zones.get(name) {
                if z.uplink != "wan" {
                    let up = z.uplink.as_str();
                    if !zones.contains_key(up) {
                        return Err(SpecError::UnknownUplink(name.to_string(), up.to_string()));
                    }
                    stack.push(name.to_string());
                    visit(up, zones, state, order, stack)?;
                    stack.pop();
                }
            }
            state.insert(name, 2);
            order.push(name.to_string());
            Ok(())
        }

        // 用 BTreeMap 的键顺序保证结果稳定（同样的输入 → 同样的顺序）
        for name in self.zones.keys() {
            visit(name, &self.zones, &mut state, &mut order, &mut Vec::new())?;
        }

        // 去重保序
        let mut seen = BTreeSet::new();
        order.retain(|n| seen.insert(n.clone()));
        Ok(order)
    }

}

fn sanitize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(10)
        .collect::<String>()
        .to_lowercase()
}

fn parse_v4_cidr(zone: &str, cidr: &str) -> Result<(String, u8), SpecError> {
    let (base, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| SpecError::BadCidr(zone.to_string(), cidr.to_string()))?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| SpecError::BadCidr(zone.to_string(), cidr.to_string()))?;
    if prefix > 30 {
        return Err(SpecError::BadCidr(zone.to_string(), cidr.to_string()));
    }
    let octets: Vec<&str> = base.split('.').collect();
    if octets.len() != 4 {
        return Err(SpecError::BadCidr(zone.to_string(), cidr.to_string()));
    }
    let mut nums = [0u8; 4];
    for (i, o) in octets.iter().enumerate() {
        nums[i] = o
            .parse()
            .map_err(|_| SpecError::BadCidr(zone.to_string(), cidr.to_string()))?;
    }
    // /24 时取前三段作为前缀（最常见），其余情况保留原文
    let net = if prefix == 24 {
        format!("{}.{}.{}", nums[0], nums[1], nums[2])
    } else {
        format!("{}.{}.{}.{}", nums[0], nums[1], nums[2], nums[3])
    };
    Ok((net, prefix))
}

/// 从 zone 名确定性地派生一个 ULA /64 前缀（fd00::/8 内）。
/// 同一个 zone 名永远得到同一个前缀，不需要持久化。
///
/// 格式必须是 `fdXX:XXXX:XXXX::/64`——IPv6 每 16 bit 一组、每组 4 个 hex。
/// 之前这里漏了中间冒号，产出了 `fd0d6e40:e367::/64` 这种非法地址。
pub fn ula_prefix(zone: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(zone.as_bytes());
    let d = h.finalize();
    // fd + 40 bit global id + 16 bit subnet = 48 bit 前缀。
    // 注意分组：`fd` 本身占 8 bit，所以第一组是 `fdXX`（共 4 hex），
    // 不能写成 `fdXXXX`（8 hex，非法）。
    format!(
        "fd{:02x}:{:02x}{:02x}:{:02x}{:02x}::/64",
        d[0], d[1], d[2], d[3], d[4]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
defaults:
  cores: 2
  memory: 1024
  disk: 8
  template: debian-13-standard
  storage: local-lvm
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      server1: { role: server }
"#;

    #[test]
    fn merge_three_layers() {
        let spec = Spec::parse(BASE).unwrap();
        let zs = spec.resolve("vmbr0").unwrap();
        let h = &zs[0].hosts[1];
        assert_eq!(h.name, "server1");
        assert_eq!(h.cores, 2);
        assert_eq!(h.memory, 1024);
        assert_eq!(h.template, "debian-13-standard");
        assert_eq!(h.ipv4, "10.10.1.20/24");
    }

    #[test]
    fn host_overrides_zone_and_defaults() {
        let y = r#"
defaults: { cores: 2, memory: 1024, disk: 8, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.0.9.0/24
    memory: 2048
    hosts:
      a: { role: server, memory: 4096, ip: "77" }
"#;
        let spec = Spec::parse(y).unwrap();
        let zs = spec.resolve("vmbr0").unwrap();
        assert_eq!(zs[0].hosts[0].memory, 4096);
        assert_eq!(zs[0].hosts[0].cores, 2);
        assert_eq!(zs[0].hosts[0].ipv4, "10.0.9.77/24");
    }

    #[test]
    fn nested_zones_are_ordered_upstream_first() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts: { gateway: { role: gateway } }
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway } }
"#;
        let spec = Spec::parse(y).unwrap();
        let order = spec.topo_order().unwrap();
        assert_eq!(order, vec!["main", "sub"]);
        let zs = spec.resolve("vmbr0").unwrap();
        assert_eq!(zs[0].name, "main");
        assert_eq!(zs[1].uplink, Some("main".into()));
        assert_eq!(zs[1].uplink_bridge, Some("labmain".into()));
    }

    #[test]
    fn cycle_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  a: { uplink: b, v4: 10.1.0.0/24, hosts: { g: { role: gateway } } }
  b: { uplink: a, v4: 10.2.0.0/24, hosts: { g: { role: gateway } } }
"#;
        let spec = Spec::parse(y).unwrap();
        assert!(matches!(spec.topo_order(), Err(SpecError::Cycle(_))));
    }

    #[test]
    fn unknown_uplink_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  a: { uplink: nope, v4: 10.1.0.0/24, hosts: { g: { role: gateway } } }
"#;
        let spec = Spec::parse(y).unwrap();
        assert!(matches!(
            spec.topo_order(),
            Err(SpecError::UnknownUplink(_, _))
        ));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  a:
    uplink: wan
    v4: 10.1.0.0/24
    hostz: { g: { role: gateway } }
"#;
        assert!(Spec::parse(y).is_err(), "拼错的字段名必须报错");
    }

    #[test]
    fn ula_prefix_is_stable_and_unique() {
        assert_eq!(ula_prefix("main"), ula_prefix("main"));
        assert_ne!(ula_prefix("main"), ula_prefix("sub"));
        assert!(ula_prefix("main").starts_with("fd"));
    }

    #[test]
    fn ula_prefix_is_well_formed_ipv6() {
        // 这条是回归测试：曾经产出 `fd0d6e40:e367::/64`，第一组 8 个 hex，非法
        for zone in ["main", "sub", "a", "zone-with-long-name", "红队"] {
            let p = ula_prefix(zone);
            assert!(p.ends_with("::/64"), "{p} 应以 ::/64 结尾");
            let core = p.strip_suffix("::/64").unwrap();
            let groups: Vec<&str> = core.split(':').collect();
            assert_eq!(groups.len(), 3, "{p} 应有 3 组（48 bit 前缀），实际 {groups:?}");
            for g in &groups {
                assert_eq!(g.len(), 4, "{p} 的每组必须是 4 个 hex，实际 {g}");
                assert!(
                    g.chars().all(|c| c.is_ascii_hexdigit()),
                    "{p} 含非 hex 字符"
                );
            }
            assert!(groups[0].starts_with("fd"), "{p} 必须落在 fd00::/8");
        }
    }

    #[test]
    fn same_role_hosts_get_distinct_octets() {
        // 回归：早先每台机器都直接用角色默认位，同 zone 的第二个 server
        // 会拿到同一个 .20，两台机器抢一个 IP 而 YAML 看起来完全正常。
        let y = r#"
defaults: { cores: 2, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      s1: { role: server }
      s2: { role: server }
      s3: { role: server }
"#;
        let z = Spec::parse(y).unwrap().resolve("vmbr0").unwrap();
        let ips: Vec<&str> = z[0].hosts.iter().map(|h| h.ipv4.as_str()).collect();
        assert_eq!(
            ips,
            vec!["10.10.1.20/24", "10.10.1.21/24", "10.10.1.22/24"]
        );
    }

    #[test]
    fn explicit_duplicate_ip_is_rejected() {
        let y = r#"
defaults: { cores: 2, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      a: { role: server, ip: 30 }
      b: { role: client, ip: 30 }
"#;
        let err = Spec::parse(y).unwrap().resolve("vmbr0").unwrap_err();
        assert!(
            matches!(err, SpecError::DuplicateIp(..)),
            "应报地址冲突，实际: {err}"
        );
    }

    #[test]
    fn nat_defaults_to_true_and_can_be_turned_off() {
        let y = r#"
defaults: { cores: 2, memory: 512, disk: 4, template: t, storage: s }
zones:
  on:
    uplink: wan
    v4: 10.1.0.0/24
    hosts: { g: { role: gateway } }
  off:
    uplink: on
    v4: 10.2.0.0/24
    nat: false
    hosts: { g: { role: gateway } }
"#;
        let z = Spec::parse(y).unwrap().resolve("vmbr0").unwrap();
        assert!(z[0].nat, "不写 nat 时默认开——否则靶机出不了网");
        assert!(!z[1].nat, "显式 nat: false 必须生效");
    }

    #[test]
    fn vmid_conflict_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  a:
    uplink: wan
    v4: 10.1.0.0/24
    hosts:
      x: { role: server, vmid: 100 }
      y: { role: client, vmid: 100 }
"#;
        let spec = Spec::parse(y).unwrap();
        assert!(matches!(
            spec.resolve("vmbr0"),
            Err(SpecError::VmidConflict(100, _, _))
        ));
    }
}
