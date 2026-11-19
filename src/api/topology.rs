//! 全局拓扑：**网络是锚点，主机挂在网络上**。
//!
//! 第一版画的是"zone / 桥 / 主机 / 镜像桥"四类节点，连线是 `uplink` /
//! `member` / `mirror-port` / `mirror` 四种。看图的直接感受是"这图画错了"，
//! 因为人问的问题是**"这台机器接在哪个网段上"**，而图回答的是"这个对象
//! 属于哪个 zone"。zone 本身不是网络——它是个配置作用域。
//!
//! 所以这一版把 zone 压成网络节点的属性（网段、桥名、NAT、出网），只留两类节点：
//!
//! - `net:wan` / `net:{zone}`：外网与各 zone 的内网
//! - `host:{zone}:{host}`：容器
//!
//! 边只有两条语义，方向都是**数据流方向**（外网 → 网关 → 内网 → 主机）：
//!
//! - `attach`：谁插在谁上面。网关因此有两条——`net_up → gw`（上联口）与
//!   `gw → net_zone`（内网口），这一对边就把它画成"桥接两个网络的那台机器"，
//!   也正是 dagre 分出层级链 `外网 → 网关A → 内网A → 网关B → 内网B` 的依据。
//! - `mirror`：`net → 分析机` 的虚线单向箭头，表示"这个网段上被抓的流量"
//!   进到分析机。同一条 (网络, 分析机) 上多个源主机聚合成一条，label 里列出来，
//!   否则一个网段抓三台机就会叠出三条完全重合的虚线。
//!
//! 前端仍然只负责画：它不理解 `uplink: main`，也不需要知道 mirror 的源是主机
//! 还是桥。

use crate::core::spec::{ResolvedSpec, Role};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct Node {
    pub id: String,
    /// network | host
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
    /// attach | mirror
    pub kind: String,
    pub label: Option<String>,
    /// attach 边的接口角色：uplink（接上层网络）| inner（接本 zone 内网）
    pub port: Option<String>,
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
        id: "net:wan".into(),
        kind: "network".into(),
        label: "外网".into(),
        zone: None,
        role: None,
        vmid: None,
        ipv4: None,
        status: None,
        retired: false,
        meta: serde_json::json!({
            "bridge": st.wan_bridge,
            "external": true,
            "cidr": null,
        }),
    });

    for (spec_name, spec) in specs {
        for z in &spec.zones {
            let gateway = z.gateway.as_ref();
            let egress = gateway.map(|g| g.egress).unwrap_or(false);
            let pubs = gateway.map(|g| g.pubs.len()).unwrap_or(0);
            let vpn: Vec<&str> = gateway
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

            // 网络节点：这个 zone 的内网。标签里带 zone 名，免得一张图里
            // 三个"内网"分不清是哪个。
            let gateway_host = z
                .hosts
                .iter()
                .find(|h| h.role == Role::Gateway)
                .map(|h| h.name.clone());
            nodes.push(Node {
                id: format!("net:{}", z.name),
                kind: "network".into(),
                label: format!("{} 内网", z.name),
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
                    "mgmt_ip": z.bridge_mgmt_ip,
                    "nat": z.nat,
                    "uplink": z.uplink,
                    "uplink_bridge": z.uplink_bridge,
                    "gateway_host": gateway_host,
                    "egress": egress,
                    "pubs": pubs,
                    "vpn": vpn,
                    "external": false,
                }),
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
                        "spec": spec_name,
                        "cores": h.cores,
                        "memory": h.memory,
                        "disk": h.disk,
                        "provisioner": h.provisioner,
                        "mirror": h.mirror,
                    }),
                });

                let host_id = format!("host:{}:{}", z.name, h.name);
                let inner = format!("net:{}", z.name);
                let ip = h.ipv4.split('/').next().unwrap_or(&h.ipv4);

                if h.role == Role::Gateway {
                    // 上联口：从上层网络指向网关（数据从外面进来）
                    let up = match &z.uplink {
                        None => "net:wan".to_string(),
                        Some(u) => format!("net:{u}"),
                    };
                    edges.push(Edge {
                        from: up,
                        to: host_id.clone(),
                        kind: "attach".into(),
                        label: Some("eth0".into()),
                        port: Some("uplink".into()),
                    });
                    // 内网口：从网关指向本 zone 内网（数据流进去）
                    edges.push(Edge {
                        from: host_id,
                        to: inner,
                        kind: "attach".into(),
                        label: Some(format!("eth1 · {ip}")),
                        port: Some("inner".into()),
                    });
                } else {
                    edges.push(Edge {
                        from: inner,
                        to: host_id,
                        kind: "attach".into(),
                        label: Some(ip.to_string()),
                        port: Some("inner".into()),
                    });
                }
            }
        }

        // 镜像：同一个 (源网络, 分析机) 上的多个源聚合成一条虚线，
        // label 写清被抓的是哪几台——否则三条虚线叠在一起，看起来只有一条。
        let mut agg: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
        for m in &spec.mirrors {
            agg.entry((m.from_zone.clone(), m.to_zone.clone(), m.to_host.clone()))
                .or_default()
                .push(m.from_host.clone());
        }
        for ((from_zone, to_zone, to_host), hosts) in agg {
            edges.push(Edge {
                from: format!("net:{from_zone}"),
                to: format!("host:{to_zone}:{to_host}"),
                kind: "mirror".into(),
                label: Some(hosts.join(", ")),
                port: None,
            });
        }
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
    hosts:
      gateway: { role: gateway }
      client1: { role: client }
