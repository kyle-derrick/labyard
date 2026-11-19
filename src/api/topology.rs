//! 全局拓扑：**网络是锚点，主机挂在网络上，采集线从主机接口连到监控机**。
//!
//! 第一版画的是"zone / 桥 / 主机 / 镜像桥"四类节点，连线是 `uplink` /
//! `member` / `mirror-port` / `mirror` 四种。看图的直接感受是"这图画错了"，
//! 因为人问的问题是**"这台机器接在哪个网段上"**，而图回答的是"这个对象
//! 属于哪个 zone"。zone 本身不是网络——它是个配置作用域。
//!
//! 第二版把 zone 压成网络节点的属性，但镜像仍然画成 `网络 → 监控机`。
//! 那还是错的：镜像的来源是**每台主机的网卡**，不是网段。所以这一版：
//!
//! - 节点两类：`net:wan` / `net:{zone}`（网络）与 `host:{zone}:{host}`（主机）
//! - `attach` 边：谁插在谁上面，方向即数据流（外网 → 网关 → 内网 → 主机）。
//!   网关因此有两条——`net_up → gw`（eth1 上联）与 `gw → net_zone`（eth0 内网），
//!   这一对边就把它画成"桥接两个网络的那台机器"。
//! - `mirror` 边：`源主机 → 监控机`，一条主机一条线。label 写
//!   `eth0 → eth1`（源主机的采集口 → 监控机上的抓包口），虚线单向。
//!   不再按网络聚合：聚合之后"这个网段里到底谁被抓了"要读 label 才知道，
//!   而图本来就是用来回答这个问题的。
//!
//! 前端只负责画：它不理解 `uplink: main`，也不需要知道抓包桥叫什么名字。

