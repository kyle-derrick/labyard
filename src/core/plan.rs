//! 把解析后的 zone 展开成一串可执行步骤。
//!
//! 步骤按**阶段**分组，阶段内按 zone 拓扑序、zone 内按主机名排序，保证：
//! 同样的输入永远产出同样的顺序（否则 diff 每次都变，幂等就没法看）。
//!
//! 依赖不是靠"隐式顺序"保证，而是显式写在 `deps` 里——这样执行器可以并行
//! 阶段内无依赖的步骤，也可以只重跑失败的后半段。

use crate::core::spec::{
    capture_bridge, ResolvedMirror, ResolvedPub, ResolvedVpn, ResolvedZone, Role,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// 执行阶段。顺序即语义：前面的阶段必须先完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Bridge,
    Template,
    Container,
    Net,
    Bootstrap,
    /// 网关策略（nft 端口映射/出网、VPN）。排在 bootstrap 之后——
    /// openvpn/wireguard 的包要在 bootstrap 里装好。
    Gateway,
    Mirror,
    Snapshot,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Bridge => "bridge",
            Phase::Template => "template",
            Phase::Container => "container",
            Phase::Net => "net",
            Phase::Bootstrap => "bootstrap",
            Phase::Gateway => "gateway",
            Phase::Mirror => "mirror",
            Phase::Snapshot => "snapshot",
        }
    }
}

/// 运行时脚本的"模板版本"。
///
/// 步骤哈希只看输入（规格里的参数），不看生成器代码——生成器修好了，
/// 已完成的步骤仍然是 done，容器里跑的还是老的脚本。这在"改一条规则"时
/// 很隐蔽：改了 gateway.rs，apply 全跳过，表面上一切正常。
/// 所以生成逻辑有实质变化就把它 +1，让所有相关步骤重跑一遍。
///
/// 管两处：`Gateway` 的 nft/VPN 脚本，`Net` 的地址/路由脚本。两者都是
/// "把生成的脚本塞进容器"的步骤，症状一样（改好了但不生效）。
///
///   1 → 初版（逐条 `nft add rule`）
///   2 → 原子批次 + `dnat ip to` + 映射回程 SNAT + VPN 关闭落地
///   3 → 地址与 nft 规则落成容器内开机单元（容器重启后自愈）
pub const RULESET_VERSION: u32 = 3;

