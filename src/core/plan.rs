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
    /// 并发闸用的权重：建容器最重，建桥最轻。
    pub fn weight(self) -> u8 {
        match self {
            Phase::Bridge => 1,
            Phase::Template => 3,
            Phase::Container => 3,
            Phase::Net => 1,
            Phase::Bootstrap => 2,
            Phase::Mirror => 1,
            Phase::Snapshot => 2,
        }
    }

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
        storage: String,
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
        storage: String,
        node: Option<String>,
        /// 接入桥（建机时就绑，避免先建后改）
        bridge: String,
        ipv4: String,
        gateway: Option<String>,
        dns: Vec<String>,
        ssh_keys: Vec<String>,
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

        // ── 2. 模板：同 (storage, template) 只下一次 ──
        let mut seen_tpl: BTreeMap<(String, String), ()> = BTreeMap::new();
        for z in zones {
            for h in &z.hosts {
                let k = (h.storage.clone(), h.template.clone());
                if seen_tpl.insert(k.clone(), ()).is_none() {
                    let kind = StepKind::Template {
                        storage: k.0.clone(),
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
                    node: h.node.clone(),
                    bridge: z.bridge.clone(),
                    ipv4: h.ipv4.clone(),
                    gateway,
                    dns: h.dns.clone(),
                    ssh_keys: h.ssh_keys.clone(),
                    hostname: format!("{}-{}", sanitize(&z.name), sanitize(&h.name)),
                };
                steps.push(Step {
                    key: format!("ct:{}:{}", z.name, h.name),
                    phase: Phase::Container,
                    zone: z.name.clone(),
                    input_hash: hash_of(&kind),
                    deps: vec![
                        format!("bridge:{}", z.name),
                        format!("template:{}:{}", h.storage, h.template),
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
        for z in zones {
            for h in &z.hosts {
                let Some(prov) = &h.provisioner else { continue };
                let vmid = h.vmid.unwrap_or_else(|| vmid_for(&z.name, &h.name));
                let mut args = BTreeMap::new();
                args.insert("hostname".into(), format!("{}-{}", sanitize(&z.name), sanitize(&h.name)));
                args.insert("zone".into(), z.name.clone());
                args.insert("ipv4".into(), h.ipv4.clone());
                args.insert("role".into(), h.role.as_str().into());
                if let Some(p) = &h.ipv6_prefix {
                    args.insert("ipv6_prefix".into(), p.clone());
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
                hsh.push_str(&provisioner_hash(prov));
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
defaults: { cores: 2, memory: 1024, disk: 8, template: debian-13, storage: local-lvm }
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
        assert!(ct.deps.contains(&"template:local-lvm:debian-13".to_string()));
    }

    #[test]
    fn template_is_deduped() {
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
