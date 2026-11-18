//! 网关策略落地：nftables 规则 + VPN（wireguard / openvpn）。
//!
//! 两条原则：
//! 1. **只用内核与成熟组件**：转发/映射/地址转换全走 nftables，VPN 走内核
//!    wireguard 与 openvpn，不自己实现隧道，也不在容器里跑一个常驻的
//!    "网关服务"——那样多一个进程、多一份状态、多一个能被改坏的真相源。
//! 2. **全量重建**：每次生成完整的 nft 规则表（delete table → 重建），
//!    而不是增量 add/delete。增量在"上一条规则写错、用户手工改过、上一次
//!    执行到一半失败"这几种情况下会漂移，而漂移的防火墙比没有防火墙更糟。
//!    重建走**一次 `nft -f` 原子批次**：整批要么全上、要么全不上，中间不会
//!    留下"表被删了但新规则没进来"的空窗。
//!
//! 脚本是纯函数输出，可以在没有 PVE 的机器上逐条断言（见文末测试）。

use crate::core::spec::{ResolvedPub, ResolvedVpn};

/// WireGuard 客户端网段（固定，写在服务端与客户端的 AllowedIPs 里）
pub const WG_NET: &str = "10.66.0.0/24";
pub const WG_GW: &str = "10.66.0.1";
pub const WG_PORT: u16 = 51820;
/// OpenVPN 客户端网段
pub const OVPN_NET: &str = "10.8.0.0/24";
pub const OVPN_PORT: u16 = 1194;

pub struct Policy<'a> {
    pub lan: &'a str,
    pub uplink: &'a str,
    pub zone_cidr: &'a str,
    pub nat: bool,
    pub egress: bool,
    pub v6_prefix: Option<&'a str>,
    pub pubs: &'a [ResolvedPub],
    pub vpn: &'a ResolvedVpn,
}

