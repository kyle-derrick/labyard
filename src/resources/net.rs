//! 容器内的网络配置脚本生成。
//!
//! 全部是**纯函数**：给定参数返回一段 sh 脚本，脚本本身幂等（重复执行结果相同）。
//! 这样可以在没有 PVE 的机器上把每种组合的输出快照下来做回归测试。
//!
//! 分工：这里只负责**地址/路由/解析器**；防火墙与端口映射在
//! `resources::gateway`（nft 整表重建）。分开是因为两者的失败模式不同——
//! 地址配错是"连不上"，规则配错是"连得上但不该连的也能连"。

/// 网关容器：接口地址、默认路由、转发开关、解析器。
///
/// `gw_v4` 是网关自己的地址（10.77.0.1/24），`uplink` 是上联网卡名（eth1）。
/// 真机实测教训：早期把网段地址写上了接口，网关拿不到 .1 这个身份。
#[allow(dead_code)] // 生产走 `gateway_addressing_routed`（带下游路由）；这个是无路由的便捷封装，仅测试用
pub fn gateway_addressing(
    gw_v4: &str,
    v6_prefix: Option<&str>,
    uplink: &str,
    uplink_ip: Option<&str>,
    uplink_gw: Option<&str>,
    dns: &[String],
) -> String {
    gateway_addressing_routed(gw_v4, v6_prefix, uplink, uplink_ip, uplink_gw, &[], dns)
}

/// 带下游路由的版本。
///
/// `routes` 是 `(目标网段, 下一跳)`，出口固定 `eth0`：下游 zone 是挂在**本
/// zone 网段**上的，不是挂在上联口。没有这些路由，父 zone 与兄弟 zone 都
/// 到不了子网段——默认路由只朝上，树只能单向走。
pub fn gateway_addressing_routed(
    gw_v4: &str,
    v6_prefix: Option<&str>,
    uplink: &str,
    uplink_ip: Option<&str>,
    uplink_gw: Option<&str>,
    routes: &[(String, String)],
    dns: &[String],
) -> String {
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str("# 网关：内网口 = eth0，上联口 = eth1\n");
    s.push_str("ip link set eth0 up\n");
    s.push_str(&format!("ip link set {uplink} up\n"));
    s.push_str(&format!("ip addr replace {gw_v4} dev eth0\n"));
    if let Some(p) = v6_prefix {
        // ::1 作为网关地址
        let base = p.trim_end_matches("/64").trim_end_matches(':');
        s.push_str(&format!("ip -6 addr replace {base}::1/64 dev eth0\n"));
    }
    match (uplink_ip, uplink_gw) {
        // 嵌套 zone：上联是平台自己的桥，没有 DHCP。直接静态配地址 + 默认路由。
        (Some(ip), Some(gw)) => {
            s.push_str(&format!("ip addr replace {ip} dev {uplink}\n"));
            s.push_str(&format!("ip route replace default via {gw} dev {uplink}\n"));
        }
        (Some(ip), None) => {
            s.push_str(&format!("ip addr replace {ip} dev {uplink}\n"));
        }
        // 上联是真实外网桥：通常有 DHCP 或用户自己配
        _ => {
            s.push_str(&format!(
                "if ! ip -4 addr show {uplink} | grep -q 'inet '; then\n  \
                 (command -v dhclient >/dev/null && dhclient -1 {uplink}) || true\nfi\n"
            ));
            s.push_str(&format!(
                "ip route replace default via $(ip -4 route show dev {uplink} | awk '/default/{{print $3; exit}}') dev {uplink} 2>/dev/null || true\n"
            ));
        }
    }
    // 下游 zone 的静态路由。出口是 eth0（本 zone 网段），不是上联口：
    // 子网关的上联地址就长在这座桥上。
    for (cidr, via) in routes {
        s.push_str(&format!("ip route replace {cidr} via {via} dev eth0\n"));
    }
    s.push_str("sysctl -qw net.ipv4.ip_forward=1\n");
    // 网关**不许发 ICMP redirect**。
    //
    // 下游路由的下一跳和入接口在同一条桥上（父 zone 网段），内核默认会给
    // 源主机回一个 redirect："下次直接发给它"。主机照办之后包就绕过了本网关：
    // 本 zone 的转发策略、出网开关、镜像规则全部落空，而且子网关会把这种
    // "从 uplink 直接进来的包"当入向丢弃（`iif eth1 oif eth0 drop`），
    // 现象是"父 zone 到子网段 100% 丢包，子网段却能出来"。
    s.push_str("sysctl -qw net.ipv4.conf.all.send_redirects=0\n");
    s.push_str("sysctl -qw net.ipv4.conf.eth0.send_redirects=0\n");

    s.push_str("sysctl -qw net.ipv6.conf.all.forwarding=1\n");
    // 网关自己也要能解析（apt 装包、provisioner 用）
    s.push_str(&resolv_conf(dns));
    s
}