/// 一个步骤的完整描述。`kind` 里带全部参数，执行器不需要再去查 spec。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    Bridge {
        zone: String,
        name: String,
        /// 宿主在桥上的管理地址（Web 终端从宿主直连容器）
        mgmt_ip: String,
        /// 上联的网桥；`wan` 时为外部桥
        uplink_bridge: String,
    },
    Template {
        /// 模板所在存储（vztmpl 内容），与 rootfs 的 `storage` 是两回事
        template_storage: String,
        template: String,
    },
    Container {
        zone: String,
        host: String,
        vmid: u32,
        cores: u32,
        memory: u32,
        disk: u32,
        template: String,
        /// rootfs 落点
        storage: String,
        /// 模板所在存储（查 volid 用）
        template_storage: String,
        /// 抓包口：一个被监控网络一块，按 zone 名排序后落在 eth1、eth2……
        /// 空列表 = 这台主机不抓包。eth0 恒为管理口。
        capture_bridges: Vec<String>,
        node: Option<String>,
        /// 接入桥（建机时就绑，避免先建后改）
        bridge: String,
        /// 只有网关有第二块网卡（上联）。没有它，网关脚本配 eth1 会直接失败。
        uplink_bridge: Option<String>,
        /// eth0 走 DHCP 而不是静态地址。只有"挂在外部网桥上"的主机是这样。
        dhcp: bool,
        ipv4: String,
        gateway: Option<String>,
        // dns 不在这里：解析器由 net 步骤写 /etc/resolv.conf，这里再存一份只会漂
        ssh_keys: Vec<String>,
        /// 建机时的 root 密码；不写就用 PVE 默认（无密码 + SSH key）
        password: Option<String>,
        hostname: String,
    },
    Net {
        zone: String,
        host: String,
        vmid: u32,
        ipv4: String,
        gateway: Option<String>,
        ipv6_prefix: Option<String>,
        /// 是否网关：走带 NAT 的网关脚本，内网口 eth0 / 上联口 eth1
        is_gateway: bool,
        /// 是否需要默认路由。网关与监控机需要（要出网装包），普通主机不要
        default_route: bool,
        /// 解析器列表（来自 spec，逐级继承）。空 = 用内置公共 DNS。
        dns: Vec<String>,
        /// 网关是否做地址转换（来自 zone 的 `nat`）。只对网关有意义，
        /// 非网关恒为 false——写在步骤里是为了让执行器不必回头查 spec。
        nat: bool,
        /// 生成器版本，见 RULESET_VERSION
        ruleset: u32,
    },
    Bootstrap {
        zone: String,
        host: String,
        vmid: u32,
        provisioner: String,
        /// 渲染后的参数（进 hash，实际内容在执行时才落盘）
        args: BTreeMap<String, String>,
    },
    /// 网关策略：nft 规则 + 可选 VPN。整份策略是**全量**的——每次重建规则表，
    /// 不做增量，避免"删了一条映射但规则还在"这种漂移。
    Gateway {
        zone: String,
        host: String,
        vmid: u32,
        /// 内网网段（NAT 源）
        zone_cidr: String,
        /// 是否做地址转换
        nat: bool,
        /// 内网是否允许出网
        egress: bool,
        v6_prefix: Option<String>,
        pubs: Vec<ResolvedPub>,
        vpn: ResolvedVpn,
        /// 上联网卡名（容器内固定 eth1）
        uplink: String,
        /// 生成器版本，见 RULESET_VERSION
        ruleset: u32,
    },
    Mirror {
        zone: String,
        /// 源桥的实际名字（zone 名与桥名不一定相同：lab → lablab）
        bridge: String,
        /// 抓包桥：这块网络的流量落到监控机的哪块两层桥上（`mir{zone}`）
        capture_bridge: String,
        source_host: String,
        source_vmid: u32,
        /// 目标所在 zone（可以不是源 zone——抓包口是每网络独立的）
        target_zone: String,
        target_host: String,
        target_vmid: u32,
        /// 监控机上的抓包网卡（eth1、eth2……），只用于日志与排障
        target_iface: String,
    },
    Snapshot {
        zone: String,
        host: String,
        vmid: u32,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// 稳定标识，不含时间与序号：`ct:main:gateway`
    pub key: String,
    pub phase: Phase,
    pub zone: String,
    pub kind: StepKind,
    /// 本步骤依赖的其他 key（同阶段内的显式依赖）
    pub deps: Vec<String>,
    /// 脚本内容 + 渲染参数 + 目标 vmid 的哈希。任何一项变了就重跑。
    pub input_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub steps: Vec<Step>,
}

