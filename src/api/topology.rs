//! 全局拓扑：把已解析的规格变成"节点 + 边"。
//!
//! 前端只负责画图，不理解 `uplink: main` 这类语义——解析与展开留在后端一处。
//! 节点 id 是稳定的字符串（`zone:main`、`host:main:web`），刷新不会导致
//! 画布元素被重建。

use crate::core::spec::{ResolvedSpec, Role};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Node {
    pub id: String,
    /// wan | zone | host | mirr
    pub kind: String,
    pub label: String,
    pub zone: Option<String>,
    pub role: Option<String>,
    pub vmid: Option<u32>,
    pub ipv4: Option<String>,
    pub status: Option<String>,
    pub retired: bool,
    /// 画布上要显示的次要信息（网段、桥名、NAT、出网、映射条数……）
    pub meta: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// uplink | member | mirror-port | mirror
    pub kind: String,
    pub label: Option<String>,
}

pub struct State<'a> {
    pub wan_bridge: &'a str,
    /// vmid → PVE 状态（running/stopped）；查不到就是 None
    pub status: &'a dyn Fn(u32) -> Option<String>,
    pub retired: &'a dyn Fn(&str, &str) -> bool,
}

pub fn build(specs: &[(String, ResolvedSpec)], st: &State) -> (Vec<Node>, Vec<Edge>) {
    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();

    nodes.push(Node {
        id: "wan".into(),
        kind: "wan".into(),
        label: format!("WAN ({})", st.wan_bridge),
        zone: None,
        role: None,
        vmid: None,
        ipv4: None,
        status: None,
        retired: false,
        meta: serde_json::json!({ "bridge": st.wan_bridge }),
    });

    let mut has_analysis = false;

    for (spec_name, spec) in specs {
        for z in &spec.zones {
            let egress = z.gateway.as_ref().map(|g| g.egress).unwrap_or(false);
            let pubs = z.gateway.as_ref().map(|g| g.pubs.len()).unwrap_or(0);
            let vpn: Vec<&str> = z
                .gateway
                .as_ref()
                .map(|g| {
                    let mut v = Vec::new();
                    if g.vpn.wireguard {
                        v.push("wireguard");
                    }
                    if g.vpn.openvpn {
                        v.push("openvpn");
                    }
                    v
                })
                .unwrap_or_default();
            nodes.push(Node {
                id: format!("zone:{}", z.name),
                kind: "zone".into(),
                label: z.name.clone(),
                zone: Some(z.name.clone()),
                role: None,
                vmid: None,
                ipv4: None,
                status: None,
                retired: (st.retired)(&z.name, "*"),
                meta: serde_json::json!({
                    "spec": spec_name,
                    "bridge": z.bridge,
                    "cidr": z.v4_cidr,
                    "v6": z.v6_prefix,
                    "nat": z.nat,
                    "uplink": z.uplink,
                    "egress": egress,
                    "pubs": pubs,
                    "vpn": vpn,
                }),
            });

            // 上联：wan 或上游 zone
            let from = match &z.uplink {
                None => "wan".to_string(),
                Some(up) => format!("zone:{up}"),
            };
            edges.push(Edge {
                from,
                to: format!("zone:{}", z.name),
                kind: "uplink".into(),
                label: z.uplink.clone(),
            });

            for h in &z.hosts {
                let retired = (st.retired)(&z.name, &h.name);
                let status = h.vmid.and_then(|v| (st.status)(v));
                nodes.push(Node {
                    id: format!("host:{}:{}", z.name, h.name),
                    kind: "host".into(),
                    label: h.name.clone(),
                    zone: Some(z.name.clone()),
                    role: Some(h.role.as_str().into()),
                    vmid: h.vmid,
                    ipv4: Some(h.ipv4.clone()),
                    status,
                    retired,
                    meta: serde_json::json!({
                        "cores": h.cores,
                        "memory": h.memory,
                        "disk": h.disk,
                        "provisioner": h.provisioner,
                        "mirror": h.mirror,
                    }),
                });
                edges.push(Edge {
                    from: format!("zone:{}", z.name),
                    to: format!("host:{}:{}", z.name, h.name),
                    kind: "member".into(),
                    label: None,
                });
                if h.role == Role::Analysis {
                    has_analysis = true;
                    edges.push(Edge {
                        from: format!("host:{}:{}", z.name, h.name),
                        to: "mirr".into(),
                        kind: "mirror-port".into(),
                        label: None,
                    });
                }
            }
        }

        for m in &spec.mirrors {
            edges.push(Edge {
                from: format!("host:{}:{}", m.from_zone, m.from_host),
                to: format!("host:{}:{}", m.to_zone, m.to_host),
                kind: "mirror".into(),
                label: Some(format!("镜像 · {}", m.src_bridge)),
            });
        }
    }

    if has_analysis {
        nodes.push(Node {
            id: "mirr".into(),
            kind: "mirr".into(),
            label: "mirr".into(),
            zone: None,
            role: None,
            vmid: None,
            ipv4: None,
            status: None,
            retired: false,
            meta: serde_json::json!({ "note": "纯二层镜像桥，无 IP、不挂网关" }),
        });
    }

    (nodes, edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::spec::Spec;

    fn spec() -> ResolvedSpec {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    gateway: { egress: true, pub: [{ proto: tcp, port: 80, to: "10.10.1.20" }] }
    hosts:
      gateway: { role: gateway, vmid: 100 }
      analysis: { role: analysis, vmid: 101 }
      web: { role: server, vmid: 102 }
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts: { gateway: { role: gateway } }
mirrors:
  - { from: "sub:gateway", to: "main:analysis" }
"#;
        Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap()
    }

    fn run() -> (Vec<Node>, Vec<Edge>) {
        let s = spec();
        let st = State {
            wan_bridge: "vmbr0",
            status: &|v| if v == 100 { Some("running".into()) } else { None },
            retired: &|_z, h| h == "web",
        };
        build(&[("lab".to_string(), s)], &st)
    }

    fn has_edge(edges: &[Edge], from: &str, to: &str, kind: &str) -> bool {
        edges
            .iter()
            .any(|e| e.from == from && e.to == to && e.kind == kind)
    }

    #[test]
    fn wan_and_uplinks_are_wired() {
        let (_, e) = run();
        assert!(has_edge(&e, "wan", "zone:main", "uplink"));
        assert!(has_edge(&e, "zone:main", "zone:sub", "uplink"), "zone 之间也要连线");
    }

    #[test]
    fn hosts_hang_under_their_zone() {
        let (n, e) = run();
        assert!(n.iter().any(|x| x.id == "host:main:web"));
        assert!(has_edge(&e, "zone:main", "host:main:web", "member"));
    }

    #[test]
    fn analysis_gets_a_mirror_bridge_node() {
        let (n, e) = run();
        assert!(n.iter().any(|x| x.id == "mirr"));
        assert!(has_edge(&e, "host:main:analysis", "mirr", "mirror-port"));
        assert!(has_edge(&e, "host:sub:gateway", "host:main:analysis", "mirror"));
    }

    #[test]
    fn status_and_retired_are_carried_through() {
        let (n, _) = run();
        let gw = n.iter().find(|x| x.id == "host:main:gateway").unwrap();
        assert_eq!(gw.status.as_deref(), Some("running"));
        let web = n.iter().find(|x| x.id == "host:main:web").unwrap();
        assert!(web.retired, "host 级 retire 要反映到图里");
    }

    #[test]
    fn no_mirror_bridge_when_no_analysis() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts: { gateway: { role: gateway } }
"#;
        let s = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let st = State {
            wan_bridge: "vmbr0",
            status: &|_| None,
            retired: &|_, _| false,
        };
        let (n, _) = build(&[("lab".into(), s)], &st);
        assert!(!n.iter().any(|x| x.id == "mirr"));
    }
}