/// 生成整份 nft 规则表。
///
/// 输出是**一次 `nft -f` 原子批次**，不是一串 `nft add rule`。原因很实际：
/// 逐条 add 的写法在中间某条失败时，表已经被 `delete` 掉了，网关会处在
/// "没有任何规则"的状态——fail-open。而 nft 把一个批次的命令作为**单个
/// netlink 事务**提交，任何一条错就整批回滚，旧规则原样留着（fail-closed）。
pub fn nft_script(p: &Policy) -> String {
    let mut s = String::from("set -eu\n");
    // bootstrap 会尽力装 nftables；这里兜底并**明确失败**——网关没有防火墙
    // 规则等于没有隔离，静默跳过是最坏的结果。
    s.push_str(
        "if ! command -v nft >/dev/null 2>&1; then\n  \
         apt-get update -qq && apt-get install -y -qq --no-install-recommends nftables\n\
         fi\n",
    );
    // 批次里的 `delete table` 要求表存在，否则整批失败（首装时表还不存在）。
    // 先补一个空表：它没有任何链，不改变行为。
    s.push_str("nft list table inet labyard >/dev/null 2>&1 || nft add table inet labyard\n");
    s.push_str("nft -f - <<'LABYARD_NFT'\n");
    s.push_str("delete table inet labyard\n");
    s.push_str("table inet labyard {\n");

    // ── forward ──
    s.push_str("  chain forward {\n");
    s.push_str("    type filter hook forward priority filter; policy accept;\n");

    // 出网：关掉时先丢。**必须排在 established 之前**，否则已建立的连接
    // 还能继续出，用户看到的"已断网"是假的。
    let verdict = if p.egress { "accept" } else { "drop" };
    s.push_str(&format!(
        "    iifname \"{}\" oifname \"{}\" {}\n",
        p.lan, p.uplink, verdict
    ));

    s.push_str("    ct state established,related accept\n");
    if !p.pubs.is_empty() {
        // 映射进来的新连接：prerouting 已经改过目的地址，目标地址看起来像
        // "内网到内网"，会被后面的入向拒绝吃掉——必须按 conntrack 标记放行。
        s.push_str("    ct status dnat accept\n");
    }
    for iface in vpn_ifaces(p.vpn) {
        // VPN 客户端 ↔ 内网双向。回程一般是 established，但主动连客户端
        // （反向 shell 之类的实验）需要显式放行。
        s.push_str(&format!("    iifname \"{iface}\" accept\n"));
        s.push_str(&format!("    oifname \"{iface}\" accept\n"));
    }
    // 上联进来的新连接一律丢（映射的已在上面按 dnat 放行）
    s.push_str(&format!(
        "    iifname \"{}\" oifname \"{}\" drop\n",
        p.uplink, p.lan
    ));
    s.push_str("  }\n");

    // ── prerouting：入向端口映射 ──
    s.push_str("  chain prerouting {\n");
    s.push_str("    type nat hook prerouting priority -100; policy accept;\n");
    for pub_ in p.pubs {
        // inet 表里必须写 `dnat ip to`，否则 nft 报
        // "ip or ip6 must be specified with address for inet tables"。
        s.push_str(&format!(
            "    iifname \"{}\" {} dport {} dnat ip to {}:{}\n",
            p.uplink,
            pub_.proto.as_str(),
            pub_.port,
            pub_.to_ip,
            pub_.to_port
        ));
    }
    s.push_str("  }\n");

    // ── postrouting：地址转换 ──
    s.push_str("  chain postrouting {\n");
    s.push_str("    type nat hook postrouting priority 100; policy accept;\n");
    if p.nat {
        if !p.pubs.is_empty() {
            // 映射进来的连接要再 SNAT 一次（源改成网关的内网地址）。
            // 靶机**没有默认路由**，只有 on-link 的 zone 网段——不 SNAT 的话
            // 它的 SYN-ACK 发不出去，端口映射永远连不上。这一条是实测撞出来的：
            // DNAT 抓包能看到包到了靶机，但对面一个字节都不回。
            s.push_str(&format!("    oifname \"{}\" ct status dnat masquerade\n", p.lan));
        }
        s.push_str(&format!(
            "    ip saddr {} oifname \"{}\" masquerade\n",
            p.zone_cidr, p.uplink
        ));
        if let Some(v6) = p.v6_prefix {
            s.push_str(&format!(
                "    ip6 saddr {v6} oifname \"{}\" masquerade\n",
                p.uplink
            ));
        }
        if p.vpn.wireguard {
            s.push_str(&format!(
                "    ip saddr {WG_NET} oifname \"{}\" masquerade\n",
                p.uplink
            ));
        }
        if p.vpn.openvpn {
            s.push_str(&format!(
                "    ip saddr {OVPN_NET} oifname \"{}\" masquerade\n",
                p.uplink
            ));
        }
    }
    s.push_str("  }\n");
    s.push_str("}\n");
    s.push_str("LABYARD_NFT\n");

    s.push_str("echo \"[labgw] nft 规则已重建（原子批次）\"\n");
    s
}

fn vpn_ifaces(v: &ResolvedVpn) -> Vec<&'static str> {
    let mut out = Vec::new();
    if v.wireguard {
        out.push("wg0");
    }
    if v.openvpn {
        out.push("tun0");
    }
    out
}