impl Plan {
    /// 展开。`vmid_for` 负责给出每台主机的 vmid（由调用方查 DB 或按基数分配）。
    pub fn build(
        zones: &[ResolvedZone],
        mirrors: &[ResolvedMirror],
        provisioner_hash: &dyn Fn(&str) -> String,
        vmid_for: &dyn Fn(&str, &str) -> u32,
        want_snapshot: bool,
    ) -> Self {
        let mut steps = Vec::new();

        // ── 0. 抓包桥：每个"被监控的网络"一块独立的两层桥 ──
        //
        // 一个网络一块，而不是全局共用一块：共用的话监控机那几块网卡会落进
        // 同一个广播域，两块口收到同一份流量，抓包工具分不清哪个包来自哪个
        // 网络——而这正是"接了多个网络"唯一要回答的问题。
        let mut mir_zones: BTreeSet<String> = BTreeSet::new();
        for z in zones {
            for h in &z.hosts {
                mir_zones.extend(h.capture.iter().cloned());
            }
        }
        for zn in &mir_zones {
            let name = capture_bridge(zn);
            let kind = StepKind::Bridge {
                zone: zn.clone(),
                mgmt_ip: String::new(),
                name: name.clone(),
                uplink_bridge: String::new(),
            };
            steps.push(Step {
                key: format!("bridge:mir:{zn}"),
                phase: Phase::Bridge,
                zone: zn.clone(),
                input_hash: hash_of(&kind),
                deps: vec![],
                kind,
            });
        }

        // ── 1. 桥：每个 zone 一个，先建上游 ──
        for z in zones {
            // 外网合成 zone 的"桥"是 PVE 本来就有的 wan 桥，既不该建也不该
            // 往里写管理地址——`ensure_bridge` 会往里塞一段 iface 配置，
            // 那等于把宿主的上联口改了。
            if z.external {
                continue;
            }
            let kind = StepKind::Bridge {
                zone: z.name.clone(),
                mgmt_ip: z.bridge_mgmt_ip.clone(),
                name: z.bridge.clone(),
                uplink_bridge: z.uplink_bridge.clone().unwrap_or_default(),
            };
            steps.push(Step {
                key: format!("bridge:{}", z.name),
                phase: Phase::Bridge,
                zone: z.name.clone(),
                input_hash: hash_of(&kind),
                deps: z
                    .uplink
                    .as_ref()
                    .map(|up| vec![format!("bridge:{}", up)])
                    .unwrap_or_default(),
                kind,
            });
        }

        // ── 2. 模板：同 (template_storage, template) 只下一次 ──
        let mut seen_tpl: BTreeMap<(String, String), ()> = BTreeMap::new();
        for z in zones {
            for h in &z.hosts {
                let k = (h.template_storage.clone(), h.template.clone());
                if seen_tpl.insert(k.clone(), ()).is_none() {
                    let kind = StepKind::Template {
                        template_storage: k.0.clone(),
                        template: k.1.clone(),
                    };
                    steps.push(Step {
                        key: format!("template:{}:{}", k.0, k.1),
                        phase: Phase::Template,
                        zone: String::new(),
                        input_hash: hash_of(&kind),
                        deps: Vec::new(),
                        kind,
                    });
                }
            }
        }

        // ── 3. 容器 ──
        for z in zones {
            let gw_ipv4 = z
                .hosts
                .iter()
                .find(|h| h.role == Role::Gateway)
                .map(|h| strip_prefix(&h.ipv4));
            for h in &z.hosts {
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let gateway = if h.role == Role::Gateway {
                    None
                } else {
                    gw_ipv4.clone()
                };
                let kind = StepKind::Container {
                    zone: z.name.clone(),
                    host: h.name.clone(),
                    vmid,
                    cores: h.cores,
                    memory: h.memory,
                    disk: h.disk,
                    template: h.template.clone(),
                    storage: h.storage.clone(),
                    template_storage: h.template_storage.clone(),
                    capture_bridges: h.capture.iter().map(|z| capture_bridge(z)).collect(),
                    node: h.node.clone(),
                    bridge: z.bridge.clone(),
                    // 监控机不需要额外的上联口：它在 zone 里就是个普通主机，
                    // 出网走网关。只有网关才有第二块网卡（上联）。
                    uplink_bridge: if h.role == Role::Gateway {
                        z.uplink_bridge.clone()
                    } else {
                        None
                    },
                    dhcp: z.external,
                    ipv4: h.ipv4.clone(),
                    gateway,
                    ssh_keys: h.ssh_keys.clone(),
                    password: h.password.clone(),
                    hostname: format!("{}-{}", sanitize(&z.name), sanitize(&h.name)),
                };
                let mut deps = vec![format!(
                    "template:{}:{}",
                    h.template_storage, h.template
                )];
                // 外网 zone 没有自己的桥步骤（桥是 PVE 的 wan 桥）
                if !z.external {
                    deps.push(format!("bridge:{}", z.name));
                }
                // 抓包口要在建容器之前就绪：`pct create --net1 bridge=...` 里
                // 写了不存在的桥，PVE 会建出一块"挂着但不通"的网卡，且不报错。
                for c in &h.capture {
                    deps.push(format!("bridge:mir:{c}"));
                }
                steps.push(Step {
                    key: format!("ct:{}:{}", z.name, h.name),
                    phase: Phase::Container,
                    zone: z.name.clone(),
                    input_hash: hash_of(&kind),
                    deps,
                    kind,
                });
            }
        }

        // ── 4. 网络 ──
        for z in zones {
            // 外网 zone 的主机地址由 DHCP 给，路由/DNS 也是 DHCP 塞进来的。
            // 再跑一遍静态配置只会把它覆盖成一份错的。
            if z.external {
                continue;
            }
            let gw_ipv4 = z
                .hosts
                .iter()
                .find(|h| h.role == Role::Gateway)
                .map(|h| strip_prefix(&h.ipv4));
            for h in &z.hosts {
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let is_gw = h.role == Role::Gateway;
                // 网关要出网（走 eth1），监控机要装包/上报，其余主机必须隔离
                let wants_route = matches!(h.role, Role::Gateway | Role::Monitor);
                let kind = StepKind::Net {
                    zone: z.name.clone(),
                    host: h.name.clone(),
                    vmid,
                    ipv4: h.ipv4.clone(),
                    gateway: if is_gw { None } else { gw_ipv4.clone() },
                    ipv6_prefix: h.ipv6_prefix.clone(),
                    is_gateway: is_gw,
                    default_route: wants_route,
                    nat: is_gw && z.nat,
                    dns: h.dns.clone(),
                    ruleset: RULESET_VERSION,
                };
                steps.push(Step {
                    key: format!("net:{}:{}", z.name, h.name),
                    phase: Phase::Net,
                    zone: z.name.clone(),
                    input_hash: hash_of(&kind),
                    deps: vec![format!("ct:{}:{}", z.name, h.name)],
                    kind,
                });
            }
        }

        // ── 5. provisioning ──
        // 对**所有**主机执行（不再要求有 provisioner）：基础配置——时区、
        // 语言、apt 源、预装包——是每台机器的需求，不该以"配了 provisioner"
        // 为前提。没配 provisioner 的按 "none" 走，只跑 base bootstrap。
        //
        // 但有一条隐式依赖必须显式化：`apt_mirror` 指向内网缓存时，缓存那台
        // 必须先装完 apt-cacher-ng。真机上撞到过——主机的 bootstrap 与缓存的
        // provisioner 同时在跑，主机 apt update 打到一个还没人监听的 3142，
        // 整步失败，而"网络没问题"的表象会让人怀疑到错的地方去。
        let mut apt_source: HashMap<(String, String), String> = HashMap::new();
        for z in zones {
            for h in &z.hosts {
                let has_prov = h
                    .provisioner
                    .as_deref()
                    .map(|p| p != "none")
                    .unwrap_or(false);
                if has_prov {
                    apt_source.insert((z.name.clone(), strip_prefix(&h.ipv4)), h.name.clone());
                }
            }
        }
        for z in zones {
            for h in &z.hosts {
                let prov = h
                    .provisioner
                    .clone()
                    .unwrap_or_else(|| "none".to_string());
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let mut args = BTreeMap::new();
                args.insert("hostname".into(), format!("{}-{}", sanitize(&z.name), sanitize(&h.name)));
                args.insert("zone".into(), z.name.clone());
                args.insert("ipv4".into(), h.ipv4.clone());
                args.insert("role".into(), h.role.as_str().into());
                // 键名会变成 `LABYARD_<KEY>`，必须与 assets/base/bootstrap.sh 里
                // 读的变量名逐字对齐。之前这里是 `ipv6_prefix`，脚本读 `V6_PREFIX`，
                // 前缀永远传不进去也不报错——所以两个名字只能存在一个。
                if let Some(p) = &h.ipv6_prefix {
                    args.insert("v6_prefix".into(), p.clone());
                }
                if let Some(tz) = &h.timezone {
                    args.insert("timezone".into(), tz.clone());
                }
                if let Some(l) = &h.lang {
                    args.insert("lang".into(), l.clone());
                }
                if let Some(m) = &h.apt_mirror {
                    args.insert("apt_mirror".into(), m.clone());
                }
                if !h.packages.is_empty() {
                    // 空格连接：bootstrap.sh 里按 word splitting 展开
                    args.insert("packages".into(), h.packages.join(" "));
                }
                let kind = StepKind::Bootstrap {
                    zone: z.name.clone(),
                    host: h.name.clone(),
                    vmid,
                    provisioner: prov.clone(),
                    args,
                };
                // hash 里必须含脚本内容，否则改脚本不会触发重跑
                let mut hsh = hash_of(&kind);
                hsh.push_str(&provisioner_hash(&prov));
                let mut deps = vec![format!("net:{}:{}", z.name, h.name)];
                if let Some(m) = &h.apt_mirror {
                    if let Some(ip) = host_of_url(m) {
                        if let Some(src) = apt_source.get(&(z.name.clone(), ip)) {
                            if src != &h.name {
                                deps.push(format!("bootstrap:{}:{}", z.name, src));
                            }
                        }
                    }
                }
                steps.push(Step {
                    key: format!("bootstrap:{}:{}", z.name, h.name),
                    phase: Phase::Bootstrap,
                    zone: z.name.clone(),
                    input_hash: sha256_hex(hsh.as_bytes()),
                    deps,
                    kind,
                });
            }
        }

        // ── 6. 网关策略：每个有网关的 zone 一步 ──
        // 这一步是**全量重建** nft 规则表 + 落地 VPN，输入哈希覆盖整份策略：
        // 改一条端口映射 → 重跑；出网开关翻转 → 重跑。
        for z in zones {
            let Some(gw) = &z.gateway else { continue };
            let Some(h) = z.hosts.iter().find(|h| h.role == Role::Gateway) else {
                continue;
            };
            let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
            let kind = StepKind::Gateway {
                zone: z.name.clone(),
                host: h.name.clone(),
                vmid,
                zone_cidr: zone_cidr(&z.v4_cidr),
                nat: z.nat,
                egress: gw.egress,
                v6_prefix: z.v6_prefix.clone(),
                pubs: gw.pubs.clone(),
                vpn: gw.vpn.clone(),
                uplink: "eth1".into(),
                ruleset: RULESET_VERSION,
            };
            steps.push(Step {
                key: format!("gateway:{}", z.name),
                phase: Phase::Gateway,
                zone: z.name.clone(),
                input_hash: hash_of(&kind),
                deps: vec![
                    format!("net:{}:{}", z.name, h.name),
                    format!("bootstrap:{}:{}", z.name, h.name),
                ],
                kind,
            });
        }

        // ── 7. 镜像链路：解析层已经展开成确定的连线表 ──
        // 计划层只负责补 vmid、抓包桥与依赖。跨 zone 的连线在这里和同 zone
        // 的没有区别——每条连线自带它该落的抓包桥。
        //
        // 哈希里要含脚本内容：`labmirror.sh` 改了而哈希没变，已经"完成"的
        // 步骤全部跳过，宿主编排里跑的还是旧脚本——这种不一致在界面上完全
        // 看不出来（所有步骤都是 done）。bootstrap 早就这么做了，这里补上。
        let mirror_script_hash = crate::resources::assets::Assets::text("base/labmirror.sh")
            .map(|s| sha256_hex(s.as_bytes()))
            .unwrap_or_default();
        for m in mirrors {
            let kind = StepKind::Mirror {
                zone: m.from_zone.clone(),
                bridge: m.src_bridge.clone(),
                capture_bridge: m.capture_bridge.clone(),
                source_host: m.from_host.clone(),
                source_vmid: vmid_for(&m.from_zone, &m.from_host),
                target_zone: m.to_zone.clone(),
                target_host: m.to_host.clone(),
                target_vmid: vmid_for(&m.to_zone, &m.to_host),
                target_iface: m.to_iface.clone(),
            };
            let mut hsh = hash_of(&kind);
            hsh.push_str(&mirror_script_hash);
            steps.push(Step {
                key: format!(
                    "mirror:{}:{}->{}:{}",
                    m.from_zone, m.from_host, m.to_zone, m.to_host
                ),
                phase: Phase::Mirror,
                zone: m.from_zone.clone(),
                input_hash: sha256_hex(hsh.as_bytes()),
                deps: vec![
                    // 源主机要活着（veth 才存在）
                    format!("bootstrap:{}:{}", m.from_zone, m.from_host),
                    // 目标监控机要把抓包网卡建出来（ct 步骤负责网卡）
                    format!("ct:{}:{}", m.to_zone, m.to_host),
                    format!("bridge:mir:{}", m.from_zone),
                ],
                kind,
            });
        }

        // ── 8. 快照 ──
        if want_snapshot {
            for z in zones {
                for h in &z.hosts {
                    let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                    let kind = StepKind::Snapshot {
                        zone: z.name.clone(),
                        host: h.name.clone(),
                        vmid,
                        name: "post-apply".into(),
                    };
                    steps.push(Step {
                        key: format!("snapshot:{}:{}", z.name, h.name),
                        phase: Phase::Snapshot,
                        zone: z.name.clone(),
                        input_hash: hash_of(&kind),
                        deps: vec![format!("net:{}:{}", z.name, h.name)],
                        kind,
                    });
                }
            }
        }

        // 稳定排序：阶段 → zone → key
        steps.sort_by(|a, b| {
            a.phase
                .cmp(&b.phase)
                .then_with(|| a.zone.cmp(&b.zone))
                .then_with(|| a.key.cmp(&b.key))
        });

        Plan { steps }
    }

