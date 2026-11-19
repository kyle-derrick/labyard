#!/bin/sh
# labyard base bootstrap —— 在容器内执行，负责"这台机器能用了"这件事。
# 幂等：重复执行不改变结果。
#
# 由 labyard 通过 `pct exec` 推入并执行，参数通过环境变量传入。
set -eu

HOSTNAME_V="${LABYARD_HOSTNAME:-lab}"
ZONE="${LABYARD_ZONE:-}"
ROLE="${LABYARD_ROLE:-server}"
IPV4="${LABYARD_IPV4:-}"
V6_PREFIX="${LABYARD_V6_PREFIX:-}"
TIMEZONE="${LABYARD_TIMEZONE:-Asia/Shanghai}"
LANG_WANT="${LABYARD_LANG:-}"
APT_MIRROR="${LABYARD_APT_MIRROR:-}"
PACKAGES="${LABYARD_PACKAGES:-}"

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

# ── 时区（LABYARD_TIMEZONE，默认 Asia/Shanghai）──
if [ -f "/usr/share/zoneinfo/$TIMEZONE" ]; then
  ln -sf "/usr/share/zoneinfo/$TIMEZONE" /etc/localtime
  echo "$TIMEZONE" > /etc/timezone
  log "timezone -> $TIMEZONE"
else
  log "timezone $TIMEZONE 不存在，保持现状"
fi

# ── apt 源 ──
# 显式给 APT_MIRROR 就整体替换（无外网环境可指向内网 apt-cacher-ng）；
# 否则只把官方 http 升级成 https（避免被中间设备劫持）。
if [ -n "$APT_MIRROR" ]; then
  log "apt mirror -> $APT_MIRROR"
  # 整体重写而不是 sed 换 host：host 替换不是幂等操作，脚本跑第二遍会把
  # 镜像路径再拼一层（真机实测出过 debian/debian）。codename 从 os-release
  # 取；security 源按镜像约定把尾部 /debian 换成 /debian-security
  # （tuna 与 apt-cacher-ng 的路径映射都吃这个规则）。
  . /etc/os-release 2>/dev/null
  codename="${VERSION_CODENAME:-bookworm}"
  # security 源：镜像若以 /debian 结尾，换成 /debian-security（tuna 与
  # apt-cacher-ng 都是这个路径布局）；否则原样用（用户给了完整地址）
  case "$APT_MIRROR" in
    */debian) sec="${APT_MIRROR%/debian}/debian-security" ;;
    *) sec="$APT_MIRROR" ;;
  esac
  [ -f /etc/apt/sources.list ] && mv /etc/apt/sources.list /etc/apt/sources.list.bak
  [ -f /etc/apt/sources.list.d/debian.sources ] && mv /etc/apt/sources.list.d/debian.sources /etc/apt/sources.list.d/debian.sources.bak
  cat > /etc/apt/sources.list <<EOF
deb $APT_MIRROR $codename main contrib
deb $APT_MIRROR $codename-updates main contrib
deb $sec $codename-security main contrib
EOF
else
  if [ -f /etc/apt/sources.list.d/debian.sources ]; then
    sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null || true
  elif [ -f /etc/apt/sources.list ]; then
    sed -i 's|http://deb.debian.org|https://deb.debian.org|g' /etc/apt/sources.list 2>/dev/null || true
  fi
fi

# ── 基础工具（尽力而为）──
# 无外网的内网主机（按设计没有默认路由）apt 不可达是常态，基础工具补装
# 失败只记录，不算失败——它们是"锦上添花"，不是用户点名要的。
export DEBIAN_FRONTEND=noninteractive
need=""
for pkg in ca-certificates curl iproute2 nftables tcpdump lrzsz openssh-server; do
  if ! dpkg -s "$pkg" >/dev/null 2>&1; then
    need="$need $pkg"
  fi
done
if [ -n "$need" ]; then
  log "installing base:$need"
  apt-get update -qq || log "apt update 失败（无外网且未配 apt_mirror？）跳过基础工具补装"
  # shellcheck disable=SC2086
  apt-get install -y -qq --no-install-recommends $need || log "基础工具补装失败，继续（尽力而为）"
else
  log "base packages already present"
fi

# ── 用户点名的预装包（严格：装不上就失败）──
# 与基础工具的区别：这些是 spec 里显式声明的，静默跳过等于配置撒谎。
if [ -n "$PACKAGES" ]; then
  log "installing packages:$PACKAGES"
  apt-get update -qq
  # shellcheck disable=SC2086
  apt-get install -y -qq --no-install-recommends $PACKAGES
  log "packages installed"
fi

# ── 语言/locale（LABYARD_LANG，如 zh_CN.UTF-8）──
# 中文支持：生成 locale + 设为默认。locales 包需要 apt，无外网且没配
# 镜像时失败只记录——语言是可选配置，不该炸掉整台机器的基础配置。
if [ -n "$LANG_WANT" ]; then
  if ! dpkg -s locales >/dev/null 2>&1; then
    apt-get update -qq && apt-get install -y -qq --no-install-recommends locales
  fi
  if dpkg -s locales >/dev/null 2>&1; then
    sed -i "s|^# *\($LANG_WANT\)|\1|" /etc/locale.gen
    grep -q "^$LANG_WANT" /etc/locale.gen || echo "$LANG_WANT UTF-8" >> /etc/locale.gen
    locale-gen
    printf 'LANG=%s\n' "$LANG_WANT" > /etc/default/locale
    export LANG="$LANG_WANT"
    log "locale -> $LANG_WANT"
  else
    log "locales 包不可用（无外网？），locale 未设置"
  fi
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
  "zone": "$ZONE",
  "role": "$ROLE",
  "ipv4": "$IPV4",
  "ipv6_prefix": "$V6_PREFIX",
  "finished_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

log "done: $HOSTNAME_V zone=$ZONE role=$ROLE"
