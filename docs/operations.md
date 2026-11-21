# labyard 运维手册

面向"已经装好、正在用"的场景：升级、备份、排障、卸载。设计取舍看
[`design.md`](design.md)，这里只写怎么动手。

---

## 1. 装

前置：PVE 9.x（Debian 13）、单节点、root。`pct` / `pvesh` / `pveam` / `ip` / `nft`
由 PVE 自带，安装器只检查不代装。

```bash
# 预编译包（打 v* tag 时由 GitHub Actions 构建，见 .github/workflows/release.yml）
BASE=https://github.com/kyle-derrick/labyard/releases/latest/download
curl -fLO "$BASE/labyard-linux-amd64.tar.gz"
curl -fLO "$BASE/SHA256SUMS" && sha256sum -c SHA256SUMS --ignore-missing
tar xzf labyard-linux-amd64.tar.gz && cd labyard-linux-amd64

./installer/install.sh                      # 缺 ttyd 会自动下载
./installer/install.sh --ttyd ./ttyd        # 离线环境：指定本地 ttyd
```

要回滚到某个旧版本，把 `latest/download` 换成 `download/v0.1.0`——资产名固定，
版本只体现在 Release tag 与包内 `VERSION`。

从源码构建（装了 Rust 与 Node 的机器）：`make` → `target/release/labyard`，
把二进制放到 `installer/` 上一层再跑 `./installer/install.sh`。

装完之后的三件事：

```bash
journalctl -u labyard | grep 初始管理员密码   # 密码只在这个 journal 里出现过一次
labyard doctor                              # 9 项自检，缺什么直接说
systemctl status labyard                     # active (running)
```

初始密码的来历：首次启动生成 20 位随机密码，**只打印一次**到 journal，落
`/var/lib/labyard/password.hash`（scrypt）。安装器不把它捞出来复述——安装输出
会进 shell 历史、终端记录和自动化管道，那等于第二个泄露点。

### 文件都在哪

| 路径 | 内容 | 要不要备份 |
|---|---|---|
| `/etc/labyard/config.toml` | 主配置 | **要** |
| `/etc/labyard/tls.crt` / `tls.key` | 自签证书 | 可选（删了会重新生成） |
| `/var/lib/labyard/labyard.db` | 步骤状态、vmid 分配、retire 标记、run 历史 | **要** |
| `/var/lib/labyard/specs/*.yaml` | 规格文件（唯一真相源） | **要** |
| `/var/lib/labyard/secret.key` | cookie 签名密钥 | 可选（换掉只需重新登录） |
| `/var/lib/labyard/password.hash` | 管理员密码哈希 | 可选（删掉重启会重新生成并打印） |
| `/usr/local/bin/labyard` | 二进制 | 否 |

---

## 2. 升级与回滚

没有"升级命令"，因为不需要——二进制 + 数据目录解耦，DB 迁移在启动时自动跑
（`schema_version` 表，编号 SQL，只前进不回退）。

```bash
# 升级：替换二进制后重启。安装器会把上一版留成 labyard.prev
./installer/install.sh

# 手工：
install -m755 ./labyard /usr/local/bin/labyard && systemctl restart labyard
```

回滚：

```bash
systemctl stop labyard
mv /usr/local/bin/labyard.prev /usr/local/bin/labyard
systemctl start labyard
```

**如果新版本的迁移改过表结构，回滚二进制会读不懂新表。** 升级前先按下一节做一次
`VACUUM INTO` 备份，回滚时连 DB 一起还原。

升级**不需要**重新 apply：步骤哈希没变，apply 会全部跳过。例外是
`RULESET_VERSION` 变了（生成器逻辑有实质改动），此时相关步骤会重跑一遍——这是
设计意图，不是回归，容器里的地址与规则会被重写成新版。

---

## 3. 备份与恢复

停机备份（最稳）：

```bash
systemctl stop labyard
tar czf /root/labyard-backup-$(date +%F).tgz \
    /etc/labyard/config.toml /var/lib/labyard
systemctl start labyard
```

不停机备份 DB（一致性快照）：

```bash
# 宿主上有 sqlite3 时最省事
sqlite3 /var/lib/labyard/labyard.db "VACUUM INTO '/root/labyard-$(date +%F).db'"
```

没有 `sqlite3` 也不要紧：停机再 `cp` 就是绝对一致的（服务停下时没有写者）。
`VACUUM INTO` 的好处只是"不停机"。平台自己**不做**自动备份——备份策略归运维，
一个藏在后台、你不知道它在哪的备份比没有备份更危险。

