#!/bin/sh
# labyard base bootstrap —— 在容器内执行，负责"这台机器能用了"这件事。
# 幂等：重复执行不改变结果。
#
# 由 labyard 通过 `pct exec` 推入并执行，参数通过环境变量传入。
set -eu

HOSTNAME_V="${LABYARD_HOSTNAME:-lab}"
ROLE="${LABYARD_ROLE:-server}"
IPV4="${LABYARD_IPV4:-}"
V6_PREFIX="${LABYARD_V6_PREFIX:-}"

log() { printf '[labyard] %s\n' "$*"; }

# ── 主机名 ──
current_host=$(cat /etc/hostname 2>/dev/null || echo "")
if [ "$current_host" != "$HOSTNAME_V" ]; then
  hostnamectl set-hostname "$HOSTNAME_V" 2>/dev/null || echo "$HOSTNAME_V" > /etc/hostname
  log "hostname -> $HOSTNAME_V"
fi
if ! grep -q "$HOSTNAME_V" /etc/hosts 2>/dev/null; then
  echo "127.0.1.1 $HOSTNAME_V" >> /etc/hosts
fi

# ── 时区 ──
if [ -f /usr/share/zoneinfo/Asia/Shanghai ]; then
  ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime
  echo "Asia/Shanghai" > /etc/timezone
fi

# ── apt 源（用 HTTPS，避免 http 被中间设备劫持）──
if [ -f /etc/apt/sources.list.d/debian.sources ]; then
  sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null || true
elif [ -f /etc/apt/sources.list ]; then
  sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list 2>/dev/null || true
fi

# ── 基础工具（只在缺失时安装，避免每次都跑 apt）──
need=""
for pkg in ca-certificates curl iproute2 nftables tcpdump lrzsz openssh-server; do
  if ! dpkg -s "$pkg" >/dev/null 2>&1; then
    need="$need $pkg"
  fi
done
if [ -n "$need" ]; then
  log "installing:$need"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  # shellcheck disable=SC2086
  apt-get install -y -qq --no-install-recommends $need
  log "installed"
else
  log "base packages already present"
fi

# ── SSH（只允许密钥；密码登录关闭）──
if [ -d /etc/ssh ]; then
  sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config 2>/dev/null || true
  sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication no/' /etc/ssh/sshd_config 2>/dev/null || true
  systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null || true
fi

# ── 网络（地址由 labyard 的 net 步骤负责，这里只保证接口起来）──
if [ -n "$IPV4" ]; then
  log "ipv4 = $IPV4"
fi
if [ -n "$V6_PREFIX" ]; then
  log "ipv6 prefix = $V6_PREFIX"
fi
sysctl -qw net.ipv6.conf.all.disable_ipv6=0 2>/dev/null || true

# ── 写完成标记：labyard 用这个文件判断"这台机器配到哪一步了" ──
mkdir -p /var/lib/labyard
cat > /var/lib/labyard/provisioned.json <<EOF
{
  "hostname": "$HOSTNAME_V",
  "role": "$ROLE",
  "ipv4": "$IPV4",
  "ipv6_prefix": "$V6_PREFIX",
  "finished_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

log "done: $HOSTNAME_V ($ROLE)"
