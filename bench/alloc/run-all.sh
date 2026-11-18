#!/bin/bash
# 分配器对照压测。核心问题：反复分配释放之后，RSS 是收敛还是持续上涨。
#
# 用法（在 Debian/arm64 或 amd64 容器内）：
#   ./run-all.sh
#
# 输出：idle / 稳态峰值 HWM / 三次连续 churn 后的 RSS
#   第三次相对第二次的增量 ≈ 0  → 收敛（可以长期常驻）
#   每次都在涨                  → 不收敛（跑几周会把内存吃光）
set -u
export PATH=$HOME/.cargo/bin:$PATH
cd "$(dirname "$0")"

PORT=18080
rss() { awk '/VmRSS/{print $2}' /proc/$1/status 2>/dev/null; }
hwm() { awk '/VmHWM/{print $2}' /proc/$1/status 2>/dev/null; }

measure() {
  local label="$1" bin="$2"
  "$bin" >/dev/null 2>&1 &
  local pid=$!
  for _ in $(seq 1 80); do
    curl -s --noproxy '*' http://127.0.0.1:$PORT/health >/dev/null 2>&1 && break
    sleep 0.1
  done
  sleep 2
  local idle; idle=$(rss $pid)

  # 稳态负载：500 请求 / 50 并发
  seq 1 500 | xargs -P 50 -I{} curl -s --noproxy '*' -o /dev/null http://127.0.0.1:$PORT/api/item 2>/dev/null
  sleep 0.5
  local peak; peak=$(hwm $pid)
  local load; load=$(rss $pid)

  # 三次连续 churn，每次之后静置观察
  local c1 c2 c3
  curl -s --noproxy '*' -X POST -o /dev/null http://127.0.0.1:$PORT/api/churn 2>/dev/null
  sleep 3; c1=$(rss $pid)
  curl -s --noproxy '*' -X POST -o /dev/null http://127.0.0.1:$PORT/api/churn 2>/dev/null
  sleep 3; c2=$(rss $pid)
  curl -s --noproxy '*' -X POST -o /dev/null http://127.0.0.1:$PORT/api/churn 2>/dev/null
  sleep 3; c3=$(rss $pid)

  kill $pid 2>/dev/null; wait $pid 2>/dev/null
  printf "%-14s idle=%-8s peakHWM=%-8s afterLoad=%-8s churn1=%-8s churn2=%-8s churn3=%-8s\n" \
    "$label" "${idle}KB" "${peak}KB" "${load}KB" "${c1}KB" "${c2}KB" "${c3}KB"
}

echo "=== $(uname -m) / $(nproc) cores / glibc $(ldd --version | head -1 | awk '{print $NF}') / $(rustc --version) ==="
echo

echo "--- build: glibc ---"
cargo build --release --no-default-features -q && cp target/release/allocbench /tmp/bin-glibc
echo "--- build: jemalloc ---"
cargo build --release --features jemalloc -q && cp target/release/allocbench /tmp/bin-jemalloc
echo "--- build: mimalloc ---"
cargo build --release --features mimalloc -q && cp target/release/allocbench /tmp/bin-mimalloc

echo
echo "size(KB): glibc=$(( $(stat -c%s /tmp/bin-glibc) / 1024 )) jemalloc=$(( $(stat -c%s /tmp/bin-jemalloc) / 1024 )) mimalloc=$(( $(stat -c%s /tmp/bin-mimalloc) / 1024 ))"
echo
measure "glibc-malloc" /tmp/bin-glibc
measure "jemalloc"     /tmp/bin-jemalloc
measure "mimalloc"     /tmp/bin-mimalloc
