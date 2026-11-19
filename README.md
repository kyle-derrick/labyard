# labyard

声明式网络编排。给 Proxmox VE 单节点用：写一份 YAML，得到一个可以复现、
可以推倒重来的实验网络。

**状态：功能完整，已在真实 PVE 9.2.2 上端到端验证。** 核心引擎、PVE 命令层、Web 界面
（可视化编排 / 拓扑图 / 网关面板 / 运维 / 终端）、幂等与并发控制全部实现，99 项单元测试、
26 项 HTTP 冒烟、82 项真机逐接口回归全部通过（详见文末「已验证 / 未验证」）。

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
- 主机基础配置同样三层合并：`timezone`（时区）、`lang`（locale，如 `zh_CN.UTF-8`）、`apt_mirror`（apt 源，无外网机器可指向内网 apt-cacher-ng）、`packages`（预装包；`packages: []` 显式不要——无外网主机用它退出继承）
- `storage` 是 rootfs 落点；`template_storage` 是模板（vztmpl）落点，**默认跟随 `storage`**。rootfs 用 lvmthin/zfspool 时模板必须分开放（如 `template_storage: local`），一体存储（dir/NFS）零配置
- IPv6 默认开启，只发 ULA（`fd` 开头，按 zone 名确定性派生）+ NAT66，不向上游要前缀
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
  `revive` 原样回来；destroy 才是真删。
- 规格可以有多份：`paths.specs/*.yaml` 下的文件就是唯一真相源，Web 里可新建/复制/删除，
  单个 zone 可以单独打开编辑（原位改，保留注释与顺序）

写入走「先 parse + resolve 校验，再临时文件 + rename 原子替换」——非法 YAML 一律 400 且不碰旧文件。刷新页面从服务端读回；改动没保存时标题旁有提示。

## 快速开始

```bash
# 在 PVE 宿主上（Debian 13 / PVE 9.x）
./installer/install.sh

# 浏览器打开 https://<宿主IP>:8443
# 初始密码只在服务首次启动时打进 journal 一次：
journalctl -u labyard | grep 初始管理员密码
# 忘了就删 /var/lib/labyard/password.hash 再 systemctl restart labyard
```

或者只用命令行：

```bash
labyard doctor                                   # 环境自检，缺什么直接说
labyard apply -f examples/main.yaml --dry-run    # 只打印计划
labyard apply -f examples/main.yaml              # 执行
labyard apply -f examples/main.yaml              # 再跑一次：全部跳过
```

## 四个子命令

| 命令 | 用途 |
|---|---|
| `serve` | Web 界面（日常操作都在这里） |
| `doctor` | 自检：PVE 版本、`pct`/`pvesh`/`pveam` 是否可用、存储能力、桥是否存在 |
| `apply` | 无界面执行一份规格，给测试与 CI 用 |
| `destroy` | 删掉一个 zone 的全部资源（不可逆，要求显式传 zone 名） |

没有 `zone create` / `host add` 这类操作型子命令：两套界面维护同一套语义，
迟早会不一致。

## 环境要求

| 项 | 要求 | 说明 |
|---|---|---|
| PVE | **9.x** | 低于 9.0 拒绝启动，不做 8.x 兼容层 |
| 节点 | 单节点 | 集群只警告不阻止 |
| 架构 | amd64 / arm64 | 交叉编译见 `make cross` |
| 宿主依赖 | `pct` `pvesh` `pveam` `ip` `nft` | `install.sh` 检查，不自动装 PVE 组件 |
| 存储 | 用户指定，**没有默认值** | 选错代价太大，强制每次确认 |

**自身开销（实测，Debian 13 / amd64，HTTPS，含拓扑图与规格页）**：常驻 **12.0 MiB**
（`VmRSS` 12248 kB）、二进制 **12.7 MB**（12,703,216 B，含 1.48 MB 前端）、峰值
**44.8 MB**（`VmHWM` 44784 kB，登录那一瞬间）。

