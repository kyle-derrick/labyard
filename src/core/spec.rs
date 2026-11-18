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
    /// 显式镜像连线（源主机 → 分析机）。host 级的 `mirror: source/target`
    /// 仍然有效（同 zone 内全连），这里是它的精确版：可以跨 zone 指定。
    /// 两条路径最后都收敛成同一批 `ResolvedMirror`，执行器不需要知道来源。
    #[serde(default)]
    pub mirrors: Vec<MirrorLink>,
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
    /// 时区（如 Asia/Shanghai）。走容器内 bootstrap 落地。
    pub timezone: Option<String>,
    /// 系统 locale（如 zh_CN.UTF-8）。设置后 bootstrap 里 locale-gen 生成。
    pub lang: Option<String>,
    /// apt 源地址（如清华源）。无外网的靶机可指向内网 apt-cacher-ng。
    /// 不写则只把官方 http 源升级成 https。
    pub apt_mirror: Option<String>,
    /// 额外预装包（vim curl jq 之类）。显式声明的包安装失败会让步骤失败——
    /// 你点名要的东西装不上不该被静默跳过。
    pub packages: Option<Vec<String>>,
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
    pub timezone: Option<String>,
    pub lang: Option<String>,
    pub apt_mirror: Option<String>,
    pub packages: Option<Vec<String>>,
    /// 宿主机在 zone 网桥上的管理地址（Web 终端/调试直达用）。
    /// 不写则自动取网段 .254/前缀。宿主从这里 L2 直达容器，
    /// 不经过网关转发——所以网关的入向丢弃规则不受影响。
    pub bridge_mgmt_ip: Option<String>,
    pub node: Option<String>,
    pub dns: Option<Vec<String>>,
    pub ssh_keys: Option<Vec<String>>,
    pub password: Option<String>,
    /// 网关的运行策略：出网、端口映射、VPN。只有含 `role: gateway` 主机的
    /// zone 能写——没有网关时这些字段无处落地，resolve 会直接报错。
    pub gateway: Option<GatewaySpec>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSpec>,
}

/// 网关策略（声明式）。运行时改这些字段会**写回规格**，不在容器里留影子状态。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySpec {
    /// 内网是否允许出网。默认 true（实验网靶机要装包）。
    /// 关掉 = 转发链上把 LAN→WAN 丢弃，NAT 规则保留（打开即恢复）。
    pub egress: Option<bool>,
    /// 入向端口映射（DNAT）。列表即全量：删掉一行就删规则。
    #[serde(default, rename = "pub")]
    pub pubs: Vec<PubSpec>,
    #[serde(default)]
    pub vpn: VpnSpec,
}

/// 一条端口映射：`{proto: tcp, port: 2222, to: "10.10.1.20:22"}`
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PubSpec {
    #[serde(default)]
    pub proto: Proto,
    /// 网关对外的端口
    pub port: u16,
    /// 内网目标，`ip` 或 `ip:port`（省略端口 = 与 `port` 相同）
    pub to: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    #[default]
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Tcp => "tcp",
            Proto::Udp => "udp",
        }
    }
}

/// VPN 开关。具体实现用成熟组件：内核 wireguard + openvpn，不自己写隧道。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VpnSpec {
    #[serde(default)]
    pub wireguard: bool,
    #[serde(default)]
    pub openvpn: bool,
    /// 客户端名（决定生成的配置文件名）。空 = `["lab"]`
    #[serde(default)]
    pub clients: Vec<String>,
}

/// 一条显式镜像连线。`from`/`to` 是 `host` 或 `zone:host`。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorLink {
    pub from: String,
    pub to: String,
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
    pub timezone: Option<String>,
    pub lang: Option<String>,
    pub apt_mirror: Option<String>,
    pub packages: Option<Vec<String>>,
    /// 宿主机在 zone 网桥上的管理地址（Web 终端/调试直达用）。
    /// 不写则自动取网段 .254/前缀。宿主从这里 L2 直达容器，
    /// 不经过网关转发——所以网关的入向丢弃规则不受影响。
    pub bridge_mgmt_ip: Option<String>,
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
    pub timezone: Option<String>,
    pub lang: Option<String>,
    pub apt_mirror: Option<String>,
    pub packages: Vec<String>,
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
    /// 宿主在桥上的管理地址；Web 终端从宿主直连容器用
    pub bridge_mgmt_ip: String,
    pub v6_prefix: Option<String>,
    pub nat: bool,
    /// 网关策略；zone 里没有 gateway 角色主机时为 None
    pub gateway: Option<ResolvedGateway>,
    pub hosts: Vec<ResolvedHost>,
}

