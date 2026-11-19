#!/bin/bash
# labyard 宿主安装脚本（Proxmox VE 9.x / Debian 13）
#
# 只做四件事：放二进制、建目录、写 systemd 单元、生成自签证书。
# 不动 PVE 自身的配置，不装 PVE 组件。
#
#   ./install.sh                  安装或升级
#   ./install.sh --ttyd PATH      同上，但用本地的 ttyd 二进制（离线环境）
#   ./install.sh --uninstall      卸载（保留数据目录，除非 --purge）
#   ./install.sh --purge          连数据一起删
set -eu

BINDIR=/usr/local/bin
UNIT=/etc/systemd/system/labyard.service
CONFDIR=/etc/labyard
STATEDIR=/var/lib/labyard
BIN=./labyard
TTYD_SRC=""

say() { printf '\033[1;34m[labyard]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[警告]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[错误]\033[0m %s\n' "$*" >&2; exit 1; }

uninstall() {
  say "停止并移除服务"
  systemctl disable --now labyard.service 2>/dev/null || true
  rm -f "$UNIT" "$BINDIR/labyard" "$BINDIR/labyard.prev"
  systemctl daemon-reload 2>/dev/null || true

  # 镜像单元（labmirror@*）只依赖 /etc/labyard/mirror/*.conf，删数据前必须
  # 先停掉，否则它们会一直重启失败、把 journal 刷满，而且没有任何提示。
  if [ "${1:-}" = "--purge" ]; then
    n=0
    for u in $(systemctl list-units --all --plain --no-legend 'labmirror@*' 2>/dev/null | awk '{print $1}'); do
      systemctl disable --now "$u" >/dev/null 2>&1 || true
      n=$((n + 1))
    done
    [ "$n" -gt 0 ] && say "已停掉 $n 个镜像单元"
    warn "删除 $CONFDIR 与 $STATEDIR（不可恢复）"
    rm -rf "$CONFDIR" "$STATEDIR"
    # ttyd 是安装器放进去的（Web 终端的数据面），purge 一并清掉
    rm -f /usr/local/bin/ttyd
  else
    say "保留 $CONFDIR 与 $STATEDIR（要一起删加 --purge）"
    if systemctl list-units --plain --no-legend 'labmirror@*' 2>/dev/null | grep -q .; then
      warn "实验的 labmirror@ 单元仍在运行（它们只读 /etc/labyard/mirror）。"
      warn "要一起停： systemctl list-units 'labmirror@*' 然后逐个 disable --now"
    fi
  fi
  say "已卸载（容器与网桥属于实验资产，需在 Web 里 destroy，或手工 pct destroy）"
  exit 0
}

usage() {
  cat <<'EOF'
labyard 宿主安装脚本（PVE 9.x / Debian 13）

  ./install.sh                 安装或升级（缺 ttyd 时自动下载）
  ./install.sh --ttyd PATH     用本地的 ttyd 二进制（离线环境）
  ./install.sh --uninstall     卸载，保留 /etc/labyard 与 /var/lib/labyard
  ./install.sh --purge         卸载并删除数据（含镜像单元、ttyd、证书）
EOF
  exit 0
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage ;;
    --uninstall) uninstall ;;
    --purge) uninstall --purge ;;
    --ttyd)
      [ $# -ge 2 ] || die "--ttyd 后面要跟一个文件路径"
      TTYD_SRC="$2"
      shift 2
      ;;
    *) die "未知参数：$1（可用：--ttyd PATH / --uninstall / --purge）" ;;
  esac
done

# ── 前置检查 ──
[ "$(id -u)" -eq 0 ] || die "需要 root 运行"
[ -f "$BIN" ] || die "找不到 ./labyard，请在解压后的目录里运行"

if ! command -v pct >/dev/null 2>&1; then
  die "找不到 pct —— 这台机器看起来不是 Proxmox VE 宿主"
fi