    pub fn by_phase(&self, p: Phase) -> impl Iterator<Item = &Step> {
        self.steps.iter().filter(move |s| s.phase == p)
    }

    /// 只保留某个 zone 的步骤（revive、单 zone 重跑用）。
    ///
    /// 跨出子计划的依赖必须剔掉：否则执行器会认为"上游没跑过"，把步骤
    /// 一直往后拖——而那个上游本来就不在这个 run 里。
    pub fn for_zone(&self, zone: &str) -> Plan {
        let steps: Vec<Step> = self
            .steps
            .iter()
            .filter(|s| s.zone == zone)
            .cloned()
            .collect();
        let keys: std::collections::HashSet<String> =
            steps.iter().map(|s| s.key.clone()).collect();
        let steps = steps
            .into_iter()
            .map(|mut s| {
                s.deps.retain(|d| keys.contains(d));
                s
            })
            .collect();
        Plan { steps }
    }
}

/// zone 网段归一：`10.10.1.7/24` 也要变成 `10.10.1.0/24`，
/// 否则它会直接进 nft 的 `ip saddr`，规则语义就错了。
fn zone_cidr(cidr: &str) -> String {
    let (ip, len) = cidr.split_once('/').unwrap_or((cidr, "24"));
    let plen: u32 = len.parse().unwrap_or(24).min(32);
    let octets: Vec<u32> = ip.split('.').filter_map(|x| x.parse().ok()).collect();
    if octets.len() != 4 {
        return format!("{ip}/{len}");
    }
    let addr = (octets[0] << 24) | (octets[1] << 16) | (octets[2] << 8) | octets[3];
    let mask = if plen == 0 { 0 } else { u32::MAX << (32 - plen) };
    let net = addr & mask;
    format!(
        "{}.{}.{}.{}/{}",
        (net >> 24) & 0xff,
        (net >> 16) & 0xff,
        (net >> 8) & 0xff,
        net & 0xff,
        plen
    )
}