/// 网关策略的确定形态（端口已拆成数值，客户端列表已补默认值）。
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedGateway {
    pub egress: bool,
    pub pubs: Vec<ResolvedPub>,
    pub vpn: ResolvedVpn,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedPub {
    pub proto: Proto,
    /// 网关对外端口
    pub port: u16,
    pub to_ip: String,
    pub to_port: u16,
}

impl ResolvedPub {
    /// `tcp:2222:10.10.1.20:22` —— 脚本与前端都用这一个形态，避免三处拼接。
    pub fn spec_string(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.proto.as_str(),
            self.port,
            self.to_ip,
            self.to_port
        )
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResolvedVpn {
    pub wireguard: bool,
    pub openvpn: bool,
    pub clients: Vec<String>,
}

impl ResolvedVpn {
    pub fn enabled(&self) -> bool {
        self.wireguard || self.openvpn
    }
}

/// 解析后的整份规格：zone 与镜像连线。
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedSpec {
    pub zones: Vec<ResolvedZone>,
    pub mirrors: Vec<ResolvedMirror>,
}

/// 一条要落地的镜像连线。
///
/// 只带 `zone:host` 与源桥名，不带 vmid——vmid 是 DB 分配的，由计划层填。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResolvedMirror {
    pub from_zone: String,
    pub from_host: String,
    /// 源桥（zone 名与桥名不一定相同：lab → lablab）
    pub src_bridge: String,
    pub to_zone: String,
    pub to_host: String,
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
    #[error("zone `{0}` 配了 `gateway` 但没有 role: gateway 的主机——策略无处落地")]
    GatewayWithoutHost(String),
    #[error("zone `{0}` 的端口映射目标 `{1}` 不是合法的 ip 或 ip:port")]
    BadPubTarget(String, String),
    #[error("zone `{0}` 的端口映射里 {1} 被写了两次——后一条永远不会命中")]
    PubPortConflict(String, String),
    #[error("VPN 客户端名 `{0}` 非法（只允许小写字母、数字、- _，最长 20）")]
    BadVpnClient(String),
    #[error("镜像连线引用的主机 `{0}` 不存在")]
    MirrorUnknownHost(String),
    #[error("镜像连线 `{0}` 有歧义：多个 zone 里都有这个主机名，请写成 zone:host")]
    MirrorAmbiguous(String),
    #[error("镜像连线 `{0}` → `{1}`：目标必须是 role: analysis（只有它挂了 mirr 口）")]
    MirrorTargetNotAnalysis(String, String),
}

impl Spec {
    pub fn parse(text: &str) -> Result<Self, SpecError> {
        Ok(serde_yaml_ng::from_str(text)?)
    }

    /// 解析成可直接执行的形态：合并默认值、展开地址、拓扑排序 zone。
    ///
    /// 只要 zone 的调用方（多数）用这个；需要镜像连线的一律用 `resolve_all`，
    /// 否则镜像会被静默丢掉——这正是"解析有两套"最容易出的事故。
    pub fn resolve(&self, wan_bridge: &str) -> Result<Vec<ResolvedZone>, SpecError> {
        Ok(self.resolve_all(wan_bridge)?.zones)
    }

    /// 完整解析：zone + 镜像连线。
    pub fn resolve_all(&self, wan_bridge: &str) -> Result<ResolvedSpec, SpecError> {
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
                // 基础配置三层合并：host > zone > defaults，全 None 时留给
                // bootstrap.sh 的内置默认（时区上海、官方源 https 化）。
                let timezone = h
                    .timezone
                    .clone()
                    .or_else(|| z.timezone.clone())
                    .or_else(|| self.defaults.timezone.clone());
                let lang = h
                    .lang
                    .clone()
                    .or_else(|| z.lang.clone())
                    .or_else(|| self.defaults.lang.clone());
                let apt_mirror = h
                    .apt_mirror
                    .clone()
                    .or_else(|| z.apt_mirror.clone())
                    .or_else(|| self.defaults.apt_mirror.clone());
                // Option 链而不是"空列表当未设置"：`packages: []` 必须能表达
                // "显式不要包"——无外网的靶机继承 defaults 的包清单只会装不上。
                let packages = h
                    .packages
                    .clone()
                    .or_else(|| z.packages.clone())
                    .or_else(|| self.defaults.packages.clone())
                    .unwrap_or_default();

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
                    timezone,
                    lang,
                    apt_mirror,
                    packages,
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

            // 宿主管理地址：显式配置优先，否则取网段 .254（网关占 .1、
            // 主机从 .2 起分配，.254 实际不会冲突）
            let mgmt = z.bridge_mgmt_ip.clone().unwrap_or_else(|| {
                let (net, prefix) = z.v4.split_once('/').unwrap_or((&z.v4, "24"));
                let mut parts: Vec<&str> = net.split('.').collect();
                if parts.len() == 4 {
                    parts[3] = "254";
                    format!("{}/{}", parts.join("."), prefix)
                } else {
                    format!("{net}/254")
                }
            });

            // ── 网关策略 ──
            // 没有网关主机时，`gateway:` 里写的任何东西都没有落地的地方。
            // 静默忽略会让"我明明配了端口映射"变成一场排查——这里直接拒绝。
            let has_gateway = hosts.iter().any(|h| h.role == Role::Gateway);
            let gateway = if !has_gateway {
                if z.gateway.is_some() {
                    return Err(SpecError::GatewayWithoutHost(zone_name.clone()));
                }
                None
            } else {
                // 有网关但没写策略：给出默认（出网开、无映射、无 VPN），
                // 让执行器只处理一种形态，不必到处 Option 判断
                Some(match &z.gateway {
                    Some(g) => resolve_gateway(zone_name, g)?,
                    None => ResolvedGateway {
                        egress: true,
                        pubs: Vec::new(),
                        vpn: ResolvedVpn::default(),
                    },
                })
            };

            out.push(ResolvedZone {
                name: zone_name.clone(),
                bridge,
                uplink,
                uplink_bridge,
                v4_cidr: z.v4.clone(),
                bridge_mgmt_ip: mgmt,
                v6_prefix,
                nat: z.nat,
                gateway,
                hosts,
            });
        }

        let mirrors = self.resolve_mirrors(&out)?;
        Ok(ResolvedSpec { zones: out, mirrors })
    }

    /// 镜像连线：显式 `mirrors:` 优先，host 级 `mirror: source/target` 兜底
    /// （同 zone 内源×目标全连，保持旧语义）。两者按 `zone:host` 去重。
    fn resolve_mirrors(&self, zones: &[ResolvedZone]) -> Result<Vec<ResolvedMirror>, SpecError> {
        let mut out: Vec<ResolvedMirror> = Vec::new();
        let mut seen: BTreeSet<(String, String, String, String)> = BTreeSet::new();

        let find = |r: &str| -> Result<(String, String), SpecError> {
            // `zone:host` 或裸 `host`
            if let Some((z, h)) = r.split_once(':') {
                return zones
                    .iter()
                    .find(|zz| zz.name == z && zz.hosts.iter().any(|x| x.name == h))
                    .map(|zz| (zz.name.clone(), h.to_string()))
                    .ok_or_else(|| SpecError::MirrorUnknownHost(r.to_string()));
            }
            let hits: Vec<&ResolvedZone> = zones
                .iter()
                .filter(|zz| zz.hosts.iter().any(|x| x.name == r))
                .collect();
            match hits.len() {
                0 => Err(SpecError::MirrorUnknownHost(r.to_string())),
                1 => Ok((hits[0].name.clone(), r.to_string())),
                _ => Err(SpecError::MirrorAmbiguous(r.to_string())),
            }
        };

        let push = |zones: &[ResolvedZone], from: (String, String), to: (String, String), out: &mut Vec<ResolvedMirror>, seen: &mut BTreeSet<_>| -> Result<(), SpecError> {
            let (fz, fh) = from;
            let (tz, th) = to;
            let target = zones
                .iter()
                .find(|z| z.name == tz)
                .and_then(|z| z.hosts.iter().find(|h| h.name == th))
                .ok_or_else(|| SpecError::MirrorUnknownHost(format!("{tz}:{th}")))?;
            if target.role != Role::Analysis {
                return Err(SpecError::MirrorTargetNotAnalysis(
                    format!("{fz}:{fh}"),
                    format!("{tz}:{th}"),
                ));
            }
            let src_bridge = zones
                .iter()
                .find(|z| z.name == fz)
                .map(|z| z.bridge.clone())
                .ok_or_else(|| SpecError::MirrorUnknownHost(format!("{fz}:{fh}")))?;
            if seen.insert((fz.clone(), fh.clone(), tz.clone(), th.clone())) {
                out.push(ResolvedMirror {
                    from_zone: fz,
                    from_host: fh,
                    src_bridge,
                    to_zone: tz,
                    to_host: th,
                });
            }
            Ok(())
        };

        // 1) 显式连线
        for m in &self.mirrors {
            let from = find(&m.from)?;
            let to = find(&m.to)?;
            push(zones, from, to, &mut out, &mut seen)?;
        }

        // 2) host 级 role（旧语义）
        for z in zones {
            let sources: Vec<String> = z
                .hosts
                .iter()
                .filter(|h| h.mirror == MirrorRole::Source)
                .map(|h| h.name.clone())
                .collect();
            let targets: Vec<String> = z
                .hosts
                .iter()
                .filter(|h| h.mirror == MirrorRole::Target)
                .map(|h| h.name.clone())
                .collect();
            for s in &sources {
                for t in &targets {
                    push(
                        zones,
                        (z.name.clone(), s.clone()),
                        (z.name.clone(), t.clone()),
                        &mut out,
                        &mut seen,
                    )?;
                }
            }
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

/// 网关策略解析：把 `to` 拆成 ip + port，校验客户端名，检查端口重复。
///
/// 校验做在解析层而不是执行层：一条写错的映射如果等到 apply 才在 nft 里报
/// 语法错，用户看到的是"网关策略步骤失败"，而不是"第 2 条 pub 写错了"。
fn resolve_gateway(zone: &str, g: &GatewaySpec) -> Result<ResolvedGateway, SpecError> {
    let mut pubs = Vec::with_capacity(g.pubs.len());
    let mut used: BTreeSet<(String, u16)> = BTreeSet::new();
    for p in &g.pubs {
        let bad = || SpecError::BadPubTarget(zone.to_string(), p.to.clone());
        if p.port == 0 {
            return Err(bad());
        }
        let (ip, port) = match p.to.split_once(':') {
            Some((ip, po)) => (ip, po.parse::<u16>().map_err(|_| bad())?),
            None => (p.to.trim(), p.port),
        };
        // 只接受 IPv4 字面量：这里会直接进 nft 规则，主机名/网段会让语义
        // 变得不确定（DNS 在网关容器里，规则却在内核里）
        let mut it = ip.split('.');
        let four = (0..4)
            .map(|_| it.next().and_then(|x| x.parse::<u8>().ok()))
            .collect::<Option<Vec<u8>>>();
        if four.is_none() || it.next().is_some() || port == 0 {
            return Err(bad());
        }
        if !used.insert((p.proto.as_str().to_string(), p.port)) {
            return Err(SpecError::PubPortConflict(
                zone.to_string(),
                format!("{}/{}", p.proto.as_str(), p.port),
            ));
        }
        pubs.push(ResolvedPub {
            proto: p.proto,
            port: p.port,
            to_ip: ip.to_string(),
            to_port: port,
        });
    }

    let clients = if g.vpn.clients.is_empty() {
        vec!["lab".to_string()]
    } else {
        g.vpn.clients.clone()
    };
    for c in &clients {
        let ok = !c.is_empty()
            && c.len() <= 20
            && c
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_');
        if !ok {
            return Err(SpecError::BadVpnClient(c.clone()));
        }
    }

    Ok(ResolvedGateway {
        egress: g.egress.unwrap_or(true),
        pubs,
        vpn: ResolvedVpn {
            wireguard: g.vpn.wireguard,
            openvpn: g.vpn.openvpn,
            clients,
        },
    })
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

    // ───────────────────── 网关策略 ─────────────────────

    const GW: &str = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway:
      egress: true
      pub:
        - { proto: tcp, port: 2222, to: "10.10.1.20:22" }
        - { proto: udp, port: 5353, to: "10.10.1.30" }
      vpn: { wireguard: true, openvpn: false, clients: [lab, red] }
    hosts:
      gateway: { role: gateway }
      web: { role: server }
"#;

    #[test]
    fn gateway_policy_resolves_ports_and_clients() {
        let z = Spec::parse(GW).unwrap().resolve("vmbr0").unwrap();
        let g = z[0].gateway.as_ref().expect("有网关主机就该有策略");
        assert!(g.egress);
        assert_eq!(
            g.pubs,
            vec![
                ResolvedPub {
                    proto: Proto::Tcp,
                    port: 2222,
                    to_ip: "10.10.1.20".into(),
                    to_port: 22
                },
                ResolvedPub {
                    proto: Proto::Udp,
                    port: 5353,
                    to_ip: "10.10.1.30".into(),
                    to_port: 5353,
                },
            ],
            "省略端口时应回落到对外端口"
        );
        assert!(g.vpn.wireguard && !g.vpn.openvpn);
        assert_eq!(g.vpn.clients, vec!["lab", "red"]);
        assert_eq!(g.pubs[0].spec_string(), "tcp:2222:10.10.1.20:22");
    }

    #[test]
    fn gateway_defaults_when_unspecified() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway } }
"#;
        let z = Spec::parse(y).unwrap().resolve("vmbr0").unwrap();
        let g = z[0].gateway.as_ref().unwrap();
        assert!(g.egress, "默认出网开——靶机要装包");
        assert!(g.pubs.is_empty());
        assert!(!g.vpn.enabled());
    }

    #[test]
    fn gateway_block_without_gateway_host_is_rejected() {
        // 静默忽略等于配置撒谎：写了 pub 却没网关，映射永远不生效
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway: { pub: [{ proto: tcp, port: 80, to: "10.10.1.20" }] }
    hosts: { web: { role: server } }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve("vmbr0"),
            Err(SpecError::GatewayWithoutHost(_))
        ));
    }

    #[test]
    fn duplicate_pub_port_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway:
      pub:
        - { proto: tcp, port: 80, to: "10.10.1.20" }
        - { proto: tcp, port: 80, to: "10.10.1.30" }
    hosts: { gateway: { role: gateway } }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve("vmbr0"),
            Err(SpecError::PubPortConflict(_, _))
        ));
    }

    #[test]
    fn pub_target_must_be_ipv4() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway: { pub: [{ proto: tcp, port: 80, to: "web" }] }
    hosts: { gateway: { role: gateway } }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve("vmbr0"),
            Err(SpecError::BadPubTarget(_, _))
        ));
    }

    #[test]
    fn bad_vpn_client_name_is_rejected() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway: { vpn: { wireguard: true, clients: ["../../etc/passwd"] } }
    hosts: { gateway: { role: gateway } }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve("vmbr0"),
            Err(SpecError::BadVpnClient(_))
        ));
    }

    // ───────────────────── 镜像连线 ─────────────────────

    #[test]
    fn explicit_mirror_can_cross_zones() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      analysis: { role: analysis }
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts:
      gateway: { role: gateway }
      victim: { role: server }