PVE_VER=$(pveversion 2>/dev/null | sed -n 's|pve-manager/\([0-9]*\)\..*|\1|p' || echo 0)
if [ "${PVE_VER:-0}" -lt 9 ]; then
  die "需要 PVE 9.x，当前检测到 ${PVE_VER:-未知}"
fi
say "PVE 主版本: $PVE_VER"

for c in pvesh pveam ip nft; do
  command -v "$c" >/dev/null 2>&1 || warn "缺少命令 $c（对应功能会不可用）"
done

# ── 安装二进制 ──
say "安装二进制到 $BINDIR/labyard"
install -m 0755 "$BIN" "$BINDIR/labyard.new"
if [ -f "$BINDIR/labyard" ]; then
  # 保留上一版，出问题可以立刻回滚
  mv "$BINDIR/labyard" "$BINDIR/labyard.prev"
fi
mv "$BINDIR/labyard.new" "$BINDIR/labyard"

mkdir -p "$STATEDIR"/{locks,tmp,specs} "$CONFDIR"
chmod 700 "$STATEDIR"

# ── 配置 ──
if [ ! -f "$CONFDIR/config.toml" ]; then
  say "生成 $CONFDIR/config.toml"
  # 自动挑一个桥作为 wan：优先有默认路由的那个，否则 vmbr0
  WAN=$(ip -o link show type bridge 2>/dev/null | awk -F': ' '{print $2}' | head -1)
  WAN=${WAN:-vmbr0}
  cat > "$CONFDIR/config.toml" <<EOF
# labyard 配置。改完 systemctl restart labyard。
[pve]
node = "$(hostname -s)"
wan_bridge = "$WAN"      # 外部网络接在哪个桥上
vmid_base = 100          # 新建容器的 vmid 起始号

[server]
bind = "0.0.0.0"
port = 8443
tls_cert = "$CONFDIR/tls.crt"
tls_key = "$CONFDIR/tls.key"
secret_file = "$STATEDIR/secret.key"
session_hours = 8
EOF
  chmod 600 "$CONFDIR/config.toml"
else
  say "保留已有 $CONFDIR/config.toml"
fi

# ── 自签证书 ──
if [ ! -f "$CONFDIR/tls.crt" ] || [ ! -f "$CONFDIR/tls.key" ]; then
  say "生成自签证书（10 年）"
  IP=$(hostname -I | awk '{print $1}')
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$CONFDIR/tls.key" -out "$CONFDIR/tls.crt" \
    -subj "/CN=labyard" \
    -addext "subjectAltName=IP:$IP,DNS:$(hostname -s),DNS:localhost" 2>/dev/null \
    || die "openssl 生成证书失败"
  chmod 600 "$CONFDIR/tls.key"
fi

# ── systemd ──
say "写入 $UNIT"
cat > "$UNIT" <<'EOF'
[Unit]
Description=labyard —— 声明式实验网络编排
Documentation=https://github.com/kyle-derrick/labyard
After=network-online.target pve-cluster.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/labyard serve --config /etc/labyard/config.toml
Restart=on-failure
RestartSec=3
WorkingDirectory=/var/lib/labyard

# 保宿主优先于保服务：内存到顶先杀自己。
#
# 但账里不只有 labyard 自己：`pct` / `pvesh` 是 perl 进程，实测单个峰值
# RSS 176 MB（charged 约 60–80 MB），而且是它的子进程，**同属这个 cgroup**。
# 所以这里的上限要按"最大同时几个 pct"来算，不是按"常驻 12 MB × 2"：
#   3 × 70（apply 批内并发 3）+ 12（常驻）+ 32（scrypt）+ 余量 ≈ 384 MB
# 取 64M 那版在真机上的表现是：登录开始稳定回"密码错误"（scrypt 的 32 MB
# 工作区分配不到，verify 返 false），而 doctor 并发四个 pvesh 则直接
# oom-kill。SwapMax 给一点兜底，但不让热工作区整体换出。
MemoryMax=384M
MemorySwapMax=128M
CPUQuota=50%
IOWeight=50
TasksMax=256

