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
    /// 显式镜像连线（源主机 → 监控机）。host 级的 `mirror: source/target`
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
    /// apt 源地址（如清华源）。无外网的内网主机可指向内网 apt-cacher-ng。
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
    /// 网关是否做地址转换。**默认开**——实验网段里的主机要靠网关出网装包，
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
    /// 流量监控开关：**这个网络的流量要不要交给一台监控机**。
    ///
    /// 打开之后，本 zone 每台主机的接口都会被复制到监控机的对应网口
    /// （一个被监控网络 = 监控机上的一块网卡）。用 `targets` 可以只抓其中几台。
    ///
    /// 与顶层 `mirrors:` 的关系：这里是"整网采集"的开关（批量、默认关），
    /// `mirrors` 是"这台机器的流量交给那台监控机"的精确连线（可跨网络）。
    /// 两者最后收敛成同一批 `ResolvedMirror`，执行器不区分来源。
    pub monitor: Option<MonitorSpec>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostSpec>,
}

/// 一个 zone 的流量监控配置。
///
/// 两种形态，二选一（都写会报错）：
///
/// ```yaml
/// # 1) 新建一台监控机，挂在 wan 上
/// monitor: { name: probe, mgmt: wan, cores: 4, memory: 4096, disk: 32 }
///
/// # 2) 复用另一台已有的监控机（别的 zone 里已经有了）
/// monitor: { use: "core:probe" }
/// ```
///
/// 不写 `mgmt` 时监控机建在**本 zone** 里（和被抓的主机同网段，最省事）；
/// 写 `mgmt: wan` 表示直接挂到外部网桥（靠 DHCP 拿地址），适合监控边界流量。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorSpec {
    /// 新建监控机的名字，默认 `monitor`
    pub name: Option<String>,
    /// 改用已有主机：`zone:host`，或全规格唯一的 `host`
    #[serde(rename = "use")]
    pub existing: Option<String>,
    /// 新建时挂到哪个网络：`wan` 或某个 zone 名。默认本 zone。
    pub mgmt: Option<String>,
    /// 自定义主机位（内网，1–254）。不写 = 按角色默认位自动找空的。
    ///
    /// 挂在外网（`mgmt: wan`）时这里要写**完整地址**（`10.77.0.55` 或带
    /// 长度 `10.77.0.55/24`）；不写 = DHCP。
    pub ip: Option<String>,
    /// 自定义 CT 号（vmid）。不写 = 自动分配。
    pub vmid: Option<u32>,
    /// 只抓这些主机；不写 = 本网络全部主机（监控机自己的内网口也在内——
    /// 内网口 → 抓包口没有回环：采集流量走 mir 桥进 eth1，不会再出现在
    /// eth0 上）
    pub targets: Option<Vec<String>>,
    pub cores: Option<u32>,
    pub memory: Option<u32>,
    pub disk: Option<u32>,
    pub template: Option<String>,
    pub storage: Option<String>,
    pub template_storage: Option<String>,
    pub provisioner: Option<String>,
}

/// 挂在外部网桥上的合成 zone 名。
///
/// `uplink: wan` 里的 `wan` 是"上游是外网"的哨兵值，正常 zone 不会叫这个名
/// （`resolve` 会拒绝），所以这里可以安全地用它当"外网上的主机"的归属。
pub const WAN_ZONE: &str = "wan";
/// 没写 `monitor.name` 时的默认名字
pub const DEFAULT_MONITOR: &str = "monitor";

fn blank_host(role: Role) -> HostSpec {
    HostSpec {
        role,
        ip: None,
        ipv6: None,
        cores: None,
        memory: None,
        disk: None,
        template: None,
        storage: None,
        template_storage: None,
        timezone: None,
        lang: None,
        apt_mirror: None,
        packages: None,
        bridge_mgmt_ip: None,
        node: None,
        dns: None,
        ssh_keys: None,
        password: None,
        vmid: None,
        provisioner: None,
        mirror: MirrorRole::None,
    }
}

/// 合成 zone 的骨架（只用于"监控机挂在外网"这一种情形）。
fn synthetic_zone(uplink: &str, v4: &str, bridge: &str) -> ZoneSpec {
    ZoneSpec {
        uplink: uplink.to_string(),
        description: None,
        v4: v4.to_string(),
        v6: V6Mode::Off,
        nat: false,
        bridge: Some(bridge.to_string()),
        cores: None,
        memory: None,
        disk: None,
        template: None,
        storage: None,
        template_storage: None,
        timezone: None,
        lang: None,
        apt_mirror: None,
        packages: None,
        bridge_mgmt_ip: None,
        node: None,
        dns: None,
        ssh_keys: None,
        password: None,
        gateway: None,
        monitor: None,
        hosts: BTreeMap::new(),
    }
}