mirrors:
  - { from: "sub:victim", to: "main:analysis" }
"#;
        let s = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        assert_eq!(s.mirrors.len(), 1);
        let m = &s.mirrors[0];
        assert_eq!((m.from_zone.as_str(), m.from_host.as_str()), ("sub", "victim"));
        assert_eq!((m.to_zone.as_str(), m.to_host.as_str()), ("main", "analysis"));
        assert_eq!(m.src_bridge, "labsub", "桥名是 lab + zone 名，不是 zone 名本身");
    }

    #[test]
    fn mirror_target_must_be_analysis() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      web: { role: server }
mirrors:
  - { from: gateway, to: web }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve_all("vmbr0"),
            Err(SpecError::MirrorTargetNotAnalysis(_, _))
        ));
    }

    #[test]
    fn mirror_refs_are_validated() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway }, analysis: { role: analysis } }
mirrors:
  - { from: "main:nope", to: "main:analysis" }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve_all("vmbr0"),
            Err(SpecError::MirrorUnknownHost(_))
        ));
    }

    #[test]
    fn host_level_mirror_roles_still_work_and_dedup() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      analysis: { role: analysis, mirror: target }
      s1: { role: server, mirror: source }
mirrors:
  - { from: s1, to: analysis }
"#;
        let s = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        assert_eq!(s.mirrors.len(), 1, "同一对连线只应落一条");
    }
}
