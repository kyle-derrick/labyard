# labyard

声明式攻防实验网络编排。给 Proxmox VE 单节点用：写一份 YAML，得到一个可以复现、
可以推倒重来的实验网络。

**状态：可运行的骨架。** 核心引擎、PVE 命令层、Web 界面、幂等与并发控制已完成并有
单元测试；**尚未在真实 PVE 9 上跑过完整链路**（详见文末「已验证 / 未验证」）。

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
- IPv6 默认开启，只发 ULA（`fd` 开头，按 zone 名确定性派生）+ NAT66，不向上游要前缀
- 靶机（server/client）**不给默认路由**，只能从内网发起
- `nat: true`（默认）让网关做地址转换；`nat: false` 只转发不转换，用于上游已经路由了整个网段的情形
- 同角色主机不写 `ip` 时自动取不同主机位（两台 `server` 是 `.20`/`.21`，不会撞）
- 幂等的单位是「步骤 key + 输入哈希」，不是版本号

Web 界面里编辑的规格保存在服务端（`paths.specs/lab.yaml`），刷新不丢；改动未保存时标题旁有提示。

## 快速开始

```bash
# 在 PVE 宿主上（Debian 13 / PVE 9.x）
./installer/install.sh

# 浏览器打开 https://<宿主IP>:8443
# 初始密码在安装结束时会打印一次
```

或者只用命令行：

```bash
labyard doctor                                   # 环境自检，缺什么直接说
labyard apply -f examples/main.yaml --dry-run    # 只打印计划
labyard apply -f examples/main.yaml              # 执行
labyard apply -f examples/main.yaml              # 再跑一次：全部跳过
```

## 三条命令

| 命令 | 用途 |
|---|---|
| `serve` | Web 界面（唯一完整的操作界面） |
| `doctor` | 自检：PVE 版本、`pct`/`pvesh`/`pveam` 是否可用、存储能力、桥是否存在 |
| `apply` | 无界面执行，给测试与 CI 用 |

没有第四种操作型命令。所有日常操作走 Web，避免两套逻辑慢慢长歪。

## 环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| PVE | **9.x** | 低于 9.0 拒绝启动，不做 8.x 兼容层 |
| 节点 | 单节点 | 集群只警告不阻止 |
| 架构 | amd64 / arm64 | 交叉编译见 `make cross` |
| 宿主依赖 | `pct` `pvesh` `pveam` `ip` `nft` | `install.sh` 检查，不自动装 PVE 组件 |
| 存储 | 用户指定，**没有默认值** | 选错代价太大，强制每次确认 |

**自身开销（实测，Debian 13 / arm64，HTTP 模式）**：常驻 **6.9 MB**、二进制 **8.0 MB**
（含 1.07 MB 前端）、峰值 **38.1 MB**。

峰值比常驻高 31 MB，原因只有一个：`scrypt(N=2¹⁵, r=8)` 的工作内存是
`128·N·r = 32 MB`，只在启动生成密码和登录的那一瞬间存在。实测确认——`VmHWM`
在启动完成时就已到 38.1 MB，之后 200 个并发请求（20 并发）只让常驻从 6.9 涨到 7.5 MB。
8 个并发登录只把峰值从 38.1 推到 39.2 MB——这是哈希串行化真的生效的证据。

因此 `systemd` 的 `MemoryMax` 取 **64 MB**（32 + 7 + 余量），并且密码哈希与规格解析
都做了串行/限流。取 16 MB 会在第一次登录时被 OOM kill。

**内存分配器**：实测**不用换**。glibc 默认 4.4 MB / churn 后 5.5 MB，jemalloc 是
6.2 / 10.8，mimalloc 是 10.4 / 27.5。复现见 `bench/alloc/`。

## 技术栈

