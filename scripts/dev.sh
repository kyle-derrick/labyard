#!/bin/bash
# 把本地源码同步到 lab3 容器并跑 cargo check / test。
# 用法： ./scripts/dev.sh check | test | fmt | clippy
set -eu
SRC="/Users/silas/WorkBuddy/2026-11-15-23-34-28/labyard"
CT="${CT:-lab3}"

# `._*` 是 macOS 的资源叉，tar 会把它们一起带走；留在 web/dist 里会被
# rust-embed 打进二进制（每个 163 字节的垃圾，还会污染 embed 的文件表）。
tar czf /tmp/labyard-sync.tgz \
  --exclude=target --exclude=.git --exclude=node_modules \
  --exclude='*.tgz' --exclude='._*' --exclude='.DS_Store' \
  -C "$SRC" . 2>/dev/null

# 解包前先清空（target 除外）。tar 只覆盖不删除，前端换 hash 之后旧产物
# 会一直堆在 dist 里，然后被 rust-embed 全量嵌入——二进制无端胖 1 MB，
# 而且从体积上看不出任何原因。
orb -m "$CT" bash -c '
set -e
mkdir -p ~/labyard
find ~/labyard -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +
tar xzf - -C ~/labyard
' < /tmp/labyard-sync.tgz 2>/dev/null

orb -m "$CT" bash -c "export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/labyard && $*"
