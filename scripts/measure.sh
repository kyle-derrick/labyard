#!/bin/bash
# 实测 labyard 自身的 RSS（不含 TLS，HTTP 模式）。
# 用途：验证 §7.5 的"平台值 18–22 MB"估算是否靠谱。
set -u
cd "$(dirname "$0")/.."

BIN=./target/release/labyard
DIR=/tmp/labyard-measure
PORT=18444

rm -rf "$DIR"; mkdir -p "$DIR"

cat > "$DIR/config.toml" <<EOF
[pve]
node = "pve"
wan_bridge = "vmbr0"

[server]
bind = "127.0.0.1"
port = $PORT
allow_anonymous = true
secret_file = "$DIR/secret.key"

[paths]
db = "$DIR/db.sqlite"
locks = "$DIR/locks"
tmp = "$DIR/tmp"
specs = "$DIR/specs"
EOF

"$BIN" serve --config "$DIR/config.toml" > "$DIR/log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.2
done
sleep 2

rss() { awk '/VmRSS/{print $2}' /proc/$PID/status; }
hwm() { awk '/VmHWM/{print $2}' /proc/$PID/status; }
thr() { awk '/Threads/{print $2}' /proc/$PID/status; }
fds() { ls /proc/$PID/fd 2>/dev/null | wc -l; }

IDLE=$(rss)
IDLE_THR=$(thr)
IDLE_FD=$(fds)

# 200 个并发 20 的请求，混合 preview（重）和 state（轻）
YAML=$(python3 -c 'import json;print(json.dumps(open("examples/nested.yaml").read()))')
seq 1 200 | xargs -P 20 -I{} sh -c "
  if [ \$(( {} % 4 )) -eq 0 ]; then
    curl -s --noproxy '*' -o /dev/null -X POST http://127.0.0.1:$PORT/api/preview \
      -H 'Content-Type: application/json' -d '$YAML'
  else
    curl -s --noproxy '*' -o /dev/null http://127.0.0.1:$PORT/api/state
  fi
" 2>/dev/null

sleep 1
LOAD=$(rss)
PEAK=$(hwm)
LOAD_THR=$(thr)
LOAD_FD=$(fds)

echo "=== $(uname -m) / $(nproc) cores / $(rustc --version 2>/dev/null || echo rustc-n/a) ==="
echo
printf "%-22s %s\n" "二进制大小" "$(du -h "$BIN" | cut -f1)"
printf "%-22s %s KB\n" "空闲 RSS" "$IDLE"
printf "%-22s %s KB\n" "压测峰值 VmHWM" "$PEAK"
printf "%-22s %s KB\n" "压测后 RSS" "$LOAD"
printf "%-22s %s 线程\n" "空闲线程数" "$IDLE_THR"
printf "%-22s %s 线程\n" "压测线程数" "$LOAD_THR"
printf "%-22s %s\n" "空闲 fd" "$IDLE_FD"
printf "%-22s %s\n" "压测 fd" "$LOAD_FD"
echo
echo "说明：这是 HTTP 模式（无 TLS）。带 TLS 会多 4-6 MB 的会话与缓冲区。"
echo "平台真实值还要加：SSE 连接状态、pct exec 子进程管理、日志缓冲。"