# 需要 pct / ip / nft，必须是 root（这是设计事实，不是疏忽）
User=root

NoNewPrivileges=no
ProtectHome=read-only
PrivateTmp=yes

StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable labyard.service
systemctl restart labyard.service
sleep 1

if ! systemctl is-active --quiet labyard.service; then
  die "服务启动失败，查看： journalctl -u labyard -n 50"
fi

IP=$(hostname -I | awk '{print $1}')
ADDR="https://$IP:8443"

# 密码只在服务首次启动时打进 journal。安装器**不**把它捞出来打印——
# 安装输出会进 shell 历史、终端记录和自动化管道，journal 才是它唯一的落点。
# 这不是绕弯：bootstrap_password 的设计就是"启动时打一次"，这里复述等于第二次泄露点。
if journalctl -u labyard --since "2 minutes ago" 2>/dev/null | grep -q "初始管理员密码"; then
  PW_LOC="journalctl -u labyard | grep 初始管理员密码"
else
  PW_LOC=""
fi

# ── Web 终端的数据面：ttyd 静态二进制 ──
# Debian 13 仓库里没有 ttyd（从 forky 才开始打包），用官方 Release 的静态
# 单文件（1.3 MB，无 .so 依赖）。它只用于往容器里推，宿主自己不跑。
# 离线环境用 `--ttyd /path/to/ttyd` 跳过下载。
if [ -n "$TTYD_SRC" ]; then
  [ -f "$TTYD_SRC" ] || die "找不到 ttyd 文件：$TTYD_SRC"
  say "安装本地 ttyd：$TTYD_SRC"
  install -m 0755 "$TTYD_SRC" /usr/local/bin/ttyd
elif [ -x /usr/local/bin/ttyd ]; then
  say "已有 /usr/local/bin/ttyd，跳过下载"
else
  case "$(uname -m)" in
    x86_64) TTYD_ARCH=x86_64 ;;
    aarch64|arm64) TTYD_ARCH=aarch64 ;;
    *) TTYD_ARCH="" ;;
  esac
  if [ -n "$TTYD_ARCH" ]; then
    say "下载 ttyd（Web 终端）"
    if curl -fsSL --connect-timeout 10 -o /usr/local/bin/ttyd \
        "https://github.com/tsl0922/ttyd/releases/download/1.7.7/ttyd.$TTYD_ARCH"; then
      chmod +x /usr/local/bin/ttyd
      echo "  ttyd: $(/usr/local/bin/ttyd --version 2>/dev/null || echo 未知版本)"
    else
      rm -f /usr/local/bin/ttyd
      echo "  ⚠ ttyd 下载失败——Web 终端不可用（其余功能不受影响）"
      echo "    离线环境可重跑： ./install.sh --ttyd /path/to/ttyd"
    fi
  else
    warn "未知架构 $(uname -m)，跳过 ttyd——Web 终端不可用（其余功能不受影响）"
  fi
fi

echo
say "安装完成"
echo "  地址:   $ADDR"
if [ -n "$PW_LOC" ]; then
  echo "  密码:   首次安装，执行 $PW_LOC 查看"
  echo "          （只在 journal 里出现这一次；忘了就删 $STATEDIR/password.hash 再重启）"
else
  echo "  密码:   沿用上次的（忘了就删 $STATEDIR/password.hash 再重启）"
fi
echo
echo "  自检:   labyard doctor"
echo "  日志:   journalctl -u labyard -f"
echo "  升级:   替换二进制后 systemctl restart labyard（上一版留在 labyard.prev）"
echo
if [ "$(curl -sk -o /dev/null -w '%{http_code}' "$ADDR/api/health" 2>/dev/null)" = "200" ]; then
  say "健康检查通过"
else
  warn "健康检查未通过，请查看 journalctl -u labyard -n 50"
fi
