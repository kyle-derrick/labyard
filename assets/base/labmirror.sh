#!/bin/sh
# labmirror@<instance> —— 宿主侧流量镜像：把**一台源主机**的网卡流量复制到
# 监控机的抓包口。tc mirred 是内核标准机制，这里只是把它的配置从一次性命令
# 变成可 restart、可自愈的单元。
#
# 为什么按 veth 而不是按桥：
#   按桥 mirror（`tc filter add dev <bridge> ingress`）抓的是"整个广播域"，
#   于是同一个网络上每加一台被采集的机器，就多跑一份完全相同的规则，而
#   监控机上根本分不清这个包来自哪台机器。按 `veth<vmid>i*` 逐口做，才是
#   "这台主机的接口 → 监控机的那块口"这个真实语义，也和界面上的虚线一致。
#
# 自愈的关键：口名不写死。容器重建后 veth 编号会变（veth100i0 → veth105i0），
# 写死的话规则挂不上却**不报错**，表现为"镜像单元 active 但没有流量"。
# 所以每次启动按 vmid 前缀现探，探不到就短暂重试（宿主重启时容器还在起）。
set -eu

INSTANCE="${1:-${INSTANCE:-}}"
[ -n "$INSTANCE" ] || { echo "用法: labmirror <instance>" >&2; exit 2; }

CONF="/etc/labyard/mirror/${INSTANCE}.conf"
[ -f "$CONF" ] || { echo "缺少配置 $CONF" >&2; exit 2; }
# shellcheck disable=SC1090
. "$CONF"

: "${SRC_BRIDGE:?配置缺 SRC_BRIDGE}"
: "${SOURCE_VMID:?配置缺 SOURCE_VMID}"
: "${TARGET_VMID:?配置缺 TARGET_VMID}"
: "${CAPTURE_BRIDGE:?配置缺 CAPTURE_BRIDGE}"

log() { printf '[labmirror %s] %s\n' "$INSTANCE" "$*"; }

# 找桥上的某个成员口：按 veth<vmid>i<N> 前缀匹配。
# PVE 的 veth 命名是 `veth<vmid>i<index>`，同一容器可能有多块（i0、i1…），
# 这里取第一块——源主机的采集口就是它的 eth0（在 zone 桥上的那块）。
iface_of() {
  # $1 = 桥名, $2 = vmid
  ip -o link show master "$1" 2>/dev/null \
    | awk -F': ' '{print $2}' \
    | sed 's/@.*//' \
    | grep -E "^veth${2}i[0-9]+$" \
    | head -1 || true
}

# 宿主重启时容器还在启动，veth 还没建出来。等一会儿而不是立刻失败：
# 失败会让 systemd 记一次 start-limit，之后就算口出来了也不再启动。
SRC_IF=""
DST_IF=""
i=0
while [ "$i" -lt 20 ]; do
  SRC_IF=$(iface_of "$SRC_BRIDGE" "$SOURCE_VMID")
  DST_IF=$(iface_of "$CAPTURE_BRIDGE" "$TARGET_VMID")
  [ -n "$SRC_IF" ] && [ -n "$DST_IF" ] && break
  i=$((i + 1))
  sleep 1.5
done

[ -n "$SRC_IF" ] || { log "源主机 $SOURCE_VMID 在 $SRC_BRIDGE 上没有网口（容器停着？）"; exit 1; }
[ -n "$DST_IF" ] || { log "监控机 $TARGET_VMID 在 $CAPTURE_BRIDGE 上没有网口"; exit 1; }
log "mirroring $SRC_IF ($SRC_BRIDGE) -> $DST_IF ($CAPTURE_BRIDGE, ${TARGET_IFACE:-?})"

# ── 双向都要抓，且按**源口**聚合 ──
# 源主机 veth 的 ingress = 容器发出去的包，egress = 发给容器的包。
# 只抓 ingress 会漏掉一半（服务端的响应包全在 egress 上），这在"只看到一个
# 方向的会话"时最难排查：抓包工具里握手只有 SYN。
#
# 一台主机可能被**多处**采集：整网监控（网络 → 监控机）+ 一条精确连线
# （t1 → cache）。这里不能"每个实例挂一条规则"：matchall 是终止式匹配，
# 同一条链上两条 matchall 只有优先级高的那条会跑，后加的那条计数永远是 0
# ——真机上就是"面板两处都写着采集中，只有一处真在收包"。
#
# 正确做法：把同一源口的**全部**目标合成一条 filter，串多个 mirred action。
# 任何实例重启都会读全量 conf 重建这条规则，因此天然幂等；并发重启也不会
# 打架（最后写的都是同一份内容）。
MY_BRIDGE="$SRC_BRIDGE"
MY_VMID="$SOURCE_VMID"
DSTS=""
for c in /etc/labyard/mirror/*.conf; do
  d=$(
    # shellcheck disable=SC1090
    . "$c"
    [ "$SRC_BRIDGE" = "$MY_BRIDGE" ] && [ "$SOURCE_VMID" = "$MY_VMID" ] || exit 0
    iface_of "$CAPTURE_BRIDGE" "$TARGET_VMID"
  )
  [ -n "$d" ] && DSTS="$DSTS $d"
done
DSTS=$(printf '%s\n' $DSTS | sort -u | tr '\n' ' ')
[ -n "$DSTS" ] || { log "源口 $SRC_IF 找不到任何可用的抓包口"; exit 1; }

PREF=$(printf '%s' "$SRC_BRIDGE/$SOURCE_VMID" | cksum | cut -d' ' -f1)
PREF=$((PREF % 59000 + 100))

if ! tc qdisc show dev "$SRC_IF" | grep -q clsact; then
  tc qdisc add dev "$SRC_IF" clsact
fi

# 这个口上的镜像规则由本脚本独占：先全清再按当前目标集合重建。
# 清的范围只有这一块 veth，别的源口的规则不受影响。
tc filter del dev "$SRC_IF" ingress 2>/dev/null || true
tc filter del dev "$SRC_IF" egress 2>/dev/null || true

ACTIONS=""
for d in $DSTS; do
  ACTIONS="$ACTIONS action mirred egress mirror dev $d"
done
# shellcheck disable=SC2086
tc filter add dev "$SRC_IF" ingress pref "$PREF" matchall $ACTIONS
# shellcheck disable=SC2086
tc filter add dev "$SRC_IF" egress pref "$PREF" matchall $ACTIONS

# 装完当场验一次：`tc filter add` 成功但方向挂错时，单元照样 active，
# 面板上写着"采集中"却一个包都收不到。这里失败就让单元红着，让巡检去补。
# 文本取自 `tc filter show`：`action order 1: mirred (Egress Mirror to device veth112i1)`
for d in $DSTS; do
  tc filter show dev "$SRC_IF" ingress | grep -q "Mirror to device $d" \
    || { log "ingress 规则没挂上（$SRC_IF → $d）"; exit 1; }
  tc filter show dev "$SRC_IF" egress | grep -q "Mirror to device $d" \
    || { log "egress 规则没挂上（$SRC_IF → $d）"; exit 1; }
done

log "已挂 ingress+egress 镜像规则（pref $PREF；目标:$DSTS；排障：tc -s filter show dev $SRC_IF ingress）"
