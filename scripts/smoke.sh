#!/bin/bash
# 容器内的端到端冒烟测试：不需要 PVE，只验证 HTTP 层与 planner。
set -u
cd "$(dirname "$0")/.."

BIN=./target/release/labyard
DIR=/tmp/labyard-smoke
PORT=18443

rm -rf "$DIR"; mkdir -p "$DIR"

cat > "$DIR/config.toml" <<EOF
[pve]
node = "pve"
wan_bridge = "vmbr0"
vmid_base = 100

[server]
bind = "127.0.0.1"
port = $PORT
allow_anonymous = true
session_hours = 8
secret_file = "$DIR/secret.key"

[paths]
db = "$DIR/labyard.db"
locks = "$DIR/locks"
tmp = "$DIR/tmp"
specs = "$DIR/specs"
EOF

"$BIN" serve --config "$DIR/config.toml" > "$DIR/server.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT

for _ in $(seq 1 60); do
  curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/health" >/dev/null 2>&1 && break
  sleep 0.2
done

pass=0; fail=0
check() {
  local name="$1" expect="$2" actual="$3"
  if echo "$actual" | grep -q "$expect"; then
    printf "  ✓ %s\n" "$name"; pass=$((pass+1))
  else
    printf "  ✗ %s\n    期望包含: %s\n    实际: %s\n" "$name" "$expect" "${actual:0:200}"; fail=$((fail+1))
  fi
}

echo "== 健康检查 =="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/health")
check "health 返回 ok" '"ok":true' "$r"

echo "== 静态资源 =="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/")
check "首页是 HTML" '<div id="root">' "$r"
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/some/spa/route")
check "未知路径回落到 index.html" '<div id="root">' "$r"

echo "== 认证（anonymous 模式）=="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/auth/me")
check "me 返回 anonymous" 'anonymous' "$r"

echo "== 预览：单 zone =="
YAML=$(python3 -c 'import json,sys;print(json.dumps(open("examples/main.yaml").read()))')
r=$(curl -s --noproxy '*' -X POST "http://127.0.0.1:$PORT/api/preview" \
  -H 'Content-Type: application/json' -d "{\"yaml\":$YAML}")
check "预览成功并返回步骤" '"steps"' "$r"
n=$(echo "$r" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)["steps"]))' 2>/dev/null || echo 0)
check "步骤数 > 8（实际 $n）" '' "$( [ "$n" -gt 8 ] && echo ok )"
check "包含桥步骤" 'bridge:main' "$r"
check "包含容器步骤" 'ct:main:gateway' "$r"
check "包含镜像步骤" 'mirror:main:server1->analysis' "$r"

echo "== 预览：嵌套 zone =="
YAML2=$(python3 -c 'import json;print(json.dumps(open("examples/nested.yaml").read()))')
r=$(curl -s --noproxy '*' -X POST "http://127.0.0.1:$PORT/api/preview" \
  -H 'Content-Type: application/json' -d "{\"yaml\":$YAML2}")
check "嵌套 zone 被接受" 'bridge:sub' "$r"
check "上游 main 先于 sub" '"zone":"main"' "$r"

echo "== 预览：错误输入 =="
r=$(curl -s --noproxy '*' -X POST "http://127.0.0.1:$PORT/api/preview" \
  -H 'Content-Type: application/json' -d '{"yaml":"zones: {a: {uplink: nope, v4: 10.1.0.0/24, hosts: {g: {role: gateway}}}}"}')
check "未知 uplink 报错" '不存在' "$r"
r=$(curl -s --noproxy '*' -X POST "http://127.0.0.1:$PORT/api/preview" \
  -H 'Content-Type: application/json' -d '{"yaml":"zones: [ }"}')
check "YAML 语法错误报错" 'error' "$r"

echo "== 状态 =="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/state")
check "state 返回 steps 数组" '"steps"' "$r"

echo "== doctor（无 PVE，应优雅失败而非崩溃）=="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/doctor")
check "doctor 返回 items" '"items"' "$r"
check "doctor 标出 PVE 不可用" '"ok":false' "$r"

echo "== OpenAPI =="
r=$(curl -s --noproxy '*' "http://127.0.0.1:$PORT/api/openapi.json")
check "openapi 文档存在" '"openapi"' "$r"

echo
echo "结果：通过 $pass，失败 $fail"
if [ "$fail" -gt 0 ]; then
  echo "--- server.log ---"
  tail -20 "$DIR/server.log"
  exit 1
fi