/// 从 `http://10.77.0.10:3142/debian` 里抠出主机。只认 IPv4 字面量：
/// 域名（tuna 之类）指向的是外网，不可能是"内网里某台机器"。
fn host_of_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.split(':').next().unwrap_or(authority);
    let ok = host.split('.').count() == 4
        && host
            .split('.')
            .all(|o| !o.is_empty() && o.chars().all(|c| c.is_ascii_digit()));
    ok.then(|| host.to_string())
}

fn strip_prefix(ipv4_with_len: &str) -> String {
    ipv4_with_len
        .split('/')
        .next()
        .unwrap_or(ipv4_with_len)
        .to_string()
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn hash_of<T: Serialize>(v: &T) -> String {
    let json = serde_json::to_string(v).unwrap_or_default();
    sha256_hex(json.as_bytes())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::spec::Spec;

    const Y: &str = r#"
defaults: { cores: 2, memory: 1024, disk: 8, template: debian-13, storage: local-lvm, template_storage: local }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      analysis: { role: analysis, mirror: target, provisioner: apt-mirror }
      server1: { role: server, mirror: source }
      client1: { role: client }
"#;

    fn build(y: &str) -> Plan {
        let spec = Spec::parse(y).unwrap();
        let s = spec.resolve_all("vmbr0").unwrap();
        Plan::build(&s.zones, &s.mirrors, &|_| "noop".into(), &|z, h| {
            // 测试用确定性分配
            let idx = match h {
                "gateway" => 1,
                "analysis" => 2,
                "server1" => 3,
                _ => 4,
            };
            let zoff = if z == "main" { 100 } else { 200 };
            zoff + idx
        }, false)
    }

    #[test]
    fn phases_are_ordered() {
        let p = build(Y);
        let mut last = Phase::Bridge;
        for s in &p.steps {
            assert!(s.phase >= last, "阶段顺序被打乱: {:?} 在 {:?} 之后", s.phase, last);
            last = s.phase;
        }
    }

    #[test]
    fn container_depends_on_bridge_and_template() {
        let p = build(Y);
        let ct = p.steps.iter().find(|s| s.key == "ct:main:gateway").unwrap();
        assert!(ct.deps.contains(&"bridge:main".to_string()));
        assert!(ct.deps.contains(&"template:local:debian-13".to_string()));
    }

    #[test]
    fn template_is_deduped() { // 同 (template_storage, template) 只下一次
        let p = build(Y);
        let n = p.steps.iter().filter(|s| s.phase == Phase::Template).count();
        assert_eq!(n, 1, "同一 storage+template 只应下载一次");
    }

    #[test]
    fn only_targets_lack_default_route() {
        let p = build(Y);
        let get = |name: &str| -> (bool, bool) {
            p.steps
                .iter()
                .find_map(|s| match &s.kind {
                    StepKind::Net {
                        host,
                        is_gateway,
                        default_route,
                        ..
                    } if host == name => Some((*is_gateway, *default_route)),
                    _ => None,
                })
                .unwrap()
        };
        // (is_gateway, default_route)
        assert_eq!(get("gateway"), (true, true), "网关是网关，且要出网");
        assert_eq!(get("analysis"), (false, true), "监控机不是网关，但要装包");
        assert_eq!(get("server1"), (false, false), "普通主机不要默认路由");
        assert_eq!(get("client1"), (false, false), "普通主机不要默认路由");
    }

    #[test]
    fn nat_flag_reaches_the_net_step() {
        // 只有网关会带 nat；非网关即使 zone 写了 nat 也不该做地址转换。
        // 早先这个字段根本没传到执行器，`nat: false` 是个纯装饰字段。
        let p = build(Y);
        for s in &p.steps {
            if let StepKind::Net {
                host,
                is_gateway,
                nat,
                ..
            } = &s.kind
            {
                if *is_gateway {
                    assert!(*nat, "网关 `{host}` 应带 nat（Y 里没关）");
                } else {
                    assert!(!*nat, "非网关 `{host}` 不该带 nat");
                }
            }
        }
    }

    #[test]
    fn net_step_carries_the_ruleset_version() {
        // 地址/路由脚本也是"生成器输出塞进容器"，靠这个字段顶掉已完成的步骤。
        // 漏填空就等于"改了 net.rs 但不生效"，且 apply 全程显示 done。
        let p = build(Y);
        let versions: Vec<u32> = p
            .steps
            .iter()
            .filter_map(|s| match &s.kind {
                StepKind::Net { ruleset, .. } => Some(*ruleset),
                _ => None,
            })
            .collect();
        assert!(!versions.is_empty(), "应该有 net 步骤");
        assert!(
            versions.iter().all(|v| *v == RULESET_VERSION),
            "net 步骤必须带当前生成器版本 {RULESET_VERSION}"
        );
    }

    #[test]
    fn bootstrap_env_names_match_the_script() {
        // 契约测试：参数键会变成 `LABYARD_<KEY>`，而脚本按名字读。
        // 两边差一个字母，参数就静默传不进去（真实踩过：ipv6_prefix vs V6_PREFIX），
        // 所以直接拿脚本源码核对，而不是靠人记住。
        let script = crate::resources::assets::Assets::text("base/bootstrap.sh").expect("内置脚本");
        let p = build(Y);
        let mut checked = 0;
        for s in &p.steps {
            if let StepKind::Bootstrap { args, .. } = &s.kind {
                for k in args.keys() {
                    let env = format!("LABYARD_{}", k.to_uppercase().replace('-', "_"));
                    assert!(
                        script.contains(&env),
                        "脚本里没有读 {env}，键 `{k}` 传了也不会生效"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked >= 4, "应该至少检查到 hostname/role/ipv4/v6_prefix，实际 {checked}");
    }

    #[test]
    fn mirror_step_exists_per_pair() {
        let p = build(Y);
        let m: Vec<_> = p.by_phase(Phase::Mirror).collect();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].key, "mirror:main:server1->main:analysis");
    }

    #[test]
    fn gateway_step_sits_between_bootstrap_and_mirror() {
        let y = r#"
defaults: { cores: 2, memory: 1024, disk: 8, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway:
      egress: false
      pub:
        - { proto: tcp, port: 2222, to: "10.10.1.20:22" }
    hosts:
      gateway: { role: gateway }
      web: { role: server }
      analysis: { role: analysis, mirror: target }
mirrors:
  - { from: web, to: analysis }
"#;
        let p = build(y);
        let gw = p
            .steps
            .iter()
            .find(|s| s.key == "gateway:main")
            .expect("有网关主机的 zone 必须有 gateway 步骤");
        assert_eq!(gw.phase, Phase::Gateway);
        assert!(gw.deps.contains(&"bootstrap:main:gateway".to_string()));
        match &gw.kind {
            StepKind::Gateway { egress, nat, pubs, .. } => {
                assert!(!*egress, "egress: false 必须传进步骤");
                assert!(*nat);
                assert_eq!(pubs.len(), 1);
            }
            _ => panic!("类型不对"),
        }
        // 阶段顺序：bootstrap < gateway < mirror
        let idx = |ph: Phase| p.steps.iter().position(|s| s.phase == ph).unwrap();
        assert!(idx(Phase::Bootstrap) < idx(Phase::Gateway));
        assert!(idx(Phase::Gateway) < idx(Phase::Mirror));
    }

    #[test]
    fn plan_can_be_filtered_to_one_zone() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway }, web: { role: server } }
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts: { gateway: { role: gateway } }
"#;
        let p = build(y);
        let sub = p.for_zone("sub");
        assert!(!sub.steps.is_empty());
        assert!(sub.steps.iter().all(|s| s.zone == "sub"));
        // 跨 zone 依赖（bridge:main）必须被剔掉，否则 sub 的桥永远等不到上游
        let b = sub.steps.iter().find(|s| s.key == "bridge:sub").unwrap();
        assert!(b.deps.is_empty(), "跨 zone 依赖应被剔掉: {:?}", b.deps);
    }

    #[test]
    fn zone_cidr_normalises_network_address() {
        assert_eq!(zone_cidr("10.10.1.0/24"), "10.10.1.0/24");
        assert_eq!(zone_cidr("10.10.1.77/24"), "10.10.1.0/24");
        assert_eq!(zone_cidr("10.20.5.9/16"), "10.20.0.0/16");
        assert_eq!(zone_cidr("garbage"), "garbage/24");
    }

    #[test]
    fn plan_is_deterministic() {
        let a = build(Y);
        let b = build(Y);
        let ka: Vec<_> = a.steps.iter().map(|s| (&s.key, &s.input_hash)).collect();
        let kb: Vec<_> = b.steps.iter().map(|s| (&s.key, &s.input_hash)).collect();
        assert_eq!(ka, kb);
    }

    #[test]
    fn input_hash_changes_with_parameters() {
        let a = build(Y);
        let y2 = Y.replace("memory: 1024", "memory: 2048");
        let b = build(&y2);
        let ha = a.steps.iter().find(|s| s.key == "ct:main:server1").unwrap();
        let hb = b.steps.iter().find(|s| s.key == "ct:main:server1").unwrap();
        assert_ne!(ha.input_hash, hb.input_hash);
    }

    #[test]
    fn apt_mirror_host_waits_for_the_cache() {
        // 真机上撞到的竞态：主机的 apt_mirror 指向内网缓存，而缓存的
        // apt-cacher-ng 还没装完，主机 apt update 直接 connection refused。
        // 依赖必须显式写出来，靠并发调度"大概先跑"是不行的。
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: debian-13, storage: local-lvm }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      cache:   { role: analysis, ip: 10, provisioner: apt-mirror }
      client1: { role: client, ip: 20, apt_mirror: "http://10.10.1.10:3142/debian" }
      other:   { role: server, ip: 21, apt_mirror: "http://mirrors.tuna.tsinghua.edu.cn/debian" }
"#;
        let p = build(y);
        let c = p.steps.iter().find(|s| s.key == "bootstrap:main:client1").unwrap();
        assert!(
            c.deps.contains(&"bootstrap:main:cache".to_string()),
            "指向内网缓存的主机必须等缓存装完: {:?}",
            c.deps
        );
        let o = p.steps.iter().find(|s| s.key == "bootstrap:main:other").unwrap();
        assert!(
            !o.deps.iter().any(|d| d.starts_with("bootstrap:")),
            "外网源不该凭空多出依赖: {:?}",
            o.deps
        );
    }

    #[test]
    fn url_host_parsing() {
        assert_eq!(host_of_url("http://10.77.0.10:3142/debian").as_deref(), Some("10.77.0.10"));
        assert_eq!(host_of_url("https://mirrors.tuna.tsinghua.edu.cn/debian"), None);
        assert_eq!(host_of_url("10.0.0.5/debian"), Some("10.0.0.5".to_string()));
    }
}