/// 普通主机：静态地址；不给默认路由（避免从内网直接出网）。
pub fn host_script(
    ipv4: &str,
    gateway: Option<&str>,
    v6_prefix: Option<&str>,
    default_route: bool,
    dns: &[String],
) -> String {
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str("ip link set eth0 up\n");
    s.push_str(&format!("ip addr replace {ipv4} dev eth0\n"));
    if default_route {
        if let Some(gw) = gateway {
            s.push_str(&format!("ip route replace default via {gw} dev eth0\n"));
        }
    } else {
        s.push_str("# 内网主机：不要默认路由\n");
        s.push_str("ip route del default 2>/dev/null || true\n");
    }
    if let Some(p) = v6_prefix {
        let base = p.trim_end_matches("/64").trim_end_matches(':');
        // 用最后一段 IPv4 的十进制值派生接口标识，保证同网段唯一且稳定
        let suffix = ipv4
            .split('/')
            .next()
            .unwrap_or("0.0.0.0")
            .rsplit('.')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(2);
        s.push_str(&format!("ip -6 addr replace {base}::{suffix:x}/64 dev eth0\n"));
        if default_route {
            if let Some(_gw) = gateway {
                s.push_str(&format!("ip -6 route replace default via {base}::1 dev eth0\n"));
            }
        } else {
            s.push_str("ip -6 route del default 2>/dev/null || true\n");
        }
    }
    s.push_str(&resolv_conf(dns));
    s
}