/// 网关策略（声明式）。运行时改这些字段会**写回规格**，不在容器里留影子状态。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySpec {
    /// 内网是否允许出网。默认 true（内网主机要装包）。
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
    /// 流量监控：接管其它主机接口的镜像流量。
    ///
    /// 老规格里这个名字叫 `analysis`，保留成别名——改名是为了去掉"分析/靶场"
    /// 这类域-specific 的说法，不该让已经存在的规格因此解析失败。
    #[serde(alias = "analysis", alias = "Analysis")]
    Monitor,
    /// 网内主机。对网络来说服务端和终端都是主机，不值得区分——区分只会让
    /// 用户在"这台到底是 server 还是 client"上浪费时间。老规格的
    /// `server` / `client` 保留成别名照常解析，落盘统一写成 `host`。
    #[serde(alias = "server", alias = "Server", alias = "client", alias = "Client")]
    Host,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Gateway => "gateway",
            Role::Monitor => "monitor",
            Role::Host => "host",
        }
    }

    /// 默认主机位（用户没写 `ip` 时用）
    pub fn default_host_octet(self) -> u8 {
        match self {
            Role::Gateway => 1,
            Role::Monitor => 10,
            Role::Host => 20,
        }
    }

    /// 中文名。前端也有一份（`ROLE_LABEL`），两边必须一致；这里给后端
    /// 的提示文案用（比如"目标必须是 monitor（流量监控）"）。
    pub fn label(self) -> &'static str {
        match self {
            Role::Gateway => "网关",
            Role::Monitor => "流量监控",
            Role::Host => "主机",
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
    /// 这台监控机要抓哪些网络的流量。一个网络一块抓包网卡，按 zone 名
    /// 排序后从 eth1 起编号（eth0 永远是管理口）。
    ///
    /// 只有 `role: monitor` 的主机会有内容；其余主机是空列表。计划层据此
    /// 决定 `pct create` 要给几块网卡，拓扑据此画"哪块口接哪个网络"。
    pub capture: Vec<String>,
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
    /// 这个 zone 是不是"外网"合成 zone（监控机直接挂在 wan 桥上的情形）。
    /// 真 zone 才建桥、配地址；合成 zone 的网桥是 PVE 本来就有的，绝不能删。
    pub external: bool,
    /// 本 zone 的监控机（主机名）。没有开监控时为 None。
    ///
    /// 与 [`Self::monitor_created`] 的区别：`use`（复用别的 zone 的监控机）
    /// 也会写这里——抓包连线要它；但销毁语义只认"谁建的谁负责删"。
    pub monitor: Option<String>,
    /// 本 zone **新建**出来的监控机主机名（`monitor: {name: ...}` 才有）。
    /// `use` 复用的不写——销毁 zone 时不该顺手把别人在用的监控机删了。
    pub monitor_created: Option<String>,
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
    /// 抓包落在监控机的哪块网卡（eth1、eth2……）。一个被监控网络一块，
    /// 由 `from_zone` 在监控机的 capture 列表里的位置决定。
    pub to_iface: String,
    /// 这块抓包网卡所在的两层桥名（`mir{zone}`）。每条镜像一个独立广播域，
    /// 不同网络的流量不会串到同一块网卡上。
    pub capture_bridge: String,
}