规格目录可以单独进 git：

```bash
cd /var/lib/labyard && git init && git add specs && git commit -m "specs snapshot"
```

恢复：

```bash
systemctl stop labyard
tar xzf labyard-backup-2026-11-18.tgz -C /           # 覆盖回原位
chmod 700 /var/lib/labyard
systemctl start labyard
labyard apply -f /var/lib/labyard/specs/lab.yaml --dry-run   # 先看计划
labyard apply -f /var/lib/labyard/specs/lab.yaml --yes       # 对账一次
```

**DB 丢了也丢不掉机器**：vmid 会重新分配，但那只是"平台不记得 vmid"，容器还在
PVE 里。此时先 `pct list` 看清现状，再决定是重新 apply（可能建重复容器）还是手工
`pct set` 回填 vmid。

---

## 4. Web 挂了怎么办

Web 是唯一操作界面，但**不是唯一执行路径**：

| 想做的事 | 没有 Web 时 |
|---|---|
| 看自检 | `labyard doctor` / `labyard doctor --json` |
| 执行规格 | `labyard apply -f <spec> [--dry-run] [--yes] [--snapshot]` |
| 删一个 zone | `labyard destroy -f <spec> --zone <name> --yes` |
| 看步骤状态 | `sqlite3 /var/lib/labyard/labyard.db "select * from step_state"` |
| 重启服务 | `systemctl restart labyard` |
| 彻底不管了 | 直接点 PVE 页面：容器和桥都在，只是没人管状态 |

恢复 Web 之后**先跑一次 apply 对账**，让平台状态与容器实际状态重新一致。

---

## 5. 排障

按"症状 → 先查什么 → 常见原因"排。