/// 解析器列表。spec 里写了就用 spec 的——实验网经常要把内网主机指向内网 DNS
/// 才能解析到假域名；没写才退回公共解析器。
///
/// 之所以在这里写而不是靠 `pct create --nameserver`：net 步骤在此之后执行，
/// 会整份覆盖 `/etc/resolv.conf`，PVE 写的那份留不下来。一个地方说了算。
fn resolv_conf(dns: &[String]) -> String {
    let list: Vec<&str> = if dns.is_empty() {
        vec!["223.5.5.5", "119.29.29.29"]
    } else {
        dns.iter().map(|s| s.as_str()).collect()
    };
    let mut s = String::from("cat > /etc/resolv.conf <<'EOF'\n");
    for ns in list {
        s.push_str(&format!("nameserver {ns}\n"));
    }
    s.push_str("EOF\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_has_no_default_route_when_asked() {
        let s = host_script("10.10.1.20/24", Some("10.10.1.1"), None, false, &[]);
        assert!(s.contains("ip route del default"), "内网主机必须删默认路由");
        assert!(!s.contains("ip route replace default"));
    }

    #[test]
    fn host_has_default_route_when_asked() {
        let s = host_script("10.10.1.20/24", Some("10.10.1.1"), None, true, &[]);
        assert!(s.contains("ip route replace default via 10.10.1.1"));
    }

    #[test]
    fn ipv6_suffix_is_derived_and_stable() {
        let a = host_script("10.10.1.20/24", None, Some("fd00:1::/64"), true, &[]);
        let b = host_script("10.10.1.20/24", None, Some("fd00:1::/64"), true, &[]);
        assert!(a.contains("fd00:1::14/64"), "20 的十六进制是 14，实际: {a}");
        assert_eq!(a, b);
    }

    #[test]
    fn dns_comes_from_the_spec() {
        // 写了 dns 就必须用写的——实验网里内网主机要靠内网 DNS 才能解析假域名
        let dns = vec!["10.10.1.10".to_string(), "1.1.1.1".to_string()];
        let h = host_script("10.10.1.20/24", Some("10.10.1.1"), None, true, &dns);
        assert!(h.contains("nameserver 10.10.1.10"));
        assert!(h.contains("nameserver 1.1.1.1"));
        assert!(!h.contains("223.5.5.5"), "显式 dns 不应被公共解析器混进来");
        let g = gateway_addressing("10.10.1.1/24", None, "eth1", None, None, &dns);
        assert!(g.contains("nameserver 10.10.1.10"), "网关自己也要能解析");
    }

    #[test]
    fn default_dns_when_unspecified() {
        let h = host_script("10.10.1.20/24", None, None, false, &[]);
        assert!(h.contains("nameserver 223.5.5.5"));
    }

    #[test]
    fn gateway_uses_its_own_ip_not_the_zone_cidr() {
        // 真机教训：早期把网段地址 10.10.1.0/24 写上接口——网段号当 IP 是错的，
        // 网关必须拿 .1 这个身份，主机的默认路由才有落点。
        let s = gateway_addressing("10.10.1.1/24", None, "eth1", None, None, &[]);
        assert!(s.contains("ip addr replace 10.10.1.1/24 dev eth0"));
        assert!(!s.contains("ip addr replace 10.10.1.0/24"), "网段号不该上接口");
    }

    #[test]
    fn gateway_turns_forwarding_on() {
        let s = gateway_addressing("10.10.1.1/24", Some("fd00:1::/64"), "eth1", None, None, &[]);
        assert!(s.contains("net.ipv4.ip_forward=1"));
        assert!(s.contains("net.ipv6.conf.all.forwarding=1"));
        assert!(s.contains("ip -6 addr replace fd00:1::1/64 dev eth0"));
        // ICMP redirect 会让主机绕开网关直连下一跳，转发策略与镜像一并失效
        assert!(
            s.contains("net.ipv4.conf.all.send_redirects=0"),
            "网关不能发 redirect:\n{s}"
        );
    }

    #[test]
    fn downstream_routes_go_out_the_lan_iface() {
        // 子网挂在**本 zone 网段**上，下一跳就是子网关的上联地址，
        // 出口是 eth0 而不是 uplink——写成 eth1 的话路由黑洞，
        // 表现是"父 zone 进不去子网，子网却能一路出来"。
        let routes = vec![
            ("10.30.1.0/24".to_string(), "10.77.0.253".to_string()),
            ("10.30.2.0/24".to_string(), "10.77.0.253".to_string()),
        ];
        let s = gateway_addressing_routed(
            "10.77.0.1/24",
            None,
            "eth1",
            Some("192.168.33.201/24"),
            Some("192.168.33.1"),
            &routes,
            &[],
        );
        assert!(
            s.contains("ip route replace 10.30.1.0/24 via 10.77.0.253 dev eth0"),
            "缺下游路由:\n{s}"
        );
        assert!(
            s.contains("ip route replace 10.30.2.0/24 via 10.77.0.253 dev eth0"),
            "孙网段也要指向直接子网关:\n{s}"
        );
        // 默认路由不受影响
        assert!(s.contains("ip route replace default via 192.168.33.1 dev eth1"));
    }

    #[test]
    fn no_downstream_routes_means_no_extra_lines() {
        let s = gateway_addressing("10.10.1.1/24", None, "eth1", None, None, &[]);
        assert!(!s.contains("ip route replace 10."), "没有下游就不该写路由:\n{s}");
    }
}