/// 抓包桥的名字：`mir` + zone 名。IFNAMSIZ 是 15，zone 名最长 10 → 13。
pub fn capture_bridge(zone: &str) -> String {
    format!("mir{}", sanitize(zone))
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
    #[error("镜像连线 `{0}` → `{1}`：目标必须是 role: monitor（流量监控，只有它挂抓包口）")]
    MirrorTargetNotAnalysis(String, String),
    #[error("镜像连线 `{0}` → `{1}`：把自己镜像到自己没有意义（会生成 100-100 这种空转单元）")]
    MirrorSelfLoop(String, String),
    #[error("zone 名 `wan` 是保留字（表示外网），换一个名字")]
    ZoneNameReserved(String),
    #[error("zone `{0}` 的 monitor 里 `name` 与 `use` 只能写一个")]
    MonitorBoth(String),
    #[error("zone `{0}` 的监控机 `{1}` 与已有主机重名——给它换个名字（monitor.name）")]
    MonitorNameConflict(String, String),
    #[error("监控机 `{0}` 不存在。可用的监控机：{1}。`use` 可以写 `zone:host` 指向一台 role 为 monitor 的主机，也可以写 `zone:名字` 引用某个网络的新建声明（monitor.name）——监控机实际建在哪个网络由该声明的 mgmt 决定，引用时不用管")]
    MonitorUnknown(String, String),
    #[error("监控机 `{0}` 有歧义：多个 zone 里都有这个主机名，请写成 zone:host")]
    MonitorAmbiguous(String),
    #[error("`{0}` 的 role 是 {1}，不是 monitor（流量监控）——不能当抓包目标")]
    MonitorTargetWrongRole(String, &'static str),
    #[error("zone `{0}` 的 monitor.targets 里 `{1}` 不存在")]
    MonitorTargetUnknown(String, String),
}

// ═════════════════════ 解析辅助 ═════════════════════

/// 某个 zone 里有没有这个名字的主机——包括那些刚由 `monitor.name`
/// 合成出来、还没进入任何 map 的监控机。
fn host_named(
    spec: &Spec,
    extra: &BTreeMap<String, BTreeMap<String, HostSpec>>,
    zone: &str,
    host: &str,
) -> bool {
    spec.zones
        .get(zone)
        .map(|z| z.hosts.contains_key(host))
        .unwrap_or(false)
        || extra
            .get(zone)
            .map(|m| m.contains_key(host))
            .unwrap_or(false)
}

/// 把 `monitor.use` 解析成 `(zone, host)`，并检查它确实是监控机角色。
///
/// 允许裸主机名：一份规格里只有一台同名主机时不用写 zone 前缀。有歧义就
/// 报错而不是"挑第一个"——挑错了是把流量送到别的机器上，不报错等于骗人。
fn lookup_monitor(zones: &[ResolvedZone], r: &str) -> Result<(String, String), SpecError> {
    let (mz, mh, role) = if let Some((z, h)) = r.split_once(':') {
        let zz = zones
            .iter()
            .find(|x| x.name == z)
            .ok_or_else(|| monitor_unknown(r, zones))?;
        let hh = zz
            .hosts
            .iter()
            .find(|x| x.name == h)
            .ok_or_else(|| monitor_unknown(r, zones))?;
        (zz.name.clone(), hh.name.clone(), hh.role)
    } else {
        let hits: Vec<(&ResolvedZone, &ResolvedHost)> = zones
            .iter()
            .flat_map(|z| z.hosts.iter().filter(|h| h.name == r).map(move |h| (z, h)))
            .collect();
        match hits.len() {
            0 => return Err(monitor_unknown(r, zones)),
            1 => (
                hits[0].0.name.clone(),
                hits[0].1.name.clone(),
                hits[0].1.role,
            ),
            _ => return Err(SpecError::MonitorAmbiguous(r.to_string())),
        }
    };
    if role != Role::Monitor {
        return Err(SpecError::MonitorTargetWrongRole(
            format!("{mz}:{mh}"),
            role.label(),
        ));
    }
    Ok((mz, mh))
}

/// "监控机不存在"的报错要把**现在到底有哪些**列出来——用户看到 `lab:monitor`
/// 不存在时往往手里正攥着一台 monitor 名字的机器，不列清单他就以为系统瞎了。
/// 清单来源：各 zone 里 role 为 monitor 的主机 + 新建声明（monitor.name）。
fn monitor_unknown(r: &str, zones: &[ResolvedZone]) -> SpecError {
    let mut found: Vec<String> = Vec::new();
    for z in zones {
        for h in &z.hosts {
            if h.role == Role::Monitor {
                found.push(format!("{}:{}", z.name, h.name));
            }
        }
    }
    SpecError::MonitorUnknown(
        r.to_string(),
        if found.is_empty() {
            "（一台都没有——先在某台 zone 的 monitor 里新建或声明一台）".to_string()
        } else {
            found.join("、")
        },
    )
}

/// 单个 zone 的三层合并。抽成函数是因为"合成 zone"（监控机挂在外网）要
/// 走完全相同的合并规则——复制一份的下场是两边慢慢漂开。
fn resolve_zone(
    spec: &Spec,
    wan_bridge: &str,
    zone_name: &str,
    z: &ZoneSpec,
    external: bool,
    extra_hosts: BTreeMap<String, HostSpec>,
    used_vmids: &mut HashMap<u32, String>,
) -> Result<ResolvedZone, SpecError> {
    if zone_name.chars().count() > 10 {
        return Err(SpecError::ZoneNameTooLong(zone_name.to_string()));
    }

    let bridge = if external {
        wan_bridge.to_string()
    } else {
        z.bridge
            .clone()
            .unwrap_or_else(|| format!("lab{}", sanitize(zone_name)))
    };

    let (v4_net, v4_prefix) = if external {
        (String::new(), 0u8)
    } else {
        parse_v4_cidr(zone_name, &z.v4)?
    };

    let uplink = if external || z.uplink == "wan" {
        None
    } else {
        Some(z.uplink.clone())
    };
    if let Some(up) = &uplink {
        if !spec.zones.contains_key(up) {
            return Err(SpecError::UnknownUplink(zone_name.to_string(), up.clone()));
        }
    }
    let uplink_bridge = match &uplink {
        None => Some(wan_bridge.to_string()),
        Some(up) => Some(
            spec.zones[up]
                .bridge
                .clone()
                .unwrap_or_else(|| format!("lab{}", sanitize(up))),
        ),
    };

    let v6_prefix = if external {
        None
    } else {
        match &z.v6 {
            V6Mode::Off => None,
            V6Mode::Prefix(p) => Some(p.clone()),
            // 从 zone 名确定性派生：同一份规格每次得到同一个前缀，不需要持久化
            V6Mode::Auto => Some(ula_prefix(zone_name)),
        }
    };

    // 声明的主机 + 由 monitor 合成出来的监控机。两者已经在 resolve_all
    // 里查过重名，这里直接接上。
    let mut hosts_in: Vec<(&String, &HostSpec)> = z.hosts.iter().collect();
    hosts_in.extend(extra_hosts.iter());

    let mut hosts = Vec::with_capacity(hosts_in.len());
    // 同一 zone 内主机号必须唯一。用手里的 HashSet 而不是事后扫一遍，
    // 是因为要能报出"谁跟谁撞了"。
    let mut used_octets: HashMap<u8, String> = HashMap::new();
    for (host_name, h) in hosts_in {
        let cores = h
            .cores
            .or(z.cores)
            .or(spec.defaults.cores)
            .ok_or_else(|| SpecError::MissingField(zone_name.to_string(), "cores".into()))?;
        let memory = h
            .memory
            .or(z.memory)
            .or(spec.defaults.memory)
            .ok_or_else(|| SpecError::MissingField(zone_name.to_string(), "memory".into()))?;
        let disk = h
            .disk
            .or(z.disk)
            .or(spec.defaults.disk)
            .ok_or_else(|| SpecError::MissingField(zone_name.to_string(), "disk".into()))?;
        let template = h
            .template
            .clone()
            .or_else(|| z.template.clone())
            .or_else(|| spec.defaults.template.clone())
            .ok_or_else(|| SpecError::MissingField(zone_name.to_string(), "template".into()))?;
        let storage = h
            .storage
            .clone()
            .or_else(|| z.storage.clone())
            .or_else(|| spec.defaults.storage.clone())
            .ok_or_else(|| SpecError::MissingField(zone_name.to_string(), "storage".into()))?;
        // 模板存储：显式指定 > 跟随 rootfs 存储。这里**不做**"存储是否
        // 支持 vztmpl"的判断——那要查 PVE，是 doctor 的职责，解析层只做合并。
        let template_storage = h
            .template_storage
            .clone()
            .or_else(|| z.template_storage.clone())
            .or_else(|| spec.defaults.template_storage.clone())
            .unwrap_or_else(|| storage.clone());
        let timezone = h
            .timezone
            .clone()
            .or_else(|| z.timezone.clone())
            .or_else(|| spec.defaults.timezone.clone());
        let lang = h
            .lang
            .clone()
            .or_else(|| z.lang.clone())
            .or_else(|| spec.defaults.lang.clone());
        let apt_mirror = h
            .apt_mirror
            .clone()
            .or_else(|| z.apt_mirror.clone())
            .or_else(|| spec.defaults.apt_mirror.clone());
        // Option 链而不是"空列表当未设置"：`packages: []` 必须能表达
        // "显式不要包"——无外网的主机继承 defaults 的包清单只会装不上。
        let packages = h
            .packages
            .clone()
            .or_else(|| z.packages.clone())
            .or_else(|| spec.defaults.packages.clone())
            .unwrap_or_default();

        // 主机号：写了就校验，没写就按角色默认位往后找第一个空的。
        // 早先的实现直接用角色默认位，**同一个 zone 里两台 server 会拿到
        // 同一个 .20**，两台机器抢一个 IP，而 YAML 看起来完全正常。
        let ipv4 = if external {
            // 外网上的主机默认 DHCP；写了完整地址（`10.77.0.55[/24]`）就是
            // 静态。这里不认"末位"写法——外网没有本规格的网段可拼，末位
            // 拼不出合法地址，宁可报错也不猜。
            match &h.ip {
                None => "dhcp".to_string(),
                Some(s) => {
                    let (addr, len) = match s.split_once('/') {
                        Some((a, l)) => (a, Some(l)),
                        None => (s.as_str(), None),
                    };
                    let v: std::net::Ipv4Addr = addr.parse().map_err(|_| {
                        SpecError::BadCidr(
                            zone_name.to_string(),
                            format!("外网主机地址要写完整 IPv4（如 10.77.0.55/24），写了 {s}"),
                        )
                    })?;
                    match len {
                        None => v.to_string(),
                        Some(l) => {
                            let n: u8 = l.parse().map_err(|_| {
                                SpecError::BadCidr(zone_name.to_string(), format!("前缀长度 {l}"))
                            })?;
                            format!("{v}/{n}")
                        }
                    }
                }
            }
        } else {
            let host_octet = match &h.ip {
                Some(s) => s
                    .parse::<u32>()
                    .ok()
                    .filter(|v| (1..=254).contains(v))
                    .ok_or_else(|| {
                        SpecError::BadCidr(zone_name.to_string(), format!("ip {s}"))
                    })? as u8,
                None => {
                    let mut o = h.role.default_host_octet();
                    while used_octets.contains_key(&o) {
                        o = o.saturating_add(1);
                        if o > 254 {
                            return Err(SpecError::IpOutOfRange(
                                zone_name.to_string(),
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
                    zone_name.to_string(),
                    host_name.clone(),
                    format!("{v4_net}.{host_octet}"),
                    z.v4.clone(),
                ));
            }
            if let Some(prev) = used_octets.insert(host_octet, host_name.clone()) {
                return Err(SpecError::DuplicateIp(
                    zone_name.to_string(),
                    host_octet,
                    prev,
                    host_name.clone(),
                ));
            }
            format!("{}.{}/{}", v4_net, host_octet, v4_prefix)
        };

        let qualified = format!("{}:{}", zone_name, host_name);
        if let Some(v) = h.vmid {
            if let Some(prev) = used_vmids.insert(v, qualified.clone()) {
                return Err(SpecError::VmidConflict(v, prev, qualified));
            }
        }

        hosts.push(ResolvedHost {
            name: host_name.clone(),
            zone: zone_name.to_string(),
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
                .or_else(|| spec.defaults.node.clone()),
            dns: h
                .dns
                .clone()
                .or_else(|| z.dns.clone())
                .or_else(|| spec.defaults.dns.clone())
                .unwrap_or_default(),
            ssh_keys: h
                .ssh_keys
                .clone()
                .or_else(|| z.ssh_keys.clone())
                .or_else(|| spec.defaults.ssh_keys.clone())
                .unwrap_or_default(),
            password: h
                .password
                .clone()
                .or_else(|| z.password.clone())
                .or_else(|| spec.defaults.password.clone()),
            ipv4,
            ipv6_prefix: v6_prefix.clone(),
            provisioner: h.provisioner.clone(),
            mirror: h.mirror,
            // 抓包口在 resolve_all 的最后一步回填：要等所有连线齐了才知道
            // 这台监控机一共接了几个网络
            capture: Vec::new(),
        });
    }

    // 宿主管理地址：显式配置优先，否则取网段 .254（网关占 .1、
    // 主机从 .2 起分配，.254 实际不会冲突）。外部桥由 PVE 管，不动它。
    let mgmt = if external {
        String::new()
    } else {
        z.bridge_mgmt_ip.clone().unwrap_or_else(|| {
            let (net, prefix) = z.v4.split_once('/').unwrap_or((&z.v4, "24"));
            let mut parts: Vec<&str> = net.split('.').collect();
            if parts.len() == 4 {
                parts[3] = "254";
                format!("{}/{}", parts.join("."), prefix)
            } else {
                format!("{net}/254")
            }
        })
    };

    // ── 网关策略 ──
    // 没有网关主机时，`gateway:` 里写的任何东西都没有落地的地方。
    // 静默忽略会让"我明明配了端口映射"变成一场排查——这里直接拒绝。
    let has_gateway = hosts.iter().any(|h| h.role == Role::Gateway);
    let gateway = if !has_gateway {
        if z.gateway.is_some() {
            return Err(SpecError::GatewayWithoutHost(zone_name.to_string()));
        }
        None
    } else {
        Some(match &z.gateway {
            Some(g) => resolve_gateway(zone_name, g)?,
            None => ResolvedGateway {
                egress: true,
                pubs: Vec::new(),
                vpn: ResolvedVpn::default(),
            },
        })
    };

    Ok(ResolvedZone {
        name: zone_name.to_string(),
        bridge,
        uplink,
        uplink_bridge,
        v4_cidr: z.v4.clone(),
        bridge_mgmt_ip: mgmt,
        v6_prefix,
        nat: z.nat,
        gateway,
        external,
        monitor: None,
        monitor_created: None,
        hosts,
    })
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
        // `wan` 是 uplink 的哨兵值，又是合成 zone 的名字。真 zone 不能叫这个名，
        // 否则"这个 zone 的上游是谁"会变得无法表述。
        if self.zones.contains_key(WAN_ZONE) {
            return Err(SpecError::ZoneNameReserved(WAN_ZONE.to_string()));
        }
        let order = self.topo_order()?;

        // ── 第一步：把"新建监控机"落成主机 ──
        //
        // 先做这一步，是因为监控机可能被建在**另一个** zone（甚至外网）里，
        // 而下面按 zone 的合并是逐个做的。先塞进 `extra`，再走同一套三层合并，
        // 就不必为"合成出来的主机"写第二份解析逻辑。
        let mut extra: BTreeMap<String, BTreeMap<String, HostSpec>> = BTreeMap::new();
        // (被监控 zone, 只抓哪几台, 监控机 zone, 监控机主机名)
        let mut links: Vec<(String, Option<Vec<String>>, String, String)> = Vec::new();

        for (zone_name, z) in &self.zones {
            let Some(m) = &z.monitor else { continue };
            if m.existing.is_some() {
                // `use` 与 `name`/`mgmt` 是互斥的两条路：既说"用现成的"又说
                // "新建在哪"，只能是我们理解错了一个，不如直接报错。
                if m.name.is_some() || m.mgmt.is_some() {
                    return Err(SpecError::MonitorBoth(zone_name.clone()));
                }
                continue;
            }
            let name = m
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_MONITOR.to_string());
            let mgmt = m.mgmt.clone().unwrap_or_else(|| zone_name.clone());
            if mgmt != WAN_ZONE && !self.zones.contains_key(&mgmt) {
                return Err(SpecError::UnknownUplink(zone_name.clone(), mgmt));
            }
            if host_named(self, &extra, &mgmt, &name) {
                return Err(SpecError::MonitorNameConflict(mgmt, name));
            }
            let h = HostSpec {
                ip: m.ip.clone(),
                vmid: m.vmid,
                cores: m.cores,
                memory: m.memory,
                disk: m.disk,
                template: m.template.clone(),
                storage: m.storage.clone(),
                template_storage: m.template_storage.clone(),
                provisioner: m.provisioner.clone(),
                ..blank_host(Role::Monitor)
            };
            extra
                .entry(mgmt.clone())
                .or_default()
                .insert(name.clone(), h);
            links.push((zone_name.clone(), m.targets.clone(), mgmt, name));
        }

        // ── 第二步：逐 zone 合并（含被塞进来的监控机） ──
        let mut out: Vec<ResolvedZone> = Vec::with_capacity(order.len() + 1);
        let mut used_vmids: HashMap<u32, String> = HashMap::new();
        for zone_name in &order {
            let z = &self.zones[zone_name];
            let eh = extra.remove(zone_name).unwrap_or_default();
            out.push(resolve_zone(
                self,
                wan_bridge,
                zone_name,
                z,
                false,
                eh,
                &mut used_vmids,
            )?);
        }
        if let Some(hosts) = extra.remove(WAN_ZONE) {
            let mut z = synthetic_zone(WAN_ZONE, "", wan_bridge);
            z.hosts = hosts;
            // 外网上的主机没有任何依赖，排最前，读计划时先看到它
            out.insert(
                0,
                resolve_zone(
                    self,
                    wan_bridge,
                    WAN_ZONE,
                    &z,
                    true,
                    BTreeMap::new(),
                    &mut used_vmids,
                )?,
            );
        }

        // ── 第三步：`use`（复用已有监控机）要等 zone 解析完才能查 ──
        //
        // 除了直接指向一台 role: monitor 的主机，`use` 还允许引用**某个
        // zone 的新建声明**（`lab:monitor` = lab 的 monitor.name 声明）。
        // 这不是多余的语法糖：监控机实际落在哪个网络由声明的 `mgmt` 决定
        // （比如 mgmt: wan 时它建在 wan 里），引用者根本不该知道这些——
        // 按声明所在的 zone 写就行。没有这条回退，前端"复用已有"下拉里
        // 的声明选项保存必报"不存在"，用户会觉得系统瞎了。
        let mut decls: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
        for (zone_name, z) in &self.zones {
            if let Some(m) = &z.monitor {
                if m.existing.is_none() {
                    let name = m.name.clone().unwrap_or_else(|| DEFAULT_MONITOR.to_string());
                    let mgmt = m.mgmt.clone().unwrap_or_else(|| zone_name.clone());
                    decls.insert((zone_name.clone(), name.clone()), (mgmt, name));
                }
            }
        }
        for (zone_name, z) in &self.zones {
            let Some(m) = &z.monitor else { continue };
            let Some(r) = &m.existing else { continue };
            let (mz, mh) = match lookup_monitor(&out, r) {
                Ok(v) => v,
                Err(e) => match r.split_once(':') {
                    Some((dz, dh)) => decls
                        .get(&(dz.to_string(), dh.to_string()))
                        .cloned()
                        .ok_or(e)?,
                    None => return Err(e),
                },
            };
            links.push((zone_name.clone(), m.targets.clone(), mz, mh));
        }

        // ── 第四步：开关 → 连线 ──
        //
        // 语义就是"这个网络里每台主机的接口都复制一份给监控机"——包括
        // 监控机**自己的内网口**：内网口 → 抓包口没有回环，采集流量走 mir
        // 桥进 eth1，不会再出现在 eth0 上，所以 eth0 的流量就是普通的本机
        // 流量，抓了不重复、不放大。别的监控机不抓（那件事应该由那台机器
        // 的 `monitor` 配置表达）。
        let mut forced: Vec<(String, String)> = Vec::new();
        let mut zone_monitor: BTreeMap<String, String> = BTreeMap::new();
        for (cz, targets, mz, mh) in &links {
            let Some(z) = out.iter().find(|x| x.name == *cz) else {
                continue;
            };
            let to = format!("{mz}:{mh}");
            zone_monitor.insert(cz.clone(), mh.clone());
            match targets {
                Some(list) => {
                    for t in list {
                        let h = z.hosts.iter().find(|h| &h.name == t).ok_or_else(|| {
                            SpecError::MonitorTargetUnknown(cz.clone(), t.clone())
                        })?;
                        if h.role == Role::Monitor && !(mz == cz && mh.as_str() == t.as_str()) {
                            continue;
                        }
                        forced.push((format!("{cz}:{}", h.name), to.clone()));
                    }
                }
                None => {
                    for h in &z.hosts {
                        // 别的监控机跳过；本机的连线由下面的自环放行处理
                        if h.role == Role::Monitor && !(mz == cz && mh.as_str() == h.name) {
                            continue;
                        }
                        forced.push((format!("{cz}:{}", h.name), to.clone()));
                    }
                }
            }
        }

        let mut mirrors = self.resolve_mirrors(&out, &forced)?;

        // 谁新建了监控机，记在谁的头上——destroy 靠它区分"删自己建的"
        // 和"复用别人的"。
        for (cz, z) in &self.zones {
            if let Some(m) = &z.monitor {
                if m.existing.is_none() {
                    if let Some(rz) = out.iter_mut().find(|x| &x.name == cz) {
                        rz.monitor_created =
                            Some(m.name.clone().unwrap_or_else(|| DEFAULT_MONITOR.to_string()));
                    }
                }
            }
        }

        // ── 第五步：回填抓包口 ──
        //
        // 一个监控机可能接多个被监控网络，每个网络一块网卡。编号按 zone 名
        // 排序（不是按出现顺序）——pct 参数、拓扑标签、镜像脚本三处都从这
        // 一个顺序来，否则同一份规格在三个地方会给出三种 eth 编号。
        let mut caps: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for m in &mirrors {
            caps.entry((m.to_zone.clone(), m.to_host.clone()))
                .or_default()
                .insert(m.from_zone.clone());
        }
        for m in &mut mirrors {
            let idx = caps
                .get(&(m.to_zone.clone(), m.to_host.clone()))
                .and_then(|s| s.iter().position(|z| z == &m.from_zone))
                .map(|i| i + 1)
                .unwrap_or(1);
            m.to_iface = format!("eth{idx}");
            m.capture_bridge = capture_bridge(&m.from_zone);
        }
        for z in &mut out {
            z.monitor = zone_monitor.get(&z.name).cloned();
            for h in &mut z.hosts {
                if let Some(s) = caps.get(&(z.name.clone(), h.name.clone())) {
                    h.capture = s.iter().cloned().collect();
                }
            }
        }

        Ok(ResolvedSpec {
            zones: out,
            mirrors,
        })
    }

    /// 镜像连线：监控开关（`forced`）→ 显式 `mirrors:` → host 级 `mirror:`
    /// 三层按 `zone:host` 去重。执行器只看到一张确定的连线表，不关心来源。
    fn resolve_mirrors(
        &self,
        zones: &[ResolvedZone],
        forced: &[(String, String)],
    ) -> Result<Vec<ResolvedMirror>, SpecError> {
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

        let push = |from: (String, String),
                    to: (String, String),
                    out: &mut Vec<ResolvedMirror>,
                    seen: &mut BTreeSet<_>|
         -> Result<(), SpecError> {
            let (fz, fh) = from;
            let (tz, th) = to;
            let target = zones
                .iter()
                .find(|z| z.name == tz)
                .and_then(|z| z.hosts.iter().find(|h| h.name == th))
                .ok_or_else(|| SpecError::MirrorUnknownHost(format!("{tz}:{th}")))?;
            // 自环：把某台主机镜像到它自己。单元名会变成 `lab-100-100`，
            // 拿到的是一个"抓自己 eth 再灌回自己"的空转单元——静默装上也
            // 只会浪费 CPU，不如在解析阶段就拦掉。
            //
            // **唯一例外**：目标是监控机（fz==tz && fh==th 只可能是监控机
            // 监控本网段时"把自己的内网口也抓上"这一条）。这不是回环：源是
            // 监控机的 eth0（内网口），汇是它的 eth1（抓包口），采集流量走
            // mir 桥进 eth1，不会再被 eth0 上的规则复制。
            if fz == tz && fh == th && target.role != Role::Monitor {
                return Err(SpecError::MirrorSelfLoop(
                    format!("{fz}:{fh}"),
                    format!("{tz}:{th}"),
                ));
            }
            if target.role != Role::Monitor {
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
                    // 网口编号要等所有连线齐了才能定（见 resolve_all 第五步）
                    to_iface: String::new(),
                    capture_bridge: String::new(),
                });
            }
            Ok(())
        };

        // 0) 监控开关展开出来的连线
        for (from, to) in forced {
            let f = find(from)?;
            let t = find(to)?;
            push(f, t, &mut out, &mut seen)?;
        }

        // 1) 显式连线
        for m in &self.mirrors {
            let from = find(&m.from)?;
            let to = find(&m.to)?;
            push(from, to, &mut out, &mut seen)?;
        }

        // 2) host 级 role（旧语义，保留兼容）
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
        for zone in ["main", "sub", "a", "zone-with-long-name", "zone-cn"] {
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
        assert!(z[0].nat, "不写 nat 时默认开——否则内网主机出不了网");
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
        assert!(g.egress, "默认出网开——内网主机要装包");
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
    fn a_host_cannot_be_mirrored_into_itself() {
        // 普通主机抓自己毫无意义（也没有 ethN 能当汇），解析阶段就拦。
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway }, web: { role: server } }
mirrors:
  - { from: web, to: web }
"#;
        assert!(matches!(
            Spec::parse(y).unwrap().resolve_all("vmbr0"),
            Err(SpecError::MirrorSelfLoop(_, _))
        ));
    }

    #[test]
    fn monitor_captures_its_own_inner_iface() {
        // 监控机自己的内网口也在镜像范围里（内网口 → 抓包口没有回环：
        // 采集流量走 mir 桥进 eth1，不会再出现在 eth0 上）。
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    monitor: { name: mon }
    hosts: { gateway: { role: gateway }, web: { role: server } }
"#;
        let r = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let self_line = r
            .mirrors
            .iter()
            .find(|m| m.from_host == "mon" && m.to_host == "mon");
        assert!(self_line.is_some(), "监控机自己的内网口应被镜像");
        // 网关也在镜像范围内
        assert!(r.mirrors.iter().any(|m| m.from_host == "gateway"));
    }

    #[test]
    fn monitor_custom_ip_and_vmid() {
        // ip / vmid 都要能自定义；挂外网时 ip 是完整地址，留空 = DHCP
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    monitor: { name: mon, ip: 55, vmid: 900 }
    hosts: { gateway: { role: gateway } }
  dmz:
    uplink: wan
    v4: 10.10.2.0/24
    monitor: { name: edge-mon, mgmt: wan }
"#;
        let r = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let mon = &r
            .zones
            .iter()
            .find(|z| z.name == "main")
            .unwrap()
            .hosts
            .iter()
            .find(|h| h.name == "mon")
            .unwrap();
        assert_eq!(mon.vmid, Some(900));
        assert!(mon.ipv4.starts_with("10.10.1.55/"), "{}", mon.ipv4);
        let edge = &r
            .zones
            .iter()
            .find(|z| z.name == "wan")
            .unwrap()
            .hosts
            .iter()
            .find(|h| h.name == "edge-mon")
            .unwrap();
        assert_eq!(edge.ipv4, "dhcp");
    }

    #[test]
    fn monitor_external_static_ip() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  dmz:
    uplink: wan
    v4: 10.10.2.0/24
    monitor: { name: m, mgmt: wan, ip: "10.77.0.55/24" }
"#;
        let r = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let m = &r.zones[0].hosts[0];
        assert_eq!(m.ipv4, "10.77.0.55/24");
    }

    #[test]
    fn monitor_use_can_reference_a_declaration() {
        // 回归：l2 复用 lab 的新建声明，而监控机实际建在 wan（mgmt: wan）。
        // `lab:monitor` 这种写法必须能解析——按声明所在的 zone 写引用，
        // 不该要求用户知道 mgmt 把机器带到哪儿去了。
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  lab:
    uplink: wan
    v4: 10.77.0.0/24
    monitor: { name: monitor, mgmt: wan }
    hosts: { gw: { role: gateway }, t1: { role: server } }
  l2:
    uplink: lab
    v4: 10.30.1.0/24
    monitor: { use: "lab:monitor" }
    hosts: { gateway: { role: gateway }, w1: { role: client } }
"#;
        let r = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let l2 = r.zones.iter().find(|z| z.name == "l2").unwrap();
        assert_eq!(l2.monitor.as_deref(), Some("monitor"));
        assert!(
            r.mirrors
                .iter()
                .any(|m| m.from_zone == "l2" && m.to_zone == "wan" && m.to_host == "monitor"),
            "l2 的镜像应指向 wan 里那台 monitor: {:?}",
            r.mirrors
        );
        // lab 自己的新建声明也照常连线（含监控机自镜像）
        assert!(r.mirrors.iter().any(|m| m.from_zone == "lab" && m.to_zone == "wan"));
    }

    #[test]
    fn monitor_use_to_unknown_declaration_still_fails() {
        // 引用一个不存在的声明要报错，且报错要列出现有的监控机
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  lab:
    uplink: wan
    v4: 10.77.0.0/24
    monitor: { name: monitor, mgmt: wan }
    hosts: { gw: { role: gateway } }
  l2:
    uplink: lab
    v4: 10.30.1.0/24
    monitor: { use: "lab:probe" }
    hosts: { gateway: { role: gateway } }
"#;
        let err = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap_err();
        assert!(matches!(err, SpecError::MonitorUnknown(_, _)), "实际: {err}");
        let msg = err.to_string();
        assert!(msg.contains("wan:monitor"), "报错要列出 wan:monitor: {msg}");
    }

    #[test]
    fn server_and_client_roles_resolve_to_host() {
        // 角色 Simplification：server / client 都是 host（老写法保留成别名）
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      a: { role: server }
      b: { role: client }
      c: { role: host }
"#;
        let z = Spec::parse(y).unwrap().resolve("vmbr0").unwrap();
        let roles: Vec<&str> = z[0].hosts.iter().map(|h| h.role.as_str()).collect();
        assert_eq!(roles, vec!["host", "host", "host"]);
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