mirrors:
  - { from: "sub:gateway", to: "main:analysis" }
  - { from: "sub:client1", to: "main:analysis" }
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
    fn zones_are_networks_not_nodes() {
        let (n, _) = run();
        assert!(!n.iter().any(|x| x.kind == "zone"), "zone 不再是节点");
        assert!(n.iter().any(|x| x.id == "net:main" && x.kind == "network"));
        assert!(n.iter().any(|x| x.id == "net:wan"));
        assert!(!n.iter().any(|x| x.id == "mirr"), "没有 mirr 假节点");
    }

    #[test]
    fn a_gateway_bridges_two_networks() {
        let (_, e) = run();
        // 上联：外网 → 网关
        assert!(has_edge(&e, "net:wan", "host:main:gateway", "attach"));
        // 内网口：网关 → 本 zone 内网
        assert!(has_edge(&e, "host:main:gateway", "net:main", "attach"));
        // 二层 zone 的上联是上一层的内网
        assert!(has_edge(&e, "net:main", "host:sub:gateway", "attach"));
        assert!(has_edge(&e, "host:sub:gateway", "net:sub", "attach"));
    }

    #[test]
    fn ordinary_hosts_attach_to_their_own_network() {
        let (_, e) = run();
        assert!(has_edge(&e, "net:main", "host:main:web", "attach"));
        assert!(has_edge(&e, "net:sub", "host:sub:client1", "attach"));
        // 普通主机不应该被画成插在上层网络上
        assert!(!has_edge(&e, "net:wan", "host:main:web", "attach"));
    }

    #[test]
    fn mirrors_run_from_network_to_analysis_and_are_aggregated() {
        let (_, e) = run();
        let m: Vec<&Edge> = e
            .iter()
            .filter(|x| x.kind == "mirror" && x.from == "net:sub")
            .collect();
        assert_eq!(m.len(), 1, "同一网络到同一分析机只画一条");
        assert_eq!(m[0].to, "host:main:analysis");
        let label = m[0].label.as_deref().unwrap_or("");
        assert!(
            label.contains("gateway") && label.contains("client1"),
            "label 要列出被抓的源：{label}"
        );
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
    fn network_carries_the_gateway_policy_summary() {
        let (n, _) = run();
        let net = n.iter().find(|x| x.id == "net:main").unwrap();
        assert_eq!(net.meta["egress"], serde_json::json!(true));
        assert_eq!(net.meta["pubs"], serde_json::json!(1));
        assert_eq!(net.meta["cidr"], serde_json::json!("10.10.1.0/24"));
        assert_eq!(
            net.meta["gateway_host"],
            serde_json::json!("gateway"),
            "前端要靠这个字段在网络上显示网关名"
        );
    }
}
