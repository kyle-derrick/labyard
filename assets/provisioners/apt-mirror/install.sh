#!/bin/sh
# apt-mirror provisioner —— 把一台容器变成内网 apt 缓存 + 流量分析点。
#
# 为什么是缓存而不是完整镜像：完整 Debian 镜像要 60+ GB，缓存只需要几 GB，
# 而内网实验的重复下载量很小，缓存命中率足够。
set -eu

log() { printf '[apt-mirror] %s\n' "$*"; }
export DEBIAN_FRONTEND=noninteractive

# ── apt-cacher-ng：成熟方案，不自己写代理 ──
if ! dpkg -s apt-cacher-ng >/dev/null 2>&1; then
  log "installing apt-cacher-ng"
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends apt-cacher-ng
fi

# 绑 0.0.0.0：这台机器只有内网口（analysis 挂在 zone 的网桥上），
# 所以 0.0.0.0 的可达范围就是整个 zone。要更紧就改成它自己的内网地址。
if [ -f /etc/apt-cacher-ng/acng.conf ]; then
  if ! grep -q '^BindAddress: 0.0.0.0' /etc/apt-cacher-ng/acng.conf; then
    sed -i 's/^#\?BindAddress:.*/BindAddress: 0.0.0.0/' /etc/apt-cacher-ng/acng.conf
  fi
  if ! grep -q '^Port: 3142' /etc/apt-cacher-ng/acng.conf; then
    echo 'Port: 3142' >> /etc/apt-cacher-ng/acng.conf
  fi
fi
systemctl enable --now apt-cacher-ng 2>/dev/null || service apt-cacher-ng restart

# ── 抓包工具：镜像流量到达后用于分析 ──
for pkg in tcpdump tshark; do
  if ! dpkg -s "$pkg" >/dev/null 2>&1; then
    log "installing $pkg"
    apt-get install -y -qq --no-install-recommends "$pkg" || log "skip $pkg（源里没有）"
  fi
done

mkdir -p /var/lib/labyard/captures
chmod 700 /var/lib/labyard/captures

# ── 一个最小的抓包服务：手动启停，不常驻 ──
cat > /usr/local/bin/labyard-capture <<'EOF'
#!/bin/sh
# 用法: labyard-capture start <iface> | stop | status
set -eu
DIR=/var/lib/labyard/captures
PIDF=/run/labyard-capture.pid
case "${1:-status}" in
  start)
    iface="${2:-eth0}"
    mkdir -p "$DIR"
    out="$DIR/$(date +%Y%m%d-%H%M%S)-$iface.pcap"
    nohup tcpdump -i "$iface" -w "$out" -s 0 -U >/dev/null 2>&1 &
    echo $! > "$PIDF"
    echo "capturing on $iface -> $out"
    ;;
  stop)
    [ -f "$PIDF" ] && kill "$(cat "$PIDF")" 2>/dev/null || true
    rm -f "$PIDF"
    echo "stopped"
    ;;
  status)
    if [ -f "$PIDF" ] && kill -0 "$(cat "$PIDF")" 2>/dev/null; then
      echo "running pid=$(cat "$PIDF")"
    else
      echo "not running"
    fi
    ;;
esac
EOF
chmod +x /usr/local/bin/labyard-capture

log "apt cache: http://$(hostname -I | awk '{print $1}'):3142"
log "captures: /var/lib/labyard/captures"