峰值比常驻高 33 MB，原因只有一个：`scrypt(N=2¹⁵, r=8)` 的工作内存是
`128·N·r = 32 MB`，只在启动生成密码和登录的那一瞬间存在。实测确认——`VmHWM`
在登录完成时就已封顶，之后反复刷新拓扑 / 规格 / 主机列表，常驻稳定在
12.0–12.1 MB（12024 → 12092 kB）。这也是登录必须串行化（`hash_gate`）的原因：
并发 scrypt 会把工作区复制成 8 份。

因此 `systemd` 的 `MemoryMax` 取 **384 MB**（3 × 70 的子进程 + 12 常驻 + 32 哈希 + 余量），
`MemorySwapMax=128M` 兜住极端情况，并且密码哈希与规格解析都做了串行/限流。
取 16 MB 会在第一次登录时被 OOM kill；**64 MB 与 192 MB 也都翻过车**：`pct`/`pvesh`
是它的子进程、同属一个 cgroup，单个峰值 RSS 176 MB（`/usr/bin/time -v` 实测）。
64 MB 时跑几小时就开始报"密码错误"（scrypt 工作区分配失败），192 MB 时 `doctor`
四个 `pvesh` 并发直接 oom-kill。详细账本在 `docs/design.md` §7.5 第 1 条。

**内存分配器**：实测**不用换**。glibc 默认 4.4 MB / churn 后 5.5 MB，jemalloc 是
6.2 / 10.8，mimalloc 是 10.4 / 27.5。复现见 `bench/alloc/`。

## 技术栈

