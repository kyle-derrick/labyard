//! 把解析后的 zone 展开成一串可执行步骤。
//!
//! 步骤按**阶段**分组，阶段内按 zone 拓扑序、zone 内按主机名排序，保证：
//! 同样的输入永远产出同样的顺序（否则 diff 每次都变，幂等就没法看）。
//!
//! 依赖不是靠"隐式顺序"保证，而是显式写在 `deps` 里——这样执行器可以并行
//! 阶段内无依赖的步骤，也可以只重跑失败的后半段。

use crate::core::spec::{MirrorRole, ResolvedHost, ResolvedZone};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// 执行阶段。顺序即语义：前面的阶段必须先完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Bridge,
    Template,
    Container,
    Net,
    Bootstrap,
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
            Phase::Mirror => "mirror",
            Phase::Snapshot => "snapshot",
        }
    }
}

/// 一个步骤的完整描述。`kind` 里带全部参数，执行器不需要再去查 spec。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepKind {
    Bridge {
        zone: String,
        name: String,
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
        node: Option<String>,
        /// 接入桥（建机时就绑，避免先建后改）
        bridge: String,
        /// 只有网关有第二块网卡（上联）。没有它，网关脚本配 eth1 会直接失败。
        uplink_bridge: Option<String>,
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
        /// 是否需要默认路由。网关与分析机需要（要出网装包），靶机不要
        default_route: bool,
        /// 解析器列表（来自 spec，逐级继承）。空 = 用内置公共 DNS。
        dns: Vec<String>,
        /// 网关是否做地址转换（来自 zone 的 `nat`）。只对网关有意义，
        /// 非网关恒为 false——写在步骤里是为了让执行器不必回头查 spec。
        nat: bool,
    },
    Bootstrap {
        zone: String,
        host: String,
        vmid: u32,
        provisioner: String,
        /// 渲染后的参数（进 hash，实际内容在执行时才落盘）
        args: BTreeMap<String, String>,
    },
    Mirror {
        zone: String,
        source_host: String,
        source_vmid: u32,
        target_host: String,
        target_vmid: u32,
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
        provisioner_hash: &dyn Fn(&str) -> String,
        vmid_for: &dyn Fn(&str, &str) -> u32,
        want_snapshot: bool,
    ) -> Self {
        let mut steps = Vec::new();

        // ── 1. 桥：每个 zone 一个，先建上游 ──
        for z in zones {
            let kind = StepKind::Bridge {
                zone: z.name.clone(),
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
                .find(|h| h.role == crate::core::spec::Role::Gateway)
                .map(|h| strip_prefix(&h.ipv4));
            for h in &z.hosts {
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let gateway = if h.role == crate::core::spec::Role::Gateway {
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
                    node: h.node.clone(),
                    bridge: z.bridge.clone(),
                    uplink_bridge: if h.role == crate::core::spec::Role::Gateway {
                        z.uplink_bridge.clone()
                    } else {
                        None
                    },
                    ipv4: h.ipv4.clone(),
                    gateway,
                    ssh_keys: h.ssh_keys.clone(),
                    password: h.password.clone(),
                    hostname: format!("{}-{}", sanitize(&z.name), sanitize(&h.name)),
                };
                steps.push(Step {
                    key: format!("ct:{}:{}", z.name, h.name),
                    phase: Phase::Container,
                    zone: z.name.clone(),
                    input_hash: hash_of(&kind),
                    deps: vec![
                        format!("bridge:{}", z.name),
                        format!("template:{}:{}", h.template_storage, h.template),
                    ],
                    kind,
                });
            }
        }

        // ── 4. 网络 ──
        for z in zones {
            let gw_ipv4 = z
                .hosts
                .iter()
                .find(|h| h.role == crate::core::spec::Role::Gateway)
                .map(|h| strip_prefix(&h.ipv4));
            for h in &z.hosts {
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let is_gw = h.role == crate::core::spec::Role::Gateway;
                // 网关要出网（走 eth1），分析机要装包，靶机必须隔离
                let wants_route = matches!(
                    h.role,
                    crate::core::spec::Role::Gateway | crate::core::spec::Role::Analysis
                );
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
                steps.push(Step {
                    key: format!("bootstrap:{}:{}", z.name, h.name),
                    phase: Phase::Bootstrap,
                    zone: z.name.clone(),
                    input_hash: sha256_hex(hsh.as_bytes()),
                    deps: vec![format!("net:{}:{}", z.name, h.name)],
                    kind,
                });
            }
        }

        // ── 6. 镜像链路：每个 source → 每个 target 一条 ──
        for z in zones {
            let sources: Vec<&ResolvedHost> = z
                .hosts
                .iter()
                .filter(|h| h.mirror == MirrorRole::Source)
                .collect();
            let targets: Vec<&ResolvedHost> = z
                .hosts
                .iter()
                .filter(|h| h.mirror == MirrorRole::Target)
                .collect();
            for s in &sources {
                for t in &targets {
                    let kind = StepKind::Mirror {
                        zone: z.name.clone(),
                        source_host: s.name.clone(),
                        source_vmid: s.vmid.unwrap_or_else(|| vmid_for(&z.name, &s.name)),
                        target_host: t.name.clone(),
                        target_vmid: t.vmid.unwrap_or_else(|| vmid_for(&z.name, &t.name)),
                        target_iface: "eth0".into(),
                    };
                    steps.push(Step {
                        key: format!("mirror:{}:{}->{}", z.name, s.name, t.name),
                        phase: Phase::Mirror,
                        zone: z.name.clone(),
                        input_hash: hash_of(&kind),
                        deps: vec![
                            format!("bootstrap:{}:{}", z.name, s.name),
                            format!("net:{}:{}", z.name, t.name),
                        ],
                        kind,
                    });
                }
            }
        }

        // ── 7. 快照 ──
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
        let zs = spec.resolve("vmbr0").unwrap();
        Plan::build(&zs, &|_| "noop".into(), &|z, h| {
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
        assert_eq!(get("analysis"), (false, true), "分析机不是网关，但要装包");
        assert_eq!(get("server1"), (false, false), "靶机不要默认路由");
        assert_eq!(get("client1"), (false, false), "靶机不要默认路由");
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
        assert_eq!(m[0].key, "mirror:main:server1->analysis");
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
}
