#!/bin/sh
# labmirror@<instance> —— 宿主侧流量镜像：把源桥每个成员口的 ingress 复制到
# analysis 在 mirr 桥上的宿主侧口。tc mirred 是内核标准机制，这里只是把它
# 的配置从一次性命令变成可 restart、可自愈的单元。
#
# 自愈的关键：DST_IF 不写死 veth 名（analysis 重建后 veth1002i1 会变成
# veth1005i1，旧名字挂不上规则还不报错），每次启动按 MIRR_BRIDGE 现探。
set -eu

INSTANCE="${1:-${INSTANCE:-}}"
[ -n "$INSTANCE" ] || { echo "用法: labmirror <instance>" >&2; exit 2; }

CONF="/etc/labyard/mirror/${INSTANCE}.conf"
[ -f "$CONF" ] || { echo "缺少配置 $CONF" >&2; exit 2; }
# shellcheck disable=SC1090
. "$CONF"

: "${SRC_BRIDGE:?配置缺 SRC_BRIDGE}"
: "${MIRR_BRIDGE:=mirr}"

log() { printf '[labmirror %s] %s\n' "$INSTANCE" "$*"; }

# ── 目标口：mirr 桥上唯一（或第一个）成员口 ──
if [ -z "${DST_IF:-}" ]; then
  DST_IF=$(ip -o link show master "$MIRR_BRIDGE" 2>/dev/null | awk -F': ' '{print $2}' | sed 's/@.*//' | head -1 || true)
fi
[ -n "${DST_IF:-}" ] || { log "mirr 桥 $MIRR_BRIDGE 上没有成员口，无事可做"; exit 0; }
ip link show "$DST_IF" >/dev/null 2>&1 || { log "目标口 $DST_IF 不存在"; exit 1; }
log "mirroring $SRC_BRIDGE -> $DST_IF"

# ── 清理旧规则（幂等重启）──
tc qdisc del dev "$SRC_BRIDGE" clsact 2>/dev/null || true
tc qdisc add dev "$SRC_BRIDGE" clsact

# ── 每个成员口一条 ingress filter，mirred 到目标口 ──
count=0
for ifc in $(ip -o link show master "$SRC_BRIDGE" | awk -F': ' '{print $2}' | sed 's/@.*//'); do
  # 跳过目标口自己（同桥对打会成环）
  [ "$ifc" = "$DST_IF" ] && continue
  tc filter add dev "$SRC_BRIDGE" ingress matchall \
    action mirred egress mirror dev "$DST_IF" || log "给 $ifc 挂镜像失败"
  count=$((count + 1))
done
log "已镜像 $count 个口"

# 有成员口变化就退出让 systemd 重启（Restart=on-failure 不覆盖正常退出，
# 所以这里只用日志提示；真实变化由 apply 的 restart 触发）