use crate::core::spec::{ResolvedSpec, Role};
use serde::Serialize;

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
    /// | mgmt（挂在外网桥上的管理口）
    pub port: Option<String>,
    /// mirror 边：源主机上的哪块网卡（恒为 eth0——它在所连网络里的那块脸）
    pub source_iface: Option<String>,
    /// mirror 边：监控机上的哪块抓包网卡（eth1、eth2……）
    pub target_iface: Option<String>,
    /// mirror 边落到的两层桥（`mir{zone}`），抽屉里显示用
    pub capture_bridge: Option<String>,
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
            // 外网合成 zone 的"网络"就是 net:wan，不再画第二个外网节点
            if !z.external {
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
                        "monitor": z.monitor,
                        "egress": egress,
                        "pubs": pubs,
                        "vpn": vpn,
                        "external": false,
                    }),
                });
            }

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
                        "capture": h.capture,
                        "external_zone": z.external,
                    }),
                });

                let host_id = format!("host:{}:{}", z.name, h.name);
                let inner = format!("net:{}", z.name);
                let ip = h.ipv4.split('/').next().unwrap_or(&h.ipv4);

                if z.external {
                    // 挂在外网桥上的主机（监控机常在这种位置）：一块管理口，
                    // 地址由 DHCP 给，所以这里不写 IP。
                    edges.push(Edge {
                        from: "net:wan".into(),
                        to: host_id,
                        kind: "attach".into(),
                        label: Some("eth0 · DHCP".into()),
                        port: Some("mgmt".into()),
                        source_iface: None,
                        target_iface: None,
                        capture_bridge: None,
                    });
                } else if h.role == Role::Gateway {
                    // 上联口：从上层网络指向网关（数据从外面进来）。
                    // 容器里的编号是 eth1 —— eth0 是内网口，两者写反了会让人
                    // 按图去 `ip addr show eth0` 找 WAN 地址，然后找不到。
                    let up = match &z.uplink {
                        None => "net:wan".to_string(),
                        Some(u) => format!("net:{u}"),
                    };
                    edges.push(Edge {
                        from: up,
                        to: host_id.clone(),
                        kind: "attach".into(),
                        label: Some("eth1".into()),
                        port: Some("uplink".into()),
                        source_iface: None,
                        target_iface: None,
                        capture_bridge: None,
                    });
                    // 内网口：从网关指向本 zone 内网（数据流进去）
                    edges.push(Edge {
                        from: host_id,
                        to: inner,
                        kind: "attach".into(),
                        label: Some(format!("eth0 · {ip}")),
                        port: Some("inner".into()),
                        source_iface: None,
                        target_iface: None,
                        capture_bridge: None,
                    });
                } else {
                    edges.push(Edge {
                        from: inner,
                        to: host_id,
                        kind: "attach".into(),
                        label: Some(ip.to_string()),
                        port: Some("inner".into()),
                        source_iface: None,
                        target_iface: None,
                        capture_bridge: None,
                    });
                }
            }
        }

        // 镜像：一条主机一条虚线。源主机在它自己的 zone 桥上的那块网卡
        // （eth0）→ 监控机的抓包网卡，方向就是流量被复制过去的方向。
        for m in &spec.mirrors {
            edges.push(Edge {
                from: format!("host:{}:{}", m.from_zone, m.from_host),
                to: format!("host:{}:{}", m.to_zone, m.to_host),
                kind: "mirror".into(),
                label: Some(format!("eth0 → {}", m.to_iface)),
                port: None,
                source_iface: Some("eth0".into()),
                target_iface: Some(m.to_iface.clone()),
                capture_bridge: Some(m.capture_bridge.clone()),
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
    monitor: { use: "edge:probe" }
    hosts:
      gateway: { role: gateway, vmid: 100 }
      web: { role: server, vmid: 102 }
  edge:
    uplink: wan
    v4: 10.90.0.0/24
    hosts:
      probe: { role: monitor, vmid: 101 }
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts:
      gateway: { role: gateway }
      client1: { role: client }
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
        assert!(
            n.iter().filter(|x| x.id == "net:wan").count() == 1,
            "外网节点只应有一个（合成 zone 不再画第二个）"
        );
    }

    #[test]
    fn a_gateway_bridges_two_networks() {
        let (_, e) = run();
        assert!(has_edge(&e, "net:wan", "host:main:gateway", "attach"));
        assert!(has_edge(&e, "host:main:gateway", "net:main", "attach"));
        assert!(has_edge(&e, "net:main", "host:sub:gateway", "attach"));
        assert!(has_edge(&e, "host:sub:gateway", "net:sub", "attach"));
    }

    #[test]
    fn gateway_iface_labels_match_the_container() {
        // 容器里 eth0 是内网口、eth1 是上联口（net.rs 与 ensure_ct 都这么建）。
        // 图上写反了会让人照着去 `ip addr show eth0` 找 WAN 地址。
        let (_, e) = run();
        let up = e
            .iter()
            .find(|x| x.to == "host:main:gateway" && x.port.as_deref() == Some("uplink"))
            .unwrap();
        assert_eq!(up.label.as_deref(), Some("eth1"));
        let inner = e
            .iter()
            .find(|x| x.from == "host:main:gateway")
            .unwrap();
        assert!(inner.label.as_deref().unwrap_or("").starts_with("eth0"));
    }

    #[test]
    fn ordinary_hosts_attach_to_their_own_network() {
        let (_, e) = run();
        assert!(has_edge(&e, "net:main", "host:main:web", "attach"));
        assert!(has_edge(&e, "net:sub", "host:sub:client1", "attach"));
        assert!(!has_edge(&e, "net:wan", "host:main:web", "attach"));
    }

    #[test]
    fn mirrors_run_from_host_interface_to_monitor_interface() {
        let (_, e) = run();
        let m: Vec<&Edge> = e.iter().filter(|x| x.kind == "mirror").collect();
        // main 网络开了监控（抓 gateway + web），edge 网络没开（probe 自己不被抓）
        assert_eq!(m.len(), 2, "一台源主机一条虚线，实际 {m:?}");
        assert!(m.iter().all(|x| x.to == "host:edge:probe"));
        let gw = m.iter().find(|x| x.from == "host:main:gateway").unwrap();
        assert_eq!(gw.source_iface.as_deref(), Some("eth0"));
        assert_eq!(gw.target_iface.as_deref(), Some("eth1"));
        assert_eq!(gw.label.as_deref(), Some("eth0 → eth1"));
        assert_eq!(gw.capture_bridge.as_deref(), Some("mirmain"));
    }

    #[test]
    fn external_zone_hosts_hang_off_the_wan_bridge() {
        let y = r#"
defaults: { cores: 1, memory: 512, disk: 4, template: t, storage: s }
zones:
  lab:
    uplink: wan
    v4: 10.7.0.0/24
    monitor: { name: probe, mgmt: wan }
    hosts: { gateway: { role: gateway }, t1: { role: server } }
  core:
    uplink: wan
    v4: 10.9.0.0/24
    monitor: { use: "wan:probe" }
    hosts: { gateway: { role: gateway }, t2: { role: server } }
"#;
        let s = Spec::parse(y).unwrap().resolve_all("vmbr0").unwrap();
        let st = State {
            wan_bridge: "vmbr0",
            status: &|_| None,
            retired: &|_, _| false,
        };
        let (n, e) = build(&[("lab".to_string(), s)], &st);
        let probe = n.iter().find(|x| x.id == "host:wan:probe").unwrap();
        assert_eq!(probe.role.as_deref(), Some("monitor"));
        assert_eq!(probe.ipv4.as_deref(), Some("dhcp"));
        assert!(has_edge(&e, "net:wan", "host:wan:probe", "attach"));
        // 两个网络各给 probe 一块抓包口
        let t = probe.meta["capture"].as_array().unwrap();
        assert_eq!(t.len(), 2, "core 与 lab 各一块：{t:?}");
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
        assert_eq!(
            net.meta["monitor"],
            serde_json::json!("probe"),
            "网络上要能显示'这个网段归谁采集'"
        );
    }
}