| 层 | 选择 | 为什么 |
|---|---|---|
| 语言 | Rust 1.100 | 常驻 6.9 MB、无 GC 抖动、8.0 MB 单文件（含前端） |
| HTTP | axum 0.8 | 与 tokio 同一 runtime，SSE/WebSocket 原生 |
| DB | rusqlite（bundled） | 10 张表、按主键取一行，不需要 ORM |
| 配置 | serde_yaml_ng | `serde_yaml` 已归档，这是活跃继任 |
| 前端 | React 18 + TypeScript + Ant Design 5 + Vite | 成熟组件库，少写表单 |
| PVE 通道 | **原生命令**（`pct`/`pvesh`） | 见下 |

**为什么不用 PVE 的 HTTP API**：`pct exec`、写 `/etc/pve/lxc/*.conf`、启停 systemd 模板
实例、配 `tc mirred`——这四件事 API 全都做不到。本地运行时用 API 只会多一条通道、
多一套 token 与 ACL 要维护。命令直连是单节点部署的正确形态。

## 目录

```
src/
├── core/        schema（YAML→模型）、plan（步骤图）、runner（子进程）
├── pve/         pct/pvesh 命令拼装 + JSON 解析 + 能力探测
├── resources/   执行器：probe → decide → apply；网络脚本生成
├── db/          SQLite + 编号迁移
├── api/         axum 路由、SSE、认证、静态资源
├── web/         rust-embed 嵌入前端产物
└── assets/      执行体（bootstrap.sh、ttyd attach、provisioner）
web/             React + AntD 源码
bench/           三语言内存对照 + 分配器对照（可复现）
docs/design.md   唯一权威设计文档
```

## 设计上写死的几件事

**进程组必须杀干净。** `pct exec` 起的是 `apt`，超时只杀父进程会留下孤儿占着 dpkg 锁，
下次 apply 直接卡死。所以子进程一律 `process_group(0)`，取消时 `kill(-pid)`。

**读状态禁止 N+1。** `/etc/pve` 是单线程 FUSE，20 台机各查一次会把整个节点拖慢。
状态一律用一次 `pvesh get /cluster/resources --type vm` 取全。

**并发 3 不是拍脑袋。** 实测并发 1/2/3/4 的耗时是 200/105/70/95 秒，磁盘峰值
30/60/120/240 MB/s——并发到 3 之后耗时不再降，磁盘还在线性涨。

**背压丢中间不丢尾。** 子进程输出走有界 channel（500 行），满了丢中间并插入
`[已省略 N 行]`。报错通常在尾部，尾部绝不丢。

## 已验证 / 未验证

| 状态 | 内容 |
|---|---|
| ✅ 单元测试通过 | 50 项：YAML 三层合并、zone 拓扑与环检测、ULA 前缀合法性、步骤哈希确定性、进程组杀孙进程、超时、取消、密码哈希、cookie 签名、VMID 分配、分配器对照 |
| ✅ HTTP 冒烟通过 | 26 项（`scripts/smoke.sh`）：健康检查、SPA 回落、认证、单/嵌套 zone 预览、vmid 回填、ULA 合法性、规格读写与原子性、apply 前置拦截、错误输入、无 PVE 时优雅降级、OpenAPI |
| ✅ 容器内验证 | Rust 编译、`cargo test`、前端 `vite build`、二进制体积（8.0 MB）与内存（常驻 6.9 MB / 峰值 38.1 MB，含 scrypt 32 MB 工作区）、真浏览器渲染与规格读回 |
| ⚠️ 仅设计推演 | PVE 9 的字段级行为（`network`、`lxc`、`snapshot`）、`pveam` 模板列表结构、厚 LVM 快照 |
| ❌ 完全未验证 | 真实 PVE 上的完整 apply、IPv6 双栈全链路、ttyd 反代与文件传输、升级迁移 |

**mock 全绿不等于真机通过**，这一条不含糊。第一次真机运行请先跑 `labyard doctor`。

## 许可

Apache-2.0。Proxmox VE 本体是 AGPL-3.0，labyard 只调用其命令行工具，不链接、不分发
其代码，因此不受传染。