/// VPN 落地脚本：装包 → 生成/复用密钥 → 写配置 → 起服务；关掉时停服务。
///
/// 两个方向都要在同一个脚本里做完，因为它就是"把规格里的 vpn 段同步到容器"
/// 的那一步——只做开启，关闭就变成了"规格里关了、容器里还开着"。
///
/// 密钥**只存在容器里**（`/etc/wireguard/server.key`、`/etc/openvpn/easy-rsa/pki`）：
/// 平台不持有私钥，下载客户端配置时现读。容器删了，密钥随之消失——VPN 与
/// 网关同生命周期，这是对的语义，也避免"库里躺着私钥"这种负债。
pub fn vpn_script(p: &Policy) -> String {
    let mut s = String::from("set -eu\n");
    let clients = p.vpn.clients.join(" ");
    // "10.66.0.0/24" → "10.66.0"，客户端地址就是 .2、.3……
    let wg_base = WG_NET
        .split('/')
        .next()
        .unwrap_or("10.66.0.0")
        .rsplit_once('.')
        .map(|(head, _)| head)
        .unwrap_or("10.66.0")
        .to_string();

    if p.vpn.wireguard {
        s.push_str(&format!(
            r#"# ── WireGuard ──
if ! command -v wg >/dev/null 2>&1; then
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends wireguard-tools
fi
# 无 tun 的容器起不来 wg0。这不是"尽力而为"能绕过去的，直接停在这里并
# 给出可执行的修法：labyard 的 vpn 步骤会先用 pct set 补上这个设备。
if [ ! -e /dev/net/tun ]; then
  echo "缺少 /dev/net/tun：容器需要 dev0: /dev/net/tun（labyard 会自动添加并重启容器）" >&2
  exit 3
fi
mkdir -p /etc/wireguard/clients
chmod 700 /etc/wireguard
[ -s /etc/wireguard/server.key ] || (umask 077; wg genkey > /etc/wireguard/server.key)
SRV_PRIV=$(cat /etc/wireguard/server.key)
SRV_PUB=$(printf '%s' "$SRV_PRIV" | wg pubkey)
ENDPOINT=$(ip -4 -o addr show {uplink} | awk '{{print $4}}' | cut -d/ -f1 | head -1)
CONF=/etc/wireguard/wg0.conf
{{
  echo '[Interface]'
  echo 'Address = {wg_gw}/24'
  echo 'ListenPort = {wg_port}'
  echo "PrivateKey = $SRV_PRIV"
  echo "PostUp = nft add rule inet labyard forward iifname wg0 accept 2>/dev/null || true"
  i=2
  for c in {clients}; do
    keyf=/etc/wireguard/clients/$c.key
    [ -s "$keyf" ] || (umask 077; wg genkey > "$keyf")
    cp=$(printf '%s' "$(cat "$keyf")" | wg pubkey)
    echo ''
    echo "[Peer]  # $c"
    echo "PublicKey = $cp"
    echo "AllowedIPs = {wg_base}.$i/32"
    cat > /etc/wireguard/clients/$c.conf <<EOF
[Interface]
PrivateKey = $(cat "$keyf")
Address = {wg_base}.$i/24

[Peer]
PublicKey = $SRV_PUB
Endpoint = $ENDPOINT:{wg_port}
AllowedIPs = {zone_cidr}
PersistentKeepalive = 25
EOF
    i=$((i+1))
  done
}} > "$CONF"
chmod 600 "$CONF"
systemctl enable wg-quick@wg0 >/dev/null 2>&1 || true
systemctl restart wg-quick@wg0
systemctl is-active --quiet wg-quick@wg0
echo "[labgw] wireguard 已启动，客户端: {clients}"
"#,
            uplink = p.uplink,
            wg_gw = WG_GW,
            wg_port = WG_PORT,
            clients = clients,
            wg_base = wg_base,
            zone_cidr = p.zone_cidr,
        ));
    } else {
        // 关闭要真的落地：停单元、删链路。密钥留在容器里（重新打开时复用，
        // 客户端配置因此不会作废）。
        s.push_str(
            "# ── WireGuard：关闭 ──\n\
             systemctl disable --now wg-quick@wg0 2>/dev/null || true\n\
             ip link show wg0 >/dev/null 2>&1 && ip link del wg0 2>/dev/null || true\n\
             echo \"[labgw] wireguard 已关闭\"\n",
        );
    }

    if p.vpn.openvpn {
        let (net, mask) = v4_net_mask(p.zone_cidr);
        s.push_str(&format!(
            r#"# ── OpenVPN ──
if ! command -v openvpn >/dev/null 2>&1 || [ ! -d /usr/share/easy-rsa ]; then
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends openvpn easy-rsa
fi
if [ ! -e /dev/net/tun ]; then
  echo "缺少 /dev/net/tun：容器需要 dev0: /dev/net/tun（labyard 会自动添加并重启容器）" >&2
  exit 3
fi
ER=/etc/openvpn/easy-rsa
mkdir -p "$ER"
# easy-rsa 的脚本在 /usr/share/easy-rsa；拷到 /etc 下再跑，升级包不会覆盖 pki
[ -x "$ER/easyrsa" ] || cp -r /usr/share/easy-rsa/* "$ER/"
cd "$ER"
export EASYRSA_BATCH=1
export EASYRSA_ALGO=ec
export EASYRSA_CURVE=secp384r1
[ -d pki ] || ./easyrsa init-pki >/dev/null
[ -s pki/ca.crt ] || {{ EASYRSA_REQ_CN=labyard-lab-ca ./easyrsa build-ca nopass >/dev/null; }}
[ -s pki/issued/server.crt ] || ./easyrsa build-server-full server nopass >/dev/null
for c in {clients}; do
  [ -s "pki/issued/$c.crt" ] || ./easyrsa build-client-full "$c" nopass >/dev/null
done
# tls-crypt 用共享密钥，省掉单独的 tls-auth 方向参数
[ -s /etc/openvpn/tc.key ] || openvpn --genkey secret /etc/openvpn/tc.key
mkdir -p /etc/openvpn/server
cat > /etc/openvpn/server/labyard.conf <<EOF
port {ovpn_port}
proto udp
dev tun
topology subnet
ca $ER/pki/ca.crt
cert $ER/pki/issued/server.crt
key $ER/pki/private/server.key
dh none
ecdh-curve secp384r1
tls-crypt /etc/openvpn/tc.key
server {ovpn_net} 255.255.255.0
push "route {net} {mask}"
keepalive 10 60
persist-key
persist-tun
verb 3
EOF
systemctl enable openvpn-server@labyard >/dev/null 2>&1 || true
systemctl restart openvpn-server@labyard
systemctl is-active --quiet openvpn-server@labyard
echo "[labgw] openvpn 已启动，客户端: {clients}"
"#,
            clients = clients,
            ovpn_port = OVPN_PORT,
            ovpn_net = OVPN_NET,
            net = net,
            mask = mask,
        ));
    } else {
        s.push_str(
            "# ── OpenVPN：关闭 ──\n\
             systemctl disable --now openvpn-server@labyard 2>/dev/null || true\n\
             echo \"[labgw] openvpn 已关闭\"\n",
        );
    }

    s
}

/// `10.10.1.0/24` → (`10.10.1.0`, `255.255.255.0`)。openvpn 的 push route
/// 只认点分掩码，不认 CIDR。
pub fn v4_net_mask(cidr: &str) -> (String, String) {
    let (ip, len) = cidr.split_once('/').unwrap_or((cidr, "24"));
    let plen: u32 = len.parse().unwrap_or(24).min(32);
    let mask = if plen == 0 { 0 } else { u32::MAX << (32 - plen) };
    let m = format!(
        "{}.{}.{}.{}",
        (mask >> 24) & 0xff,
        (mask >> 16) & 0xff,
        (mask >> 8) & 0xff,
        mask & 0xff
    );
    (ip.to_string(), m)
}

/// 客户端配置里的 `Endpoint`。下载时用规格里的上联桥管理地址不可靠，
/// 所以由网关自己算（见 vpn_script），这里只放生成逻辑的说明位。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::spec::Proto;

    fn policy<'a>(pubs: &'a [ResolvedPub], vpn: &'a ResolvedVpn) -> Policy<'a> {
        Policy {
            lan: "eth0",
            uplink: "eth1",
            zone_cidr: "10.10.1.0/24",
            nat: true,
            egress: true,
            v6_prefix: None,
            pubs,
            vpn,
        }
    }

    fn pub_(proto: Proto, port: u16, ip: &str, to_port: u16) -> ResolvedPub {
        ResolvedPub {
            proto,
            port,
            to_ip: ip.into(),
            to_port,
        }
    }

    #[test]
    fn pub_becomes_dnat_and_is_allowed_through_forward() {
        let pubs = vec![pub_(Proto::Tcp, 2222, "10.10.1.20", 22)];
        let vpn = ResolvedVpn::default();
        let s = nft_script(&policy(&pubs, &vpn));
        assert!(
            s.contains("iifname \"eth1\" tcp dport 2222 dnat ip to 10.10.1.20:22"),
            "端口映射要落到 prerouting 的 DNAT 上:\n{s}"
        );
        assert!(
            s.contains("ct status dnat accept"),
            "DNAT 的连接必须在 forward 里放行，否则只有第一个包被改地址、后面的被丢"
        );
        assert!(
            s.contains("oifname \"eth0\" ct status dnat masquerade"),
            "映射到靶机时必须 SNAT 回网关：靶机没有默认路由，不回程就永远连不上"
        );
    }

    #[test]
    fn inbound_is_dropped_by_default() {
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn::default();
        let s = nft_script(&policy(&pubs, &vpn));
        assert!(s.contains("iifname \"eth1\" oifname \"eth0\" drop"));
        assert!(
            !s.contains("ct status dnat accept"),
            "没有映射时不该有无用的放行规则"
        );
    }

    #[test]
    fn egress_off_drops_lan_to_wan_before_established() {
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn::default();
        let mut p = policy(&pubs, &vpn);
        p.egress = false;
        let s = nft_script(&p);
        let drop = s.find("iifname \"eth0\" oifname \"eth1\" drop").expect("要有出网丢弃");
        let est = s.find("ct state established,related accept").unwrap();
        assert!(drop < est, "出网丢弃必须排在 established 之前，否则旧连接还能出");
        assert!(!s.contains("iifname \"eth0\" oifname \"eth1\" accept"));
    }

    #[test]
    fn nat_off_means_no_masquerade_at_all() {
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn::default();
        let mut p = policy(&pubs, &vpn);
        p.nat = false;
        let s = nft_script(&p);
        assert!(!s.contains("masquerade"), "nat=false 不该有任何 masquerade");
        assert!(s.contains("iifname \"eth0\" oifname \"eth1\" accept"), "但仍要放行转发");
    }

    #[test]
    fn vpn_subnets_are_natted_and_forwarded() {
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn {
            wireguard: true,
            openvpn: true,
            clients: vec!["lab".into()],
        };
        let s = nft_script(&policy(&pubs, &vpn));
        assert!(s.contains("iifname \"wg0\" accept"));
        assert!(s.contains("iifname \"tun0\" accept"));
        assert!(s.contains(&format!("ip saddr {WG_NET} oifname \"eth1\" masquerade")));
        assert!(s.contains(&format!("ip saddr {OVPN_NET} oifname \"eth1\" masquerade")));
    }

    #[test]
    fn vpn_script_mentions_tun_and_components() {
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn {
            wireguard: true,
            openvpn: true,
            clients: vec!["lab".into(), "red".into()],
        };
        let s = vpn_script(&policy(&pubs, &vpn));
        assert!(s.contains("/dev/net/tun"), "没有 tun 时要给出可执行的错误");
        assert!(s.contains("wg genkey"));
        assert!(s.contains("wg-quick@wg0"));
        assert!(s.contains("easyrsa build-client-full"));
        assert!(s.contains("openvpn-server@labyard"));
        assert!(s.contains("for c in lab red"));
        assert!(s.contains("push \"route 10.10.1.0 255.255.255.0\""));
    }

    #[test]
    fn vpn_off_actually_stops_the_units() {
        // 只把规格里的开关关掉、容器里 wg0 还在跑，是最糟的不一致：
        // 平台说没有，实际还开着。关闭必须是一条真实执行的命令。
        let pubs: Vec<ResolvedPub> = vec![];
        let vpn = ResolvedVpn::default();
        let s = vpn_script(&policy(&pubs, &vpn));
        assert!(
            s.contains("systemctl disable --now wg-quick@wg0"),
            "关闭时要停 wireguard 单元:\n{s}"
        );
        assert!(
            s.contains("systemctl disable --now openvpn-server@labyard"),
            "关闭时要停 openvpn 单元:\n{s}"
        );
        assert!(!s.contains("wg genkey"), "关闭时不该生成任何密钥");
    }

    #[test]
    fn net_mask_conversion() {
        assert_eq!(
            v4_net_mask("10.10.1.0/24"),
            ("10.10.1.0".into(), "255.255.255.0".into())
        );
        assert_eq!(
            v4_net_mask("10.20.0.0/16"),
            ("10.20.0.0".into(), "255.255.0.0".into())
        );
    }
}