| 症状 | 先查 | 常见原因 / 处理 |
|---|---|---|
| 服务起不来 | `journalctl -u labyard -n 50` | 端口被占（`--port` 换一个）；证书路径错（删 `tls.crt`/`tls.key` 重启会重生成）；`config.toml` 语法错 |
| 打开页面 502 / 一直转 | `curl -sk https://127.0.0.1:8443/api/health` | 服务没起；反代加了路径前缀（labyard 必须挂在根路径） |
| 忘了密码 | — | 删 `/var/lib/labyard/password.hash` 后重启，会重新生成并打印到 journal（要删密码文件前先 `systemctl stop`，否则运行中的进程还在用旧的） |
| apply 全跳过，但环境明显不对 | `sqlite3 ... "select * from step_state"` | 手工改过容器/规则。清掉相关步骤记录让它重跑：`delete from step_state where step_key like 'net:%'`。**不要**直接删整个表——那会让所有步骤重跑 |
| 改了 `net.rs`/`gateway.rs` 却不生效 | 看 `RULESET_VERSION` 是否 +1 | 步骤哈希只看输入、不看生成器代码。生成逻辑有实质变化必须手动 +1 |
| 容器重启后 zone 失联 | `pct exec <vmid> -- systemctl status labyard-net` | 开机单元没装（老版本装的容器）：重跑 `net` 步骤即可重新落盘 |
| 容器重启后网关全放行 | `pct exec <vmid> -- nft list ruleset \| head` | 同上，`labyard-nft` 单元缺失或失败。`journalctl -u labyard-nft` 看脚本报错 |
| 网关策略"下发成功"但没生效 | `pct exec <vmid> -- nft list chain inet labyard forward` | 顺序对不对（egress 关闭时 drop 必须在 `established` 之前）；规则表是不是空的 |
| 端口映射连不上 | 从宿主 `curl` 映射端口 | ① 无默认路由的主机没有回程路由（映射到无默认路由的主机必须有 `ct status dnat masquerade`）；② `dnat ip to` 写漏 `ip`（inet 表）；③ 宿主入向被 drop |
| 映射通了但只第一个包通 | 同上 | 缺 `ct status dnat accept`：被 DNAT 的包看起来像"内网发内网"，会被默认拒绝入向吃掉 |
| Web 终端 404 | `pct exec <vmid> -- systemctl is-active labyard-ttyd` | ttyd 没装（宿主没有 `/usr/local/bin/ttyd`，用 `--ttyd` 补）；容器没有内网地址（ttyd 绑的是内网 IP，不是 0.0.0.0） |
| 终端页面能开、按键无反应 | 浏览器 devtools 看 WS | 反代拒了 `Upgrade`；或 ttyd 的 `Host` 头校验没过（反代必须转发**容器视角**的 Host） |
| 镜像不生效 | `tc filter show dev <bridge> egress`；`systemctl status 'labmirror@*'` | 监控机重建后 veth 名变了（apply 会重算）；conf 里的桥名写成了 zone 名而不是桥名 |
| retire 后镜像还在跑 | `systemctl list-units 'labmirror@*'` | 单元名是 `{zone}-{源vmid}-{目标vmid}`，废弃监控机时按后缀匹配。老版本只做前缀，升级后重跑一次 retire |
| 无外网主机装包失败 | `pct exec <cache> -- systemctl status apt-cacher-ng` | 缓存没起来，或主机的 `apt_mirror` 指向了还没就绪的缓存（plan 里已经有隐式依赖，报错说明缓存真的起不来） |
| 密码明明没记错，页面一直说"密码错误" | `systemctl show labyard -p MemoryMax -p MemoryPeak`；`grep -i "oom" <(journalctl -u labyard --no-pager)` | 老版本 `MemoryMax=64M` 的典型症状：`pct`/`pvesh` 子进程 charged 60–80 MB 顶穿上限，cgroup 极端回收后 scrypt 的 32 MB 工作区分配不到，`verify_password` 返回 false（接口如实报"密码错误"，不区分原因）。现在 384 MB + `MemorySwapMax=128M` 就是为这个改的；遇到时先 `systemctl restart labyard`，再用 `systemctl show labyard -p MemoryMax` 确认上限没被人改小 |
| 服务被 OOM kill | `systemctl show labyard -p MemoryPeak -p MemorySwapPeak`；`grep VmHWM /proc/$(systemctl show labyard -p MainPID --value)/status`；`journalctl -u labyard \| grep -i "oom-kill"` | `MemoryMax=384M`：**账里不只有 labyard 自己**——`pct`/`pvesh` 是它的子进程、同属一个 cgroup，单个峰值 RSS 176 MB（charged 60–80 MB）。所以要按"最大同时几个 pct"算，不是按"常驻 × 2"算。**注意 cgroup 口径会把页缓存算进去**，`MemoryCurrent` 贴着上限是正常且可回收的，判断是否真危险看 `VmHWM` |
| PVE 页面变卡 | `journalctl -u labyard \| grep pmxcfs` | `/etc/pve` 是单线程 FUSE，平台状态一律一次 `GET /cluster/resources` 批量取，不逐台查 |
| 容器起不来 | `pct status <vmid>` / `pct config <vmid>` | 桥不存在（先 `bridge` 步骤）；存储满；`memory` 太小 |

排障时优先看**平台状态与容器实际状态哪里不一致**——labyard 的价值就是把"以为"
变成"事实"，排障也一样：`pct list`、`nft list ruleset`、`systemctl list-units`
比看日志更快给出事实。

---

## 6. 卸载

```bash
./installer/install.sh --uninstall    # 停服务、删单元与二进制，保留数据
./installer/install.sh --purge        # 连 /etc/labyard、/var/lib/labyard、镜像单元、ttyd 一起删
```

两者都**不动容器与网桥**——那是实验资产，删它们要显式做：

```bash
labyard destroy -f <spec> --zone <name> --yes   # 或在 Web 里 destroy
pct list                                        # 确认没有残留
ip link show type bridge | grep lab             # 确认桥已清
```

---

## 7. 已知限制

| 限制 | 说明 |
|---|---|
| 单节点 | 集群只警告不阻止。`pve.node` 是配置里写死的一个节点，平台不感知调度——把 zone 建到别的节点上不是它的能力 |
| 不做升级迁移 | 从未发布过，没有历史版本要迁；`schema_version` 只保证"同一代内"的演进 |
| IPv6 出公网未验证 | ULA 与 NAT66 在真机验证过；宿主没有 IPv6 上游，出公网这段没有真机证据 |
| 厚 LVM 快照 | PVE 侧仍是 tech preview。`doctor` 只列出存储类型，不判断厚薄；快照操作原样透传 PVE 的报错，失败信息会显示在界面上 |
| `pct push/pull` 文件面板 | 没做。终端里的 ZMODEM 覆盖"传个文件"的主场景 |
| Web 是唯一界面 | 服务挂掉时用 CLI（见第 4 节）；再不行直接在 PVE 里操作 |