| 层 | 选择 | 为什么 |
|---|---|---|
| 语言 | Rust 1.100 | 常驻 12 MiB、无 GC 抖动、12.7 MB 单文件（含前端） |
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
| ✅ 单元测试通过 | **101 项**：YAML 三层合并、zone 拓扑与环检测、ULA 前缀合法性、步骤哈希确定性、生成器版本参与哈希、apt 缓存隐式依赖、zone 片段往返、nft 原子批次与 `dnat ip to`、映射回程 SNAT、VPN 关闭落地、进程组杀孙进程、超时、取消、密码哈希、cookie 签名、VMID 分配、NAT 开关、DNS 取值、bootstrap 变量名契约、分配器对照 |
| ✅ HTTP 冒烟通过 | 26 项（`scripts/smoke.sh`）：健康检查、SPA 回落、认证、单/嵌套 zone 预览、vmid 回填、ULA 合法性、规格读写与原子性、apply 前置拦截、错误输入、无 PVE 时优雅降级、OpenAPI |
| ✅ 真机逐接口回归 | 90 项（`scripts/api-regress.py`）：路由表里每个端点各打一次，含负路径（非法 action、不存在的主机、重复连线、`confirm` 不匹配、快照名注入）与"写完读回断言"（`egress` 真的关了、`retired` 标记真的翻、镜像连线真的没了）。它同时是三条 bug 的复现用例：裸 host 镜像连线删不掉、retire 监控机时镜像单元按前缀匹配不到、已结束 run 的 SSE 永不收流 |
| ✅ 容器内验证 | Rust 编译、`cargo test`、前端 `vite build`、二进制体积（12.7 MB，含 1.48 MB 前端）与内存（常驻 12.0 MiB / 峰值 44.8 MB，含 scrypt 32 MB 工作区）、真浏览器渲染与规格读回 |
| ✅ **真实 PVE 9.2.2 全链路** | `doctor` 9 项全绿；`apply` **14/14** 步成功（含 bootstrap）；网关拿 `.1`、NAT 出网生效、内网主机**无默认路由**、宿主入向被网关丢弃；幂等重跑全部跳过；基础配置实测生效（时区 CST、`zh_CN.utf8` locale 生成、vim/curl/jq 装入、源指向 tuna、主机 opt-out 不装包）；`destroy` 全路径验证（硬检查、CT/桥/interfaces 清理、销毁后重建闭环）|
| ✅ **Web 运维** | 主机表格（电源 start/stop/reboot）、快照（列/建/回滚/删）、run 历史与明细、retry、destroy（zone 名二次确认）。真机逐项验证通过 |
| ✅ **流量监控** | `zone.monitor` 开整网采集：每个被监控网络一块抓包桥，监控机按主机逐口镜像（tc mirred，双向），监控机可挂本网段/外网/任意网络（`mgmt`），也可复用别的 zone 已有监控机（`use`）；精确连线走 `mirrors:`。采集单元按桥探测落点自愈 |
| ✅ **网关策略** | `egress` 关：`forward` 里 drop 排在 `established` 之前，容器立刻断网（含已建立连接）；开：`http=200`。端口映射 `tcp:17681 → 10.77.0.20:7681` 实测可穿透（回 `404` 是 ttyd 自己的响应，说明链路通），删除后端口关闭 |
| ✅ **VPN** | WireGuard：`wg0` 起来、公钥/peer/allowed-ips 正确、客户端 `.conf` 可下载；OpenVPN：`openvpn-server@labyard` active、监听 1194、`.ovpn` 可下载。**关闭也落地**（`disable --now` + 删 `wg0`），平台显示与实际一致 |
| ✅ **retire / revive** | 主机级与整 zone 级都闭环：停机 → 摘镜像单元 → 拓扑折叠，保留 vmid/IP/磁盘/快照；revive 原样回来且 `gateway` / `mirror` 自动重跑恢复 |
| ✅ **多规格管理** | 创建/复制（重名 409、非法名 400）、列表、zone 片段单独读写（往返只做块状序列化的风格归一，注释与其他 zone 不动）、增删 zone、删除规格 |
| ✅ **拓扑** | `/api/topology` 输出**两类节点两类边**：网络（外网 + 各 zone 内网）与主机，边是 `attach`（方向即数据流：外网→网关→内网→主机）与 `mirror`（主机→监控机虚线，一台主机一条）。前端 react-flow + dagre 只负责画。图上可开抽屉改运行态、Shift 框选后右键加镜像、悬停监控机高亮其采集链路 |
| ✅ **可视化编排** | `GET/PUT /api/specs/{name}/model` 做 YAML ↔ JSON 投影，前端用卡片 + 表格改结构，保存走与手工粘贴**同一条校验**（`Spec::parse` + `resolve_all`）；YAML 是可切换的高级选项。代价写在界面上：保存会重排 YAML 并丢注释 |
| ✅ **重启自愈** | 地址与 nft 规则落成容器内开机单元（`labyard-net` / `labyard-nft`）：`pct reboot` 后地址、含端口映射的完整策略、关闭态的 egress 都原样恢复。这一步是真机发现的——早先只 `ip addr` 一次性下发，容器重启后整个 zone 静默失联，网关静默 fail-open |
| ✅ **Web 终端** | ttyd 1.7.7 静态二进制注入容器（systemd 单元、绑容器内网地址）→ axum HTTP+WS 反代（hyper client + tokio-tungstenite）→ iframe 嵌入（ttyd 自带 xterm.js、CJK 输入、ZMODEM 传文件）。真机验证：页面 200（729 KB）、WS 握手 `101 Switching Protocols` |
| ⚠️ 仅设计推演 | IPv6 出公网（宿主无 IPv6 上游，环境限制；区域内 ULA 互通与 NAT66 已实测）、厚 LVM 快照、`pct snapshot`、升级迁移（**从未发布，刻意不做**） |
| ❌ 明确没做 | 自动发现外来成员（`observed`/`unknown`）、IP allowlist、TOTP、审计表、`ionice`、apply 前资源预检、`doctor --trace`。逐条都在 `docs/design.md` §5.9 / §7.6.5 / §8.4 / §9 写明，并在原位置标注了"这是设想，没有实现" |

**mock 全绿不等于真机通过**，这一条不含糊——现在的每一条 ✅ 都有真机证据。

## 许可

Apache-2.0。Proxmox VE 本体是 AGPL-3.0，labyard 只调用其命令行工具，不链接、不分发
其代码，因此不受传染。
