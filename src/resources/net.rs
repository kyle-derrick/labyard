//! 容器内的网络配置脚本生成。
//!
//! 全部是**纯函数**：给定参数返回一段 sh 脚本，脚本本身幂等（重复执行结果相同）。
//! 这样可以在没有 PVE 的机器上把每种组合的输出快照下来做回归测试。

use crate::core::plan::StepKind;

/// 网关容器：转发 + NAT66 + 可选 NAT44。
pub fn gateway_script(
    v4_cidr: &str,
    v6_prefix: Option<&str>,
    uplink: &str,
    nat44: bool,
) -> String {
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str("# 网关：内网口 = eth0，上联口 = eth1\n");
    s.push_str("ip link set eth0 up\n");
    s.push_str("ip link set eth1 up\n");
    s.push_str(&format!(
        "ip addr replace {v4_cidr} dev eth0\n"
    ));
    if let Some(p) = v6_prefix {
        // ::1 作为网关地址
        let base = p.trim_end_matches("/64").trim_end_matches(':');
        s.push_str(&format!("ip -6 addr replace {base}::1/64 dev eth0\n"));
    }
    // 上联口走 DHCP（PVE 的 wan 桥通常有 DHCP 或用户自己配）
    s.push_str("if ! ip -4 addr show eth1 | grep -q 'inet '; then\n");
    s.push_str("  (command -v dhclient >/dev/null && dhclient -1 eth1) || true\n");
    s.push_str("fi\n");
    s.push_str(&format!(
        "ip route replace default via $(ip -4 route show dev {uplink} | awk '/default/{{print $3; exit}}') dev {uplink} 2>/dev/null || true\n"
    ));

    s.push_str("sysctl -qw net.ipv4.ip_forward=1\n");
    s.push_str("sysctl -qw net.ipv6.conf.all.forwarding=1\n");

    s.push_str("nft delete table inet labyard 2>/dev/null || true\n");
    s.push_str("nft add table inet labyard\n");
    s.push_str("nft 'add chain inet labyard forward { type filter hook forward priority 0; policy accept; }'\n");
    s.push_str("nft 'add chain inet labyard postrouting { type nat hook postrouting priority 100; }'\n");
    if nat44 {
        s.push_str(&format!(
            "nft add rule inet labyard postrouting ip saddr {v4_cidr} oifname \"{uplink}\" masquerade\n"
        ));
    }
    if let Some(p) = v6_prefix {
        let base = p.trim_end_matches("/64").trim_end_matches(':');
        s.push_str(&format!(
            "nft add rule inet labyard postrouting ip6 saddr {base}::/64 oifname \"{uplink}\" masquerade\n"
        ));
    }
    s.push_str("# 出向放行，入向默认丢弃（除已建立连接）\n");
    s.push_str("nft add rule inet labyard forward ct state established,related accept\n");
    s.push_str(&format!(
        "nft add rule inet labyard forward iifname \"{uplink}\" oifname \"eth0\" drop\n"
    ));
    s
}

/// 普通主机：静态地址；靶机不给默认路由（避免从内网直接出网）。
pub fn host_script(ipv4: &str, gateway: Option<&str>, v6_prefix: Option<&str>, default_route: bool) -> String {
    let mut s = String::new();
    s.push_str("set -eu\n");
    s.push_str("ip link set eth0 up\n");
    s.push_str(&format!("ip addr replace {ipv4} dev eth0\n"));
    if default_route {
        if let Some(gw) = gateway {
            s.push_str(&format!("ip route replace default via {gw} dev eth0\n"));
        }
    } else {
        s.push_str("# 靶机：不要默认路由\n");
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
    s.push_str("cat > /etc/resolv.conf <<'EOF'\n");
    s.push_str("nameserver 223.5.5.5\nnameserver 119.29.29.29\n");
    s.push_str("EOF\n");
    s
}

/// 把 `Net` 步骤渲染成脚本。
pub fn for_step(kind: &StepKind, v4_cidr: &str, uplink_iface: &str, nat44: bool) -> Option<String> {
    match kind {
        StepKind::Net {
            ipv4,
            gateway,
            ipv6_prefix,
            default_route,
            ..
        } => Some(host_script(
            ipv4,
            gateway.as_deref(),
            ipv6_prefix.as_deref(),
            *default_route,
        )),
        StepKind::Container { .. } => {
            // 网关在建机阶段还没配地址，这里只返回需要的信息占位
            let _ = (v4_cidr, uplink_iface, nat44);
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_has_no_default_route_when_asked() {
        let s = host_script("10.10.1.20/24", Some("10.10.1.1"), None, false);
        assert!(s.contains("ip route del default"), "靶机必须删默认路由");
        assert!(!s.contains("ip route replace default"));
    }

    #[test]
    fn host_has_default_route_when_asked() {
        let s = host_script("10.10.1.20/24", Some("10.10.1.1"), None, true);
        assert!(s.contains("ip route replace default via 10.10.1.1"));
    }

    #[test]
    fn ipv6_suffix_is_derived_and_stable() {
        let a = host_script("10.10.1.20/24", None, Some("fd00:1::/64"), true);
        let b = host_script("10.10.1.20/24", None, Some("fd00:1::/64"), true);
        assert!(a.contains("fd00:1::14/64"), "20 的十六进制是 14，实际: {a}");
        assert_eq!(a, b);
    }

    #[test]
    fn gateway_nat66_present_when_v6_on() {
        let s = gateway_script("10.10.1.0/24", Some("fd00:1::/64"), "eth1", false);
        assert!(s.contains("masquerade"));
        assert!(s.contains("ip6 saddr fd00:1::/64"));
        assert!(!s.contains("ip saddr 10.10.1.0/24 oifname"));
    }

    #[test]
    fn gateway_drops_inbound_from_uplink() {
        let s = gateway_script("10.10.1.0/24", None, "eth1", true);
        assert!(s.contains("iifname \"eth1\" oifname \"eth0\" drop"));
    }
}
