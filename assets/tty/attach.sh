#!/bin/sh
# ttyd 的启动命令。由 systemd 以 root 调用，环境变量来自 labyard：
#   LABYARD_VMID    要进入的容器 id
#   LABYARD_USER    已通过反代认证的用户（仅用于审计日志）
#
# 安全约束：
# - vmid 必须是数字，防止命令注入
# - 只允许 login 到存在且 running 的容器
# - 不提供 shell 参数透传（ttyd 已用 -a 绑定）
set -eu

VMID="${LABYARD_VMID:?LABYARD_VMID 未设置}"
USER_NAME="${LABYARD_USER:-unknown}"

case "$VMID" in
  ''|*[!0-9]*)
    echo "非法 vmid: $VMID" >&2
    exit 1
    ;;
esac

status=$(pct status "$VMID" 2>/dev/null | awk '{print $2}')
if [ "$status" != "running" ]; then
  echo "容器 $VMID 状态为 '$status'，不是 running" >&2
  exit 1
fi

logger -t labyard-tty "user=$USER_NAME vmid=$VMID"
exec pct exec "$VMID" -- login -f root
