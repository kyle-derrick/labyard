#!/bin/bash
# 把本地源码同步到 lab3 容器并跑 cargo check / test。
# 用法： ./scripts/dev.sh check | test | fmt | clippy
set -eu
SRC="/Users/silas/WorkBuddy/2026-11-15-23-34-28/labyard"
CT="${CT:-lab3}"

tar czf /tmp/labyard-sync.tgz \
  --exclude=target --exclude=.git --exclude=node_modules \
  --exclude='*.tgz' -C "$SRC" . 2>/dev/null

cat /tmp/labyard-sync.tgz | orb -m "$CT" bash -c 'mkdir -p ~/labyard && tar xzf - -C ~/labyard' 2>/dev/null

orb -m "$CT" bash -c "export PATH=\$HOME/.cargo/bin:\$PATH; cd ~/labyard && $*"
