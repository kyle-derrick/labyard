# labyard

**声明式网络编排，给 Proxmox VE 单节点用：写一份 YAML，得到一个可复现、可推倒重来的实验网络。**

Declarative network lab orchestration for Proxmox VE.

[![release](https://img.shields.io/github/v/release/kyle-derrick/labyard?sort=semver)](https://github.com/kyle-derrick/labyard/releases)
[![license](https://img.shields.io/github/license/kyle-derrick/labyard)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.80%2B-orange)](Cargo.toml)

已在真实 PVE 9.2.2 上跑通完整链路：`doctor` 自检 → `apply` 建网 → 双栈出网 → `destroy` 拆干净。

---

## 它解决什么

手工搭一套实验环境，问题不在于「第一次怎么建」，而在于：

- 建到一半失败，不知道从哪继续
- 改了参数，不知道哪些机器要重建
- 网络拓扑嵌套时，地址、路由、NAT 容易配错且难查
- 拆掉之后，宿主上留一堆桥、容器、iptables 规则

labyard 的做法是**声明 + 收敛**：YAML 描述目标状态，每次执行都做
「探测现状 → 对比 → 只做差的」。重复执行同一份文件不会重复建机器，改一个字段只重建
受影响的那一步。

## 快速开始

在 PVE 宿主上（Debian 13 / PVE 9.x）：

```bash
curl -fLO https://github.com/kyle-derrick/labyard/releases/latest/download/labyard-linux-amd64.tar.gz
curl -fLO https://github.com/kyle-derrick/labyard/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing

tar xzf labyard-linux-amd64.tar.gz && cd labyard-linux-amd64
./installer/install.sh
```

安装器只做这几件事：放二进制到 `/usr/local/bin`、建 `/etc/labyard` 与 `/var/lib/labyard`、
写 systemd 单元、生自签证书、下载 ttyd。**不动 PVE 自身配置。**

```bash
# 浏览器打开 https://<宿主IP>:8443
# 初始密码只在首次启动时打进 journal 一次：
journalctl -u labyard | grep 初始管理员密码
# 忘了就删 /var/lib/labyard/password.hash 再 systemctl restart labyard
```

也可以完全不用 Web：

```bash
labyard doctor                                   # 环境自检，缺什么直接说
labyard apply -f examples/main.yaml --dry-run    # 只打印计划
labyard apply -f examples/main.yaml              # 执行
labyard apply -f examples/main.yaml              # 再跑一次：全部跳过
```

升级就是重跑上面四条命令（安装器会把上一版留成 `/usr/local/bin/labyard.prev`）。
校验与回滚到指定版本见 [`docs/operations.md`](docs/operations.md) §1–2。

## 核心模型

一个 `zone` = 一个网桥 + 一组主机 + 一个上联。上联可以是 `wan`，也可以是**另一个 zone**
——于是多层嵌套是递归的自然结果，不是特例：

```yaml
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      server1: { role: server }
  sub:
    uplink: main        # sub 的网关挂在 main 的内网里
    v4: 10.20.1.0/24
    hosts:
      gateway: { role: gateway }
      client1: { role: client }
```

其余约定：

- 三层默认值：`defaults` → `zone` → `host`，逐字段覆盖
- 主机基础配置同样三层合并：`timezone`（时区）、`lang`（locale，如 `zh_CN.UTF-8`）、`apt_mirror`（apt 源，无外网机器可指向内网 apt-cacher-ng）、`packages`（预装包；`packages: []` 显式不要——无外网主机用它退出继承）
- `storage` 是 rootfs 落点；`template_storage` 是模板（vztmpl）落点，**默认跟随 `storage`**。rootfs 用 lvmthin/zfspool 时模板必须分开放（如 `template_storage: local`），一体存储（dir/NFS）零配置
- IPv6 默认开启。给内网主机**只发** ULA（`fd` 开头，按 zone 名确定性派生）+ NAT66；网关自己从上联口按 SLAAC 收一个上游 GUA 仅作 NAT66 源地址，**不申请前缀委派（PD）、也不把上游地址下发给内网主机**
- 内网主机（server/client）**不给默认路由**，只能从内网发起
- `nat: true`（默认）让网关做地址转换；`nat: false` 只转发不转换，用于上游已经路由了整个网段的情形
- 同角色主机不写 `ip` 时自动取不同主机位（两台 `server` 是 `.20`/`.21`，不会撞）
- 幂等的单位是「步骤 key + 输入哈希」，不是版本号

### 网关、镜像与生命周期

```yaml
zones:
  lab:
    uplink: wan
    v4: 10.77.0.0/24
    # 整网流量监控：本网络每台主机的接口流量复制到一台监控机的抓包口
    monitor: { name: probe, mgmt: wan }   # 管理口挂外网桥；不写 mgmt = 本网段
    hosts:
      gw:     { role: gateway, ip: 1 }
      t1:     { role: server, ip: 20,
                apt_mirror: http://10.77.0.10:3142/debian }
    gateway:
      egress: true            # 内网是否允许出网（关 = 立刻切断，已建立连接一起断）
      pub:                    # 端口映射：宿主端口 → 内网某台机
        - { proto: tcp, port: 17681, to: "10.77.0.20:7681" }
      vpn:                    # 远程接入；私钥只存在容器里，下载时现读现拼
        wireguard: false
        openvpn: false

mirrors:                      # 精确连线：单台主机的流量交给某台监控机（可跨网络）
  - { from: "sub:client1", to: "lab:probe" }
```

- `role: gateway` 每个 zone 唯一，拿网段 `.1`；`egress` / `pub` / `vpn` 只作用于它
- 网关策略每次**整表重建**（`nft -f` 单事务），不做增量——不存在"删了映射规则还在"
- **retire ≠ destroy**：retire 停机 + 摘镜像单元 + 拓扑折叠，保留 vmid / IP / 磁盘 / 快照，
  `revive` 原样回来；destroy 才是真删
- 规格可以有多份：`paths.specs/*.yaml` 下的文件就是唯一真相源，Web 里可新建/复制/删除，
  单个 zone 可以单独打开编辑（原位改，保留注释与顺序）
- 写入走「先 parse + resolve 校验，再临时文件 + rename 原子替换」——非法 YAML 一律 400
  且不碰旧文件；刷新页面从服务端读回，改动没保存时标题旁有提示

## 子命令

| 命令 | 用途 |
|---|---|
| `serve` | Web 界面（日常操作都在这里） |
| `doctor` | 自检：PVE 版本、`pct`/`pvesh`/`pveam` 是否可用、存储能力、桥是否存在 |
| `apply` | 无界面执行一份规格，给测试与 CI 用 |
| `destroy` | 删掉一个 zone 的全部资源（不可逆，要求显式传 zone 名） |

没有 `zone create` / `host add` 这类操作型子命令：两套界面维护同一套语义，迟早会不一致。

## 环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| PVE | **9.x** | 低于 9.0 拒绝启动，不做 8.x 兼容层 |
| 节点 | 单节点 | 集群只警告不阻止 |
| 架构 | 发布包为 `linux/amd64` | arm64 可从源码构建，把 `Makefile` 里的 `TARGET` 改成 `aarch64-unknown-linux-gnu` 再 `make cross` |
| 宿主依赖 | `pct` `pvesh` `pveam` `ip` `nft` | `install.sh` 检查，不自动装 PVE 组件 |
| 存储 | 用户指定，**没有默认值** | 选错代价太大，强制每次确认 |

自身开销（实测，Debian 13 / amd64）：常驻约 **12 MiB**，单文件 **12.7 MB**（前端已编入），
登录瞬间峰值约 **45 MB**（scrypt 的 32 MB 工作区）。systemd 单元给到 384 MB，是因为
`pct`/`pvesh` 作为子进程也记在同一个 cgroup 上——账本与实测数据见
[`docs/design.md`](docs/design.md) §7.5。

## 当前限制

| 限制 | 说明 |
|---|---|
| 单节点 | `pve.node` 是配置里写死的节点，平台不做调度；集群只警告不阻止 |
| IPv6 跨 zone | 同 zone ULA 互通、NAT66 出公网都可用；每个 zone 是独立 /64，**网关之间没有 v6 静态路由**，跨 zone 只有 v4 通 |
| 只纳管 LXC | QEMU 虚拟机会出现在"宿主上的其它虚拟机"列表里，但不能纳管 |
| 纳管只登记 | 点「纳管」只写进规格并绑 vmid，**不改容器本身**；网络/主机名要等下一次 `apply` |
| 快照看存储 | 厚置备 LVM 的快照在 PVE 9 仍是 tech preview；`doctor` 会列出哪些存储支持快照，实际失败时原样透传 PVE 的报错 |
| 不做自动备份 | 备份策略归运维，平台不代管。停机 `tar` 一份 `/etc/labyard` + `/var/lib/labyard` 即可 |
| 认证只有单密码 | 单管理员密码 + 签名 cookie，没有多用户、TOTP、IP allowlist、审计日志。**不要直接暴露到公网**，放内网或套一层反代 |
| 升级只向前 | 替换二进制即可；DB 迁移按编号自动前滚，不提供降级 |

## 技术栈

| 层 | 选择 | 为什么 |
|---|---|---|
| 语言 | Rust（1.80+） | 常驻 12 MiB、无 GC 抖动、单文件交付 |
| HTTP | axum 0.8 | 与 tokio 同一 runtime，SSE/WebSocket 原生 |
| DB | rusqlite（bundled） | 6 张表、按主键取一行，不需要 ORM |
| 配置 | serde_yaml_ng | `serde_yaml` 已归档，这是活跃继任 |
| 前端 | React 18 + TypeScript + Ant Design 5 + Vite + React Flow | 成熟组件库；拓扑图用 react-flow + dagre，懒加载 |
| PVE 通道 | **原生命令**（`pct`/`pvesh`） | 见下 |

**为什么不用 PVE 的 HTTP API**：`pct exec`、写 `/etc/pve/lxc/*.conf`、启停 systemd 模板
实例、配 `tc mirred`——这四件事 API 全都做不到。本地运行时用 API 只会多一条通道、
多一套 token 与 ACL 要维护。命令直连是单节点部署的正确形态。

## 目录

```
src/
├── core/        spec（YAML→模型）、plan（步骤图）、runner（子进程）
├── pve/         pct/pvesh 命令拼装 + JSON 解析 + 能力探测
├── resources/   执行器：probe → decide → apply；net/gateway 脚本生成
├── db/          SQLite + 编号迁移
├── api/         axum 路由、SSE、认证、规格 CST 编辑、拓扑、终端反代
│                （前端产物由这里的 rust-embed 从 web/dist 嵌入）
└── config.rs    宿主配置
assets/          执行体（base/bootstrap.sh、base/labmirror、provisioners/）
web/             React + AntD 源码（构建产物在 web/dist，被嵌入二进制）
bench/           三语言内存对照 + 分配器对照（可复现）
installer/       install.sh（宿主一键安装 / 卸载）
scripts/         smoke.sh（HTTP 冒烟）/ api-regress.py（真机逐接口回归）
                 / dev.sh / measure.sh
docs/design.md   唯一权威设计文档
docs/operations.md  安装、升级、备份恢复、排障
```

## 开发

```bash
make            # 前端 + 后端（顺序不能反：二进制在编译期嵌入 web/dist）
make dev        # 只跑前端 dev server，代理到 8443
make test       # 单元测试
make check      # 快速类型检查
```

| 测试 | 命令 | 需要 PVE |
|---|---|---|
| 单元测试（纯函数：脚本生成、哈希、YAML 编辑、拓扑） | `make test` | 否 |
| HTTP 冒烟（路由、认证、规格读写、无 PVE 时降级） | `scripts/smoke.sh` | 否 |
| 真机逐接口回归（每个端点打一次，含负路径与写后读回） | `scripts/api-regress.py --base https://127.0.0.1:8443 --password …` | 是 |

## 设计上写死的几件事

**进程组必须杀干净。** `pct exec` 起的是 `apt`，超时只杀父进程会留下孤儿占着 dpkg 锁，
下次 apply 直接卡死。所以子进程一律 `process_group(0)`，取消时 `kill(-pid)`。

**读状态禁止 N+1。** `/etc/pve` 是单线程 FUSE，20 台机各查一次会把整个节点拖慢。
状态一律用一次 `pvesh get /cluster/resources --type vm` 取全。

**`pct exec` 全局串行。** 单个 `pct exec`（lxc-attach）实测峰值 ~176 MB，全记在服务的
cgroup 上，批内并发就会把内存上限变成赌博。所有容器内执行走一个容量为 1 的信号量，
代价是 bootstrap 阶段墙钟变长，换取内存可算。

**并发 3 是实测算出来的。** 并发 1/2/3/4 的耗时是 200/105/70/95 秒，磁盘峰值
30/60/120/240 MB/s——到 3 之后耗时不再降，磁盘还在线性涨。

**背压丢中间不丢尾。** 子进程输出走有界 channel（500 行），满了丢中间并插入
`[已省略 N 行]`。报错通常在尾部，尾部绝不丢。

## 文档

| 文件 | 内容 |
|---|---|
| [`docs/design.md`](docs/design.md) | 唯一权威设计文档：数据模型、执行语义、每处取舍的理由与踩过的坑 |
| [`docs/operations.md`](docs/operations.md) | 安装、升级与回滚、备份恢复、排障、纳管已有容器、已知限制 |
| [`examples/`](examples/) | 单 zone、多 zone、嵌套 zone 三份可直接跑的规格 |

## 许可

Apache-2.0，见 [`LICENSE`](LICENSE)。Proxmox VE 本体是 AGPL-3.0，labyard 只调用其命令行
工具，不链接、不分发其代码，因此不受传染。
