# labyard · 实验室编排平台设计文档

> 项目名：**labyard**（`lab` + `yard`，铁路编组场——按需把"网络单元"编组起来）
> 许可证：**Apache-2.0**（带专利授权；Proxmox VE 本体是 AGPL-3.0，但我们只通过 subprocess 调用其 CLI、读写其配置文件，不链接其代码，不传染）
> 基线环境：**Proxmox VE 9.x（Debian 13 Trixie）/ 单节点**。8.x 只做明确报错，不做兼容层
> 状态：**已实现并在真实 PVE 9.2.2 上端到端验证**（2026-11）。本文是权威设计文档：
> 架构、数据模型、接口与取舍，以及实现时被真机推翻/修正的地方（带日期标注）。

---

## 0. 结论先行

四句话：

1. **Web 是主界面，YAML 是真相源**。规格文件（`paths.specs/*.yaml`）是唯一权威状态，网页提供的是"改结构 → 预览计划 → 应用 → 看拓扑/主机/日志"。**改结构默认可视化**（卡片 + 表格），YAML 是可切换的高级选项，两者写的是同一个文件。理由：写进命令行的东西是一堆 `--storage local-lvm --template debian-13-standard_13.2-1_amd64.tar.zst`，容易写错版本号；而只让用户手写 YAML，等于把这道门槛当成筛选器。文件仍然是真相：能 `cat`、能进 git、能手改回去。可视化前端只在改一棵 JSON 树，解析与校验全在后端（§8.1），所以不会长出"表单能表达、YAML 不能表达"的第二套语义。
2. **但核心必须是库，不是网页的一部分**。`src/core` 不 import `axum`，Web 只是它的一个调用者。这样 E2E 测试可以驱动引擎（`labyard apply -f x.yaml --yes`），却不需要维护第二套用户界面。
3. **全新实现，但继承经验**。旧目录（`lab-net/` 等）全部废弃，不进依赖链。旧实现里踩过的六个坑（空 set、0 字节密钥、`enabled-runtime`、tun 必须 bind mount、veth 名漂移、reset 清状态）直接变成回归测试项——这些比代码值钱。新实现连"搬运旧脚本"这一步都省了：网关策略与 VPN 是由 Rust 按规格**生成** nft 规则与 shell 脚本，再作为开机单元落进容器（§8.6.1、§8.6.6），仓库里不再有 `assets/gateway/` 这种待搬运的脚本目录。
4. **幂等的单位是"步骤 + 输入哈希"，不是"跑没跑过"**。每次都先 probe 实际状态、再 diff、再 apply；配置没变就 skip，脚本改了哈希变了就重跑。这是"更新"和"重复执行"能同时成立的唯一办法。

**关于"要不要 CLI"**，我的取舍是：**不做操作型 CLI，只留四条运维命令**——

| 命令 | 用途 | 为什么不能砍 |
|---|---|---|
| `labyard serve` | 起服务（systemd 调它） | 服务入口 |
| `labyard doctor` | 打印环境探测结果（PVE 版本/存储能力/tun/桥/依赖） | 出问题时唯一能贴给别人的东西；Web 挂了它还活着 |
| `labyard apply -f <file> --yes` | 非交互执行一份 spec | **给自动化测试用**，不是给用户用 |
| `labyard destroy -f <file> --zone <z>` | 删掉一个 zone 的全部资源，不可逆 | 拆环境的最后手段；要求显式传 zone 名，和 Web 端的二次确认同源 |

`serve` 另有 `--port` / `--concurrency`，`apply` 另有 `--dry-run` / `--snapshot` / `--concurrency`。
除此之外没有任何 `labyard zone create` / `host add` 之类的子命令。两套界面维护同一套语义，成本远大于收益，而且必然有一天会不一致。

---

## 1. 起点：旧目录全部废弃

`lab-net/`、`网关+镜像环境模拟/`、`pve-lab/` 一律**不再维护、不作为运行时依赖**。labyard 是全新实现，自带 provisioner，不 `pct push` 任何旧文件。

但旧代码里踩过的坑不能丢——它们变成新平台的**回归测试项**，比代码本身值钱：

| 旧实现 | 教训 | 在 labyard 里的位置 |
|---|---|---|
| `labgw` 的 named set 端口闸 | 空 set 让规则永不匹配，端口"看起来是关的"，排查方向全错 | 新实现**不用 named set**：端口映射直接展开成 `tcp dport { 8080, 9090 }`，一条映射都没有时整行不生成。没有空集合，也就没有空集合的坑 |
| `labvpn` 的 0 字节 `server.key` | `wg genkey > f` 在 `wg` 缺失时留下空文件，之后 `[ -f ]` 判定存在、跳过重建 | 所有密钥判断改成 `[ -s f ]`（非空才算存在）；现存的坑做成回归测试（`gateway.rs` 里断言生成脚本含 `[ -s ... ]`） |
| `openvpn@server` 模板单元 | `is-enabled` 返回 `enabled-runtime` 且 exit 0，"据此判断已启用"是错的 | 不再借 systemd 模板单元的语义判断状态：VPN 的开关由生成脚本自己落地（`enable --now` / `disable --now` + 删设备），平台只信容器里的实际运行态 |
| 无特权 LXC 的 tun | 容器内 `mknod` 必然 EPERM；必须让宿主把设备交给容器，且 Stop→Start 才重新读 config | `pct set <vmid> --dev0 /dev/net/tun` 后重启容器（`pct set` 对已运行容器不会生效）；不去手写 `/etc/pve/lxc/<id>.conf`，也不依赖 PVE 的重启编排 |
| `labmirror` 硬编码落点 veth | 监控机重建后 veth 名变，规则静默失效 | 镜像单元名是 `{zone}-{源vmid}-{目标vmid}`，每次 mirror 步骤按**当前** vmid 重算；停单元时按前缀（源端）+ 后缀（目标端）双向 glob，见 `mirror_unit_globs` |
| `labgw reset` 清空 set | 重置后出网/映射全丢，需要手工恢复 | 不再有 `reset` 子命令。每次网关步骤**整表重建** `inet labyard`（§8.6.1），规则与规格永远一致，没有"重置后忘了恢复"这个状态 |

边界说清楚：**旧目录不进新仓库的依赖链**；其中的实现可以按需搬运（同一作者、同样 Apache-2.0），但搬运前必须补齐上面的测试项，否则宁可重写。

关键判断没变：**缺的不是"脚本能力"，是"编排能力"**——谁先建、建在哪、建过没有、改过没有、失败停在哪一步。重心在 §4 的引擎。

---

## 2. 目标与非目标

### 2.1 做

- 一份 YAML 一次性建出：桥（内网 / 镜像网）、LXC（网关 / 分析 / 服务端 / 客户端）、容器内网络、基础系统配置、镜像接线
- 多规格管理、全局拓扑图、可视化编排（YAML 为高级选项）+ 计划预览、任务日志、运行时操作（出网 / 端口映射 / VPN / retire）
- 重跑安全：已完成的跳过，改过的更新，失败的中断点续跑
- 自由组合：`main` 上挂 `suba`、`subc`，`subb` 再挂 `suba`；每个 zone 的网关 WAN 接上一级的内网桥
- 统一命名与地址分配（v4 + v6），分配结果持久化，重跑不重排

### 2.2 不做（明确写死，避免无限膨胀）

- **不做 VM（qemu）**。只建 LXC；要 Windows / 带内核的虚拟机，你自己用 PVE 建，平台负责把它接进哪张桥。责任分开
- **不装业务软件**。server 上跑 nginx、client 上装 nmap、监控机上装 ELK，都不管（见 §6.2）
- **不做操作型 CLI**。只有 `serve` / `doctor` / `apply` / `destroy` 四条命令（§0）
- 不做多租户 SaaS、不做计费、不做镜像仓库
- **发现只做提示，不自动纳入**。宿主上手工建的 LXC 会列在“宿主上的其它虚拟机”里，可一键纳管（写规格 + 绑 vmid），但不会自己进规格——磁盘上的文件仍是唯一真相（§5.9）
- **不做多节点编排**（首版）。集群环境只警告不阻止，但 zone 强制绑当前节点
- 不替代 PVE 的备份 / 快照底层（只调用它）
- 不自己实现流量镜像、VPN、防火墙（用内核 tc/nft 和 wireguard/openvpn）
- 不做通用 IaC 平台（只服务"网关 + 内网 + 接入主机 + 镜像"这一种拓扑）

---

## 3. 总体架构

```
                    ┌──────────────────────────────────────────┐
   浏览器 ──HTTPS──▶│ labyard  (Rust 单二进制，含内嵌前端产物)      │
                    │   :8443  systemd: labyard.service          │
                    └───────────────┬──────────────────────────┘
                                    │ 同一进程，同一 core
                    ┌───────────────▼──────────────────────────┐
                    │ core: schema → planner → runner          │
                    │  · specs/*.yaml: 期望    SQLite: 哈希/run │
                    │  · 每步 probe → diff → apply             │
                    └───┬───────────────┬──────────────┬───────┘
                        │               │              │
           pct/qm/pvesh │        pct exec│      systemd │
                        ▼               ▼              ▼
                 PVE 原生 CLI       CT 内执行      labmirror@<net>
              bridge/lxc/snapshot  install.sh     tc mirred 规则
                                   labgw/labvpn
```

### 3.1 进程模型

- **单进程**：`axum` 跑在 tokio 多线程 runtime 上，前端产物用 `rust-embed` 编进二进制，由同一个 `Router` 托管。不引 nginx、不分发静态目录。
- **任务执行**：进程内 tokio task + `CancellationToken` 树 + 每 zone 一把文件锁（`fs2`）。不引 Redis/队列。
  理由：单机、低频（一天几次 apply）、任务时长分钟级。消息队列要 broker + worker + 看板，运维成本远超收益。
- **长任务**：`POST /apply` 立即返回 `run_id`，前端订阅 `/api/runs/{id}/events`（SSE）。进程重启后 `run` 标记 `interrupted`，可一键续跑。
- **取消**：一个 run 对应一个 `context`，取消时 `ctx.Done()` 直接传到 `exec.CommandContext`，子进程被 `SIGTERM`→`SIGKILL`，**不需要手工维护 token 树**。
- **权限**：`labyard.service` 以 root 运行（要 `pct`、`ip`、`write /etc/pve/lxc/*.conf`、`systemctl`）。这是必须承认的事实，不是可以绕过的设计——见 §8.4 的暴露面收紧。
- **core 与 API 之间只有函数调用**，没有 HTTP 回环。`labyard apply`（测试用）也直接调 core，不起 HTTP。

### 3.2 为什么不选 Ansible / Terraform / Proxmox VE Helper-Scripts

| 方案 | 能给的 | 为什么不用 |
|---|---|---|
| Ansible + `community.proxmox` | 幂等模块、inventory | 状态不可查询（要靠 `--check` 重算）、Web 集成要包一层 subprocess、容器内执行还要 SSH/agent。等于自己写编排层，却多背一套 YAML 方言 |
| OpenTofu/Terraform + `bpg/proxmox` | 真 plan/apply、state | 只管 PVE 资源，管不到容器内软件；state 文件与 Web 并发编辑冲突；多背一套 HCL 方言与 provider 版本约束，而这些能力我们用 `pct`/`pvesh` 已经直接有了 |
| 现成 helper-scripts | 单容器创建 | 一次性脚本，无拓扑、无依赖、无状态 |

结论：**编排逻辑必须自己写（这是产品本身），但每一层能力都用成熟库**——见 §7。

### 3.3 为什么是"宿主上的 Web 服务"，不是本地桌面端

桌面端（Tauri / Electron / 本地跑后端 + 浏览器）看着更轻：不用常驻、随时打开。但它一旦离开宿主，就必须远程操作 PVE——而本项目有**四类操作任何 API 都做不到**：

| 操作 | API 能做吗 | 远程就只能 |
|---|---|---|
| `pct exec` 容器内执行 | **否**，REST API 没有 exec 端点 | root SSH |
| 写 `/etc/pve/lxc/<id>.conf`（tun 直通） | **否**，只接受它认识的 key | root SSH |
| `systemctl enable --now labmirror@<net>` | **否** | root SSH |
| `tc` 流量镜像规则 | **否** | root SSH |

所以桌面端不是"省掉一个服务"，而是把**一条本地 subprocess 通道**换成**HTTPS + root SSH 两条远程通道**：宿主 22 端口要对你的网络开放、root 密钥落在个人电脑上、还要处理 SSH 断连重试。省下的那十几 MB，换来的是更大的攻击面和更多失败模式。

更硬的一条是**长任务**。一次 apply 要跑几分钟到几十分钟（8 路 `apt install`）。跑在桌面端进程里，合盖、切 Wi-Fi、休眠，run 就中断；幂等引擎能续跑，但 SSE 日志丢了，`interrupted` 状态得人工判断。真要可靠，就得在宿主再装一个"任务守护"——常驻进程又回来了，只是换了名字，还多一层。

因此：**Web 服务常驻宿主是这套架构里最便宜的一环**（约 14 MB、9.9 MB 单文件、systemd 托管、开机自启、故障自愈），砍它省不下什么。

想要"桌面端的手感"，正确做法不是换架构：浏览器打开 `https://<host>:8443`，加一个 `manifest.json` 就能"安装"成独立窗口应用（PWA），体验接近原生，零架构代价。桌面客户端只在"多台 PVE 的统一控制台"场景下才值得做，而那时它是 Web 的多租户版，不是另一个程序。

---

## 4. 核心：幂等引擎

### 4.1 三层状态

| 层 | 存什么 | 存哪 |
|---|---|---|
| desired | 用户写的 YAML（原样，不解析后另存一份） | `paths.specs/*.yaml` |
| observed | 每次 probe 现查的实际状态（CT 配置、桥、容器内版本） | **只在内存里**，一次 run 结束即丢——跨 run 缓存"我以为的实际状态"是 bug 温床 |
| applied | 每个步骤最后一次成功时的输入哈希 | SQLite `step_state.input_hash` |

`apply` 的流程固定四段：

```
1. probe   ── 读实际：bridge 在不在、CT 在不在、配置对不对、容器内装的是哪版
2. plan    ── 生成动作列表：create / update / noop / destroy，带原因
3. confirm ── Web 弹窗展示动作表（破坏性动作要输名字二次确认）
4. run     ── 拓扑排序执行，每步落库；失败的停下，后面的标 skipped
```

### 4.2 步骤身份 = key + 输入哈希

```text
step_key  = "provision:main-gateway"          # 稳定，不含时间
input_hash = sha256(
    provisioner 目录下所有文件内容
  + JSON(渲染后的参数, 键排序)
  + 目标 CT 的 vmid
)
```

- 哈希一致 + 上次 `ok` → **skip**（这才是"已经配好了"的可靠判据）
- 哈希变了 → **重跑**（脚本升级、网段改了、vmid 变了）
- 上次 `failed` → **重试**（不管哈希）

比"打版本号"精确：改了 `nftables.conf` 里一行注释也会重跑，但那是几秒钟的事；而漏跑一次防火墙配置，可能要排查半小时。

### 4.3 部分失败怎么办

最容易出事的场景：CT 建好了，provision 失败。第二次跑时：

- `ct:main-gateway` 的 probe 发现 CT 存在且 cores/mem/net 都对 → `noop`
- `provision:main-gateway` 哈希没变、状态 `failed` → 重跑
- 其余未执行的 → 继续

**不做**"失败就删掉重建"。重建会丢数据、换 IP、换 veth 名（镜像口就断了）。宁可留一个半成品让人看日志。

### 4.4 破坏性变更要单独拦

| 变更 | 处理 |
|---|---|
| memory / cores 增大 | 直接改（LXC 支持热改） |
| memory 减小 | 标 `restart_required`，执行前提示 |
| disk 缩小 | **拒绝**，需手工 |
| 删除主机 | 需 `--allow-destroy` + 输入 zone 名 |
| 改 IP | 标 `destructive`（容器内服务可能写死了旧 IP） |
| 改 bridge 名 | 拒绝，建议新建 zone |

### 4.5 并发与锁

- 每 zone 一把锁：`/var/lib/labyard/locks/<zone>.lock`（`fs2::FileExt::lock_exclusive`）
- Web 上重复点"部署" → 第二次直接返回 409 + 当前 run_id
- 跨 zone 共享资源（被引用的父内网桥、被复用的监控机）→ 加引用计数，删除时检查

### 4.6 生命周期：stop / retire / destroy 三级

"删了就没了"对实验环境太粗暴——经常只是这一轮做完了，下轮还想从同一个基线重来。所以分三级，语义不重叠：

| 动作 | 做什么 | 数据 | 可逆 |
|---|---|---|---|
| `stop` | 关机 CT | 全留 | 是 |
| `retire`（废弃） | 关机 → 摘镜像实例 → 删端口映射 → 从拓扑默认折叠 | **磁盘、快照、桥、IP/VMID 记录全留** | 是，`revive` 原样回来 |
| `destroy`（删除） | 删 CT + 磁盘 + 快照 + 镜像 + 映射；桥无引用才删；释放分配 | 全清 | 否 |

`destroy` 的硬检查（任一不满足就中止，不做"尽力而为"）：

1. 必须 `--yes` 且输入 zone 名（两次确认，防手滑）
2. 桥上若还有**非本 zone** 的成员口 → 不删桥，只删自己的 CT，并打印剩余成员
3. 被其他 zone `uplink` 引用中 → 拒绝，先改上联
4. 共享监控机仍被其他 zone 的 `mirrors` 引用 → 拒绝，先删连线

`retire` 与 `destroy` 的区别只有一个但很关键：**retire 保留 IP 和 VMID 的占用**。否则一下轮重建，同样的 `main-server1` 可能拿到 `10.3.3.14`，你笔记里的地址就全废了。

---

## 5. 数据模型

### 5.1 命名规则（统一，可推导）

| 对象 | 规则 | 示例 |
|---|---|---|
| 网络单元 zone | 小写字母数字，≤10 | `main`、`suba`、`subb` |
| 主机名 | `{zone}-{role}{n}` | `main-gateway`、`suba-gateway`、`main-client1` |
| 内网桥 | `{zone}-inner` | `main-inner`、`suba-inner` |
| 抓包桥 | `mir{zone}`（每个被监控网络一块，纯二层） | `mirlab`、`mirmain` |
| 桥名 | 同上，**≤15 字符**（内核 IFNAMSIZ） | `suba-inner`(10) ✅ `longzonename-inner`(18) ❌ |
| 采集实例 | `labmirror@{zone}-{源vmid}-{目标vmid}` | `labmirror@lab-102-100` |
| 监控机 | `role: monitor` 的主机，可属于任何 zone 或挂外网 | `probe` |
| 状态目录 | `/var/lib/labyard/zones/{zone}/` | |

**15 字符是真限制**，不是风格问题：`IFNAMSIZ` 是 16（含结尾 `\0`），超了 `ip link add` 直接报 `name not a valid ifname`。zone 名限 ≤10 就是为了让 `{zone}-inner` 落在 15 以内。真超了平台在 plan 阶段报错，并给出建议名（截断 + 2 位哈希），不留到建桥那一刻才失败。

PVE 侧同时打 tags：`labyard`、zone 名、角色。这样平台能反查"宿主机上哪些 CT 是我建的"，用于导入/对账，而不是只依赖自己的数据库。

### 5.2 `lab.yaml`（完整示例）

```yaml
# ============ 全局（写一次，多份 zone 文件共用）============
pve:
  node: pve1
  vmid_range: [1000, 1099]

defaults:
  template: debian-13-standard     # 页面上从 pveam available 拉，不硬编码版本号
  storage: null                    # 不设默认值：第一次建 zone 必须在页面手选（选错存储的代价两个月后才显现）
  cores: 2
  memory: 1024                     # MiB
  disk: 8
  unprivileged: true
  apt:
    source: tuna                   # default | tuna | ustc | aliyun | custom
  access:
    mode: key-password             # key | password | key-password（逐台可覆盖）
    ssh_keys: ["~/.ssh/id_ed25519.pub"]
    password: generate             # generate | 明文 | env:VAR

wan:
  bridge: vmbr0                    # 你已有的外网 / 管理桥
  mode: dhcp                       # dhcp | static，逐 zone 可覆盖

ipv6:
  enabled: true                    # 默认开：内网只发 ULA + NAT66（网关自己收上游 GUA 做 NAT66 源）
  ula_prefix: fd42:1ab::/40        # 安装时随机生成；按 zone 序号切 /64

# ============ 网络单元：写几个就是几层 ============
zones:
  - name: main
    node: pve1                     # zone 绑节点，不跨节点（桥是 per-node 资源）
    uplink: wan                    # wan = 接外网桥
    inner:
      cidr: 10.3.3.0/24
      ipv6: auto                   # auto = 从 ula_prefix 切 /64；false = 本 zone 不配 v6
    gateway:
      memory: 1024
      wan: {mode: static, ip: 192.168.33.232/24, gw: 192.168.33.1}
    hosts:
      - {name: server1, role: server, ip: auto}
      - {name: client,  role: client, count: 2, ip: auto}

  - name: suba
    uplink: main                   # 网关 WAN 口挂到 main-inner，IP 从 10.3.3.0/24 取
    inner:
      cidr: 10.4.4.0/24
    hosts:
      - {name: client, role: client, count: 2, ip: auto}

  - name: subb
    uplink: suba                   # 想接多深接多深
    inner: {cidr: 10.5.5.0/24}
    hosts:
      - {name: client, role: client, ip: auto}

# ============ 流量监控：整网开关，写在 zone 上 ============
# monitor 二选一：新建（name + 可选 mgmt）或复用已有（use: "zone:host"）
  main:
    monitor: { name: probe, mgmt: wan }    # 管理口挂外网桥；不写 mgmt = 本网段
  suba:
    monitor: { use: "main:probe" }         # 复用 main 里那台，抓 suba 全网

# ============ 精确连线：单台主机的流量交给某台监控机（可跨网络）============
mirrors:
  - from: "subb:client"
    to: "main:probe"                       # 多个源 → 同一台监控机，不冲突
```

字段语义，逐条说清：

- `uplink: wan | <zone>`：`<zone>` 等价于 `<zone>-inner`。网关的第一块网卡接上联、第二块接自己的内网，这个顺序是**固定的**（nftables 的 `define WAN/LAN` 依赖它，`ports` 表用 `ord` 固定住）。
- `uplink: <zone>` 时，网关 WAN 的 IP 从**被引用 zone 的 `inner.cidr`** 里分配，网关指向该 zone 的网关地址。不需要你手写。
- `inner.cidr` 必填；`inner: null` 表示纯二层单元（不建网关、不分配地址），给"只挂几台机器互打"的场景留口子。
- `node`：zone 固定在一个 PVE 节点上。桥是 per-node 资源，跨节点要 SDN/VXLAN，明确不做。
- `gateway.wan`：不写就继承全局 `wan.mode`；写 `{mode: static, ip, gw}` 或 `{mode: dhcp}` 覆盖。DHCP 时平台等租约并回读地址入库。
- `inner.ipv6`：`auto` 从全局 `ula_prefix` 派生 `/64`；`false` 表示这个 zone 不配 v6（即使全局开了）。
- **`zone.monitor`**：整网采集开关，二选一——`{name: probe, mgmt: wan}` 新建一台监控机（`mgmt` = 管理口挂哪：不写 = 本网段、`wan` = 外网桥 DHCP、`<zone>` = 挂到某个 zone 的内网；资源字段 `cores/memory/disk/template/storage` 可覆盖默认），或 `{use: "main:probe"}` 复用别的 zone 已有的监控机。开启后**每个被监控网络一块抓包桥（`mir{zone}`）**，监控机上每个被抓网络对应一块网卡（eth1、eth2……），本网络每台主机的接口流量复制到对应网口。
- **`mirrors`** 是独立的精确连线表（`from: "zone:host"` → `to: "zone:host"`），不属于任何 zone。所以"给 suba 的某台机也抓包"只是在列表里加一行；`zone.monitor` 是它的批量形态（整网全抓）。两者最后收敛成同一批 `ResolvedMirror`。
- `hosts[].role`：`gateway | monitor | server | client`，只决定"配不配 gw、要不要抓包口、装不装基础工具"，**不决定装什么业务**（见 §6.2）。
- `hosts[].ip`：`auto` 或具体地址。`auto` 的结果写进 DB，不回写 YAML，重跑不重排。
- `count: 2` 展开成 `client1`、`client2`。
- `access` 可在 `defaults`、`zone`、`host` 三层逐级覆盖：`{mode: password, password: env:JUMP_PW}`。

### 5.3 为什么是 `zones` 而不是 `project + parent`

第一版写的是"项目树"，你这句话把它推翻了，而且推得对：

> 对平台来说其实就是一个又一个差不多的网络组合起来了而已。

所以数据模型不是树，是**同构单元的图**。一个 zone 只描述四件事：

```
一个网关 + 一张内网 + 若干接入主机 + （可选）一条上联
```

`uplink` 指向 `wan` 或另一个 zone 的内网，串起来就是你想要的结构：

| zone | uplink | 内网 | 效果 |
|---|---|---|---|
| `main` | `wan`（vmbr0） | 10.3.3.0/24 | 一级 |
| `suba` | `main` | 10.4.4.0/24 | suba 的网关是 main-inner 上的一台机器 |
| `subb` | `suba` | 10.5.5.0/24 | 三级，subb → suba → main → 外网 |
| `subc` | `main` | 10.6.6.0/24 | 与 suba 平级，各自独立 NAT |

**不需要 `parent` 字段**——`uplink` 已经表达了全部关系。也不限制形状：subb 挂 suba 还是挂 main，只改一行。

`zones` 在数据库里是有记录的一等对象（有 brain 名、桥、CIDR），因为 `uplink: main` 要求 main 已经存在。所以：

- 一次 `apply` 可以写多个 zone，平台按 `uplink` 拓扑排序建
- 也可以分开 apply：先 `main.yaml`，再 `suba.yaml`（引用已存在的 main）
- 引用不存在的 zone → 报错"先 apply main"，**不会**偷偷帮你建一个

### 5.4 多层带来的真实代价

先把话说准，避免误导：**NAT 本身不改 MTU**。1500 的包过 NAT 还是 1500，只是改地址。真正吃 MTU 的是上联是 PPPoE（1492）或你在链路上又加了 VPN 隧道（1420 起）。所以：

| 层数 | 真实影响 | 处理 |
|---|---|---|
| 2 层 | 端口映射要配两段（外网 → main-gw:2222 → suba-gw:22）；conntrack 表项 ≈ 翻倍 | 平台按 `mirrors`/`pub` 自动逐层生成映射链 |
| 3 层 | 排障要逐跳看（`mtr` 每一跳都是一台网关）；tcpdump 要抓三个点 | `labyard trace <src> <dst>` 逐跳执行 ping/tcpdump |
| 任意 | 若上联 PPPoE 或叠加 VPN，才需要降 MTU | 在 zone 上显式 `mtu: 1400`，平台写进 `net0` 配置 |

`labyard doctor` 会检查：每层网关的 `ip_forward=1`、nftables 的 egress 规则存在、上联口 MTU 一致。这三项任一缺失，表现都是"能 ping 通小包、大包黑洞"，最难查。

### 5.5 地址分配

| 规则 | 值 |
|---|---|
| 网关内网地址 | 固定 `inner.cidr` 第 1 个可用地址（`.1`） |
| 网关上联地址 | 从 uplink zone 的 CIDR 分配，从 `.10` 起 |
| 其他主机 | 从 `.10` 起顺序分配（`.2`–`.9` 留给基础设施） |
| 保留 | `.0` 网络号、`.255` 广播（/24）、`.1`（网关） |
| 唯一性 | 同一 zone 内按最后一段查重（`DuplicateIp`：两台机都占 `.20` 直接报错）；跨 zone 靠网段不同，天然隔离 |
| CIDR 重叠 | **不检测**。两个 zone 写同一个 `10.77.0.0/24` 不会被拦——桥名不同（`lab` + zone 名），但两边路由会互相打架。这是留给人看的：网段规划错了，症状是"能 ping 通但连错机器"，比直接报错难查，所以规划时别偷懒 |
| `mir{zone}` / `wan` | 不参与分配（前者无 IP，后者由 DHCP 或你指定） |
| 持久化 | **不持久化**。地址由 YAML 决定（写了 `ip:` 就用，没写就从 `.10` 起顺序分配），"只增不重排"是文件的性质而不是表的性质——你不动 YAML，地址就不会变 |

vmid 同理但要落库：`vmid_base` 之上取最小空闲，写进 `vmids` 表，下次重跑优先复用。**不用 PVE 的自动分配**——它会在你删掉 1001 后把 1001 给别人，快照名、镜像口、你的笔记就全对不上了。

### 5.6 流量监控接线

先把监控机的归属定清楚：**它是普通主机，`role: monitor`，可以属于任何 zone，也可以直接挂外网桥**（`zone.monitor.mgmt: wan`）。它不是独立于拓扑之外的观测节点——拓扑图上它就是一台主机，只是多了几块抓包网卡。

- **每个被监控网络一块抓包桥**：`mir{zone}`，纯二层、无 IP、不挂网关，**不属于任何内外网**——它是一条独立的观测旁路，只有监控机的抓包网卡插在上面，与业务网络没有任何三层往来，也不会干扰业务流量。
- 监控机上**每个被抓网络对应一块抓包网卡**（eth1、eth2……，`ip=manual` 无地址），管理口（eth0）正常接网络、拿地址、能 SSH/装包。监控机可自定义 `ip`（内网填主机位；挂外网填完整地址，留空 = DHCP）与 `vmid`，不写就自动分配。
- 抓的是**每台主机的接口**，不是网段：同一网络里多台源主机，每台一条 `labmirror@` 实例，各自把源主机 veth 的双向流量 `mirred` 到监控机在该网络抓包桥上的宿主侧口。拓扑图上因此一条主机一条虚线，谁被抓一眼可查。**副作用：同网段内的互访会在两条链路上各被复制一份（源主机出向 + 网关入向），抓包里同一会话出现两份是 SPAN 语义的常态，分析工具（如 Wireshark 去重）按五元组去重即可；换来的是网关自身发起/终结的流量不漏。**
- **监控机自己的内网口也在镜像范围里**：内网口 → 抓包口**没有回环**——采集流量走 `mir{zone}` 桥进 eth1，不会再出现在 eth0 上，所以 eth0 上的流量就是普通的本机流量，抓了不重复、不放大。图上不画这条自环线（语义在抓包口标注与抽屉清单里）。普通主机抓自己仍然在解析阶段拒绝。
- **后台巡检**（`api/reconcile.rs`）：每 5 分钟把"全部合法规格算出的期望镜像实例"与宿主上 `/etc/labyard/mirror/*.conf` 对账——缺失或 conf 内容变了就只重跑那些 Mirror 步骤，孤儿单元（规格已删、实例还在跑）停掉并清理。容器没建过、两端有 retire 标记的连线不补；任何一份规格解析失败则整轮跳过（编辑到一半的 YAML 不是"删光镜像"的指令）。巡检与 run 共用 `run_gate`，正在 apply 时本轮跳过。

`mirrors`（含 `zone.monitor` 展开出来的）每一条，对应宿主机上一条 `labmirror@<instance>` 实例：

```
SRC_VMID=102                 # 源主机：逐个枚举它的 veth，双向镜像
MIRR_BRIDGE=mirlab           # 落点桥：监控机在这张桥上的宿主侧口
TARGET_VMID=100              # 监控机
```

自动化四步：

1. 执行时按 `mir{zone}` 桥现探监控机的宿主侧 veth（不写死 `veth<vmid>i*`——监控机重建后 vmid 会变，探测自愈）
2. 写 `/etc/labyard/mirror/<instance>.conf`
3. `systemctl enable --now labmirror@<instance>`
4. 源主机的 veth 比容器晚出现是常态：脚本里带等待重试，挂不上规则就重试几次而不是静默退出

两条推论：

- **多源 → 同一台监控机不冲突**。每个源主机一条实例，落点指向监控机在同一抓包桥上的同一个宿主侧口，tc 的 mirred 允许多条 filter 指向同一目标口，互不干扰。
- **监控机重建会静默打断镜像**。vmid 变了，veth 名跟着变，旧 conf 里的落点指向一个不存在的口，`labmirror` 规则挂不上但**不报错**。所以每次 `apply` 都按当前桥成员重算并 rewrite，然后 `restart`；脚本内容参与步骤哈希，升级 labmirror 也会触发重装。

### 5.7 数据库表

只有六张表，而且**没有一张存"期望状态"**：

```sql
schema_version(version)                                   -- 迁移游标，只前进

step_state(key PK, input_hash, state, message, updated_at) -- 幂等判定的全部依据
vmids(vmid PK, zone, host, UK(zone, host))                 -- vmid 分配，重跑不重排
runs(id PK, status, spec_name, started_at, finished_at)
run_steps(run_id, step_key, state, message, started_at, finished_at,
          PRIMARY KEY (run_id, step_key))
retired(zone, host, note, retired_at, PRIMARY KEY (zone, host))  -- host='*' 表示整个 zone
```

三条设计说明：

- **期望状态不进库。** 早期设计有一整套 `zones / networks / hosts / ports / ip_allocations /
  mirrors` 表，把 YAML 展开后存一份。做下来发现它只是 YAML 的**冗余投影**：每次 apply
  都要重新解析 YAML，那份投影没有一次被当成真相用过，反而制造了"库和文件不一致时信谁"
  的问题。现在要查"10.77.0.20 是谁"，答案在 YAML 里，`/api/hosts` 会把它和
  `pct list` 对账后合并成一张表——数据量在几十台这个量级，扫一遍比维护一份副本便宜。
- **`vmids` 必须持久化**：它是"重跑不重排"的唯一保证。YAML 里写了 `vmid` 就用写的，
  没写就查这张表，查不到才按 `vmid_base` 往上找空号。删了这张表 = 下次 apply 可能把
  容器编号重排（旧容器变孤儿）。这是唯一"看起来能推导、实际不能"的东西。
- **`retired` 是运行态，不是配置**：它记录"这一轮做完先收起来"，必须跨 apply 存活——
  否则一次 apply 就把它复活了（§8.6.3）。`host='*'` 表示整个 zone 被废弃。

`step_state.input_hash` 是引擎的心脏：`probe → decide → apply` 之后如果哈希没变，
下一轮直接 skip。哈希的输入是**步骤的规格参数 + `RULESET_VERSION`**（§8.6.6），
不是"跑没跑过"这样一个布尔值——这正是"更新"和"重复执行"能同时成立的原因。

### 5.8 IPv6（默认开，内网只发 ULA + NAT66）

默认开。理由不是"v6 普及率高"，而是更硬的一条：**默认关 = 这条代码路径永远没人测**。等到某天有人打开 `ipv6: true`，面对的是从未跑过的 nft 规则和从未配过的 `ip -6 route`，第一次必然炸。默认开，双栈从第一天就在每条回归测试里。

但"默认开"不等于"随便开"，三条硬约束：

| 约束 | 做法 | 为什么 |
|---|---|---|
| **内网只给 ULA，不给 GUA** | 全局 `ula_prefix: fdxx:xxxx:xxxx::/48`，安装时随机生成并存 DB | 如果直接把上游 GUA 下发给内网机器，它们能凭 v6 地址绕过网关直连外网，NAT 日志全失效。"内网不联网"这条保证会当场破功。注意约束的对象是**内网主机**：网关自己在 eth1 上收一个上游 GUA 作 NAT66 源地址是必需且安全的（见下"上联口被动接受上游 RA"） |
| **v6 出网必须过网关** | 网关 `masquerade`（NAT66），内网 default route 指向网关 `::1` | 和 v4 同一套可控性：能审计、能一刀切断、能按 zone 关 |
| **`target` 角色不配 v6 default route** | 和 v4 一致：`gateway: false` 的主机在 v4/v6 上都不写网关 | 隔离是结构性的，不能靠"记得别配" |

两种状态的实际差异：

| | `ipv6.enabled: true`（默认） | `ipv6.enabled: false`（每 zone 可关） |
|---|---|---|
| 网关 sysctl | `net.ipv6.conf.all.forwarding=1` | `forwarding=0` |
| 网关 nftables | forward 链放行 `$LAB6` + `oifname $WAN ip6 saddr $LAB6 masquerade` | forward 链对 v6 **显式 `drop`**，只放 established/related |
| 主机地址 | `{prefix}:{zone_idx}::10/64`，gw `::1` | 不配地址 |
| 主机 sysctl | 默认 | 可选 `net.ipv6.conf.all.disable_ipv6=1` |

**`enabled: false` 必须显式 drop，不能只是"不配地址"。** 不配的话，主机一条 link-local、或者自己 `ip -6 addr add` 一个 ULA，就能绕开 v4 防火墙直连——这是实验室里最常见的"我明明封了但它通了"。`inet` 表同时管 v4 和 v6，漏掉一边等于没封。

#### 5.8.1 `nat` 开关（每个 zone 一个）

`nat: true`（默认）时网关对本站内网做地址转换，v4 与 v6 一起开：`ip saddr $LAB4 oifname $WAN masquerade` + `ip6 saddr $LAB6 oifname $WAN masquerade`。

`nat: false` 只做转发、不做转换——用于上游已经把整个网段路由过来的情形（宿主加了静态路由，或 v6 拿到的是真正可分发的前缀）。这时内网地址在外部可见，内网机器可以直接从外部被访问，这是"要抓外部回连"的实验必需的。

**默认值必须是 `true`。** 第一版实现里这个字段被解析、被展示在预览里，却从来没传到执行器——`gateway_script` 的 `nat44` 参数是硬编码的 `true`。也就是说 `nat: false` 是一个纯装饰字段，写了没反应。现在的做法是：`ResolvedZone.nat` → `StepKind::Net { nat }` → `gateway_script(..., nat)`，三步都带值，并且有回归测试盯着"`nat: false` 时脚本里不能出现任何 `masquerade`"。

同时改掉的默认值：`nat` 原先 serde 默认 `false`，与"内网主机要靠网关出网装包"直接矛盾（不写 `nat` 的实验网会全部断网）。现在默认 `true`。


地址规划：

- `ula_prefix` 安装时随机生成（`fd` + 40 bit 随机），不用固定的 `fd00::/8` 子段，避免两个 lab 撞车
- zone 序号 `i` → `{prefix}:{i}::/64`；`0` 段留给抓包桥
- 网关 `::1`，主机从 `::10` 起，与 v4 分配规则一一对应
- 抓包桥 `mir{zone}` 不配 v6（镜像流量是二层复制，与地址无关）
- **内网不做 SLAAC、不发 RA**：内网地址是静态 ULA，确定、可复现，与 v4 模型一致；宿主桥不发 RA（发了会把整个二层域带偏）
- **上联口被动接受上游 RA**：网关 eth1 设 `accept_ra=2`——`forwarding=1` 之后内核**默认忽略 RA**，漏了这一条的症状是"内网 ULA 全通、NAT66 规则也在，但 ping6 公网 100% 丢包"，很容易误判成"宿主没有 IPv6 上游"（本项目就这么误判过一版 README）。拿到的 GUA 只用于 NAT66 的源地址，**不下发给内网主机**，所以"只给 ULA、不给 GUA"没有破
- 上联口已经有全局地址时不碰接口：每次 apply 都 `down/up` 会无谓断网

两个验收陷阱：

1. `ping` 通不代表 v6 通。要显式 `curl -6`、`ping -6`，并 `dig AAAA` 确认外部域名解析结果
2. 上联是双栈时，容器内 `apt` 可能优先走 v6。若 NAT66 没配好，表现是"apt 卡住 30 秒后回退 v4"。`doctor` 里加一项 `curl -6 -m 5` 直连测试，超时直接报错，不要留给用户猜

---

### 5.9 外来成员：你手动建的机器怎么进来

平台只认**规格里声明过**的主机。你在 PVE 网页上手动建的 VM 要接进 zone，
做法是在 `hosts` 里加一行并写上它的 `vmid`（或让平台按名字找到它）：

```yaml
zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    hosts:
      gateway: { role: gateway }
      probe: { role: monitor }
      my-win10: { role: server, vmid: 205 }   # 你手动建的，声明 vmid 就接管
```

声明之后它和平台建的主机**没有区别**：地址、路由、镜像都一样覆盖。
唯一的区别是平台不会重建它——`vmid` 已存在时 `provision` / `create` 步骤会跳过，
只做"接入"（写网络、写路由），不碰磁盘。

> **实现的是"发现 + 手动纳管"，不是自动纳入。** 早期设计里的 `observed`（桥上看见
> 但没声明）/ `unknown`（连 vmid 都推不出来）两级扫描没有实现，也没有封禁列表。
> 现在的做法更简单：`/api/hosts` 除了规格内的主机，还返回 `unmanaged`——宿主上规格
> 不认识、也没绑 vmid 的 LXC/QEMU（PVE 面板里手工建的那种）。点「纳管」把它写进
> 规格并绑 vmid（`POST /api/hosts/adopt`），此后与平台建的主机没有区别。
>
> 为什么不自动纳入：**声明式系统的价值在于磁盘上的文件就是全部真相**，自动把外来
> 容器写进规格，会让"这条记录是谁加的"变成没有答案的问题。所以纳管是显式动作，
> 且**只登记、不立刻下发**——下次 `apply` 才按规格配网络，不会在纳管一台跑着业务
> 的容器时顺手把它网断掉。

镜像为什么天然覆盖声明的成员：镜像抓的是**桥上成员口的 ingress**，按 vmid 定位口
（`veth<vmid>i<idx>` / `tap<vmid>i<idx>`），不看角色。所以只要 `vmid` 对，
一台手动建的 VM 声明进来就立刻被抓到，不需要额外配置。

一个已知限制：**QEMU 开了 PVE 防火墙**时，桥上的成员口会变成
`fwln<vmid>i<idx>` + `fwpr<vmid>i<idx>` 一对，`tap` 口本身不在桥上，
按 `tap` 前缀找不到 → 镜像挂不上。这种情况请在该 zone 里关掉 PVE 防火墙。

## 6. 资源与角色的执行细节

### 6.1 每种资源的 probe / apply

| 资源 | probe | apply | 幂等判据 |
|---|---|---|---|
| bridge | `pvesh get /nodes/{n}/network` 找 iface | 写 `/etc/network/interfaces` → `ifreload -a` | iface 存在 + `bridge_ports` 对 + 无 IP |
| 模板 | `pvesh get /nodes/{n}/storage/{s}/content` 找 `vztmpl` | `pveam download`（备选 `pvesh create /nodes/{n}/aplinfo`） | 文件存在且大小 > 0 |
| LXC | `pct config {vmid}` | `pct create {vmid}`（存在则 `pct set`） | cores/mem/rootfs/net*/features 全等 |
| 容器网络 | `pct exec -- ip -j addr show` | `pct set --net{n} ...` 改 net*，或容器内 `ip addr` | 地址与 gw 精确匹配 |
| 软件 | `pct exec -- cat /var/lib/labyard/provisioned.json` | `pct push` + `pct exec install.sh` | `input_hash` 相等 |
| 镜像 | `systemctl is-active labmirror@X` + `tc filter show` | 写 conf + `enable --now` | 实例 active 且 filter 数 = 成员口数 |
| 快照 | `pct listsnapshot {vmid}` | `pct snapshot {vmid} <name>` | 名字存在 |

### 6.2 角色分工：平台只管"接入"，不管"业务"

> 平台主要负责网络环境以及基础网络设施，如需 VM 和其他具体服务就自行部署，责任分开。

按这条边界，provisioner 只分两级：

| 角色 | 平台做的 | 平台**不**做的 |
|---|---|---|
| gateway | 全套：依赖、转发、nftables、`labgw`/`labvpn`、恢复 unit | 不建桥、不挂业务主机 |
| monitor | 抓包基础件（`tcpdump`，`tshark` 可选）、抓包目录、`mir{zone}` 接口 | 不装 ELK / Suricata / Zeek，不写规则、不管分析 |
| server / client / target | hostname、静态 IP / gw / DNS、SSH 公钥或密码、apt 源、时区 | **不装任何业务软件**（nginx、docker、nmap 都不管） |

除 gateway 外，其余角色共用一份 `bootstrap.sh`（约 120 行纯 shell，随平台分发）：

```bash
pct push <vmid> bootstrap.sh /tmp/labyard/bootstrap.sh
pct exec <vmid> -- sh /tmp/labyard/bootstrap.sh \
  --hostname main-server1 --ip 10.3.3.10/24 --gw 10.3.3.1 --dns 223.5.5.5 \
  --apt tuna --access key-password --json
```

在平台眼里 `server1` 和 `client1` 没有本质区别，都是"接入某张 inner、配好地址的主机"——这正是你说的"结构相似"。

要装额外东西，两条路，都不进平台核心：

1. **`provision.hook`**：zone 或 host 上写一个本地脚本路径，平台负责 push、执行、记哈希。脚本是你的，平台不解析内容
2. 建好之后自己 SSH 进去装

`--json` 是必须的小改造（每个脚本约 +15 行）：现有输出是给人看的彩色文本，`{"ok":true,"applied":[...],"warnings":[...]}` 才好解析。解析文本会在某次改文案时静默失败。

### 6.3 PVE 通道：命令行直连，不用 API 客户端

labyard 装在 PVE 宿主本机、以 root 运行（§3.1），所以**不需要 HTTP API 客户端、不需要 API Token、不需要 TLS 配置**。`pct` / `qm` / `pvesh` 是 PVE 官方 CLI，其中 `pvesh` 就是 `/api2/json` 的命令行前端——`--output-format json` 给出的结构与 API 完全一致。

分工只有一条规则：**读走 JSON，写走原生命令。**

| 用途 | 命令 | 说明 |
|---|---|---|
| 全量 VM/CT 状态 | `pvesh get /cluster/resources --type vm --output-format json` | 一次拿全部，替代 N 次 `pct config`（§7.6.3） |
| 单机配置详情 | `pct config <vmid>` | `key: value` 文本，按需 + 缓存 10 秒 |
| 建 / 改 / 启停 | `pct create` / `pct set` / `pct start` | **同步阻塞，不需要轮询 UPID** |
| 节点 / 存储 / 模板 | `pvesh get /nodes`、`/nodes/{n}/storage`、`/nodes/{n}/aplinfo` | 与同名 `GET` 端点同结构 |
| 版本 | `pvesh get /version --output-format json` | 低于 9.0 拒绝启动（§6.7） |
| 桥 | 写 `/etc/network/interfaces` + `ifreload -a` | API 的 `network` 端点最后也是写这个文件 |

**为什么不用 proxmoxer / HTTP**：

1. **省掉整套凭证机制**：不用建 `labyard@pve` 用户、不用建 token、不用配 ACL 权限集、不用 `verify_tls=false`。本地 root 直接过权限检查
2. **省掉 task 轮询**：API 建机返回 UPID 要自己轮询到结束；`pct create` 本来就是同步的
3. **少两个常驻依赖**（proxmoxer、httpx），少一条"token 过期 / ACL 漏授"的失败模式
4. 走 `127.0.0.1:8006` 要经过 pveproxy（另一个 Perl 进程）+ TLS，比本地直连更重

**代价必须写清**：

- 每条命令 fork/exec 一个 Perl 进程，约 100–200 ms。一天几次 apply、轮询 ≥10 s，可忽略；但**禁止**在循环里对每台机调一次
- CLI 输出不是正式兼容契约。所以只用 `--output-format json`（`pct config` 除外），**绝不解析表格文本**；`doctor` 校验关键字段存在
- 只适用单节点。多节点要 ssh 到目标节点（§6.4 的 `node.transport: ssh`），或那时再引入 API 客户端

配置因此没有凭据段：

```toml
[server]
bind = "0.0.0.0"
port = 8443
tls = "self-signed"             # self-signed | off | caddy

[pve]
node = ""                       # 留空 = 自动探测（单节点）
```

### 6.4 容器内执行通道

容器内执行没有"走 PVE API"这个选项——REST API 没有 exec 端点。只有 subprocess：

| 通道 | 用在哪 | 说明 |
|---|---|---|
| `pct exec`（本机 subprocess） | 平台与 PVE 同机 | 默认，无网络依赖，最快 |
| `pct exec` over SSH | 平台在另一台机 | `node.transport: ssh`，用系统 `ssh`（`exec.Command`），不引 SSH 库 |
| PVE 的 `lxc-attach` | 兜底 | `pct exec` 不可用时 |

`pct exec` 是同步阻塞的，用 `exec.CommandContext` 起子进程：`ctx` 取消即 `SIGTERM`→（`WaitDelay` 超时后）`SIGKILL`，stdout/stderr 各一个 `bufio.Scanner` 逐行推 SSE。

有一条必须写死：**在 `Command::pre_exec` 里调 `setpgid(0, 0)`，取消时用 `libc::kill(-pid, SIGKILL)` 杀整个进程组**。只杀父进程会留下孤儿 `apt` 进程占着 dpkg 锁，下次 apply 直接卡在 "dpkg frontend lock"。这是本项目最容易写错、且只在真机上才暴露的一处。Rust 的版本约 20 行（`pre_exec` 是 `unsafe`，且只能调 async-signal-safe 的函数；`Child` 的所有权与 `kill_on_drop` 语义要理清），比 Go 的 12 行长，但比"杀不干净"好。

### 6.5 tun 直通（现有坑，平台要自动处理）

无特权 LXC 用 WireGuard/OpenVPN 必须：

```
lxc.mount.entry: /dev/net/tun dev/net/tun none bind,create=file
lxc.cgroup2.devices.allow: c 10:200 rwm
```

写进 `/etc/pve/lxc/<vmid>.conf`，然后 **Stop → Start**（`reboot` 不重新读 config）。

平台在创建 gateway 时就写好这两行（比 GUI 的 dev0 直通更可控），并在 `labyard doctor` 里检查宿主的 `/dev/net/tun` 是否存在。dev0 直通也能用，但会被 `vzdump` 排除，这是副作用。

### 6.6 环境探测：哪些差异真会影响，哪些不会

"模板在创建时选"这件事定了基调：**平台不硬编码任何模板名**。`doctor` 调 `pvesh get /nodes/{n}/aplinfo --output-format json` 拿本节点可用的模板列表，直接喂给页面下拉框。所以 amd64 / arm64 对平台代码是透明的——容器里都是 Debian，`apt` 源两种架构都覆盖。

真正有差异的是这四项，`doctor` 一次全部探完，存成 capability 矩阵（下表写成 `pvesh get <路径>`，与同名 `GET` 端点返回同一份 JSON）：

| 探测项 | 命令 | 影响 | 不支持时 |
|---|---|---|---|
| PVE 版本 | `pvesh get /version` | 基线 9.x；`network` 端点字段、`tags`、LXC 的 `ssh-public-keys`、`features` 语法、厚 LVM 快照能力 | **低于 9.0 直接拒绝启动**（见 §6.7）；9.x 内的小版本差异走特性探测 |
| 节点列表 | `pvesh get /nodes` | 桥是 per-node 资源，zone 必须绑一个节点 | 单节点自动填入，多节点要求 YAML 写 `node` |
| 存储能力 | `pvesh get /nodes/{n}/storage` | `local-lvm`(thin)/`zfs`/`dir` 支持快照；厚置备 `lvm` 不支持 | 建 zone 前报错，不等到 `destroy --keep-snapshots` 才发现 |
| 桥清单 | `pvesh get /nodes/{n}/network` | 已存在同名桥则复用；有 IP 的桥要警告（可能变成第二个网关） | — |

另外两项不是 CLI 能问的，`doctor` 直接读本机：

- `/dev/net/tun` 存在？没有则 VPN 不可用，提前说
- `pct` / `ip` / `nft` 在 PATH？缺则平台根本跑不起来

**关于架构的诚实说明**：arm64 的唯一实际差异是"某些包在 arm64 上没有"（`wireguard-tools`、`tcpdump`、`tshark` 都有，但极小众的没有）。这不影响设计，只影响 provisioner 装包失败时的报错质量——所以 `bootstrap.sh` 要把 `apt-get install` 的失败原因原样回传，而不是吞掉。

### 6.7 PVE 9.x 基线：哪些变化真的碰到我们

基线定在 **PVE 9.x（Debian 13 Trixie / LXC 6.0.4 / kernel 6.14 / AppArmor 4）**。9.0 的发布说明里有十几条 breaking change，逐条对过，真正影响 labyard 的是下面五条：

| PVE 9 变化 | 对 labyard 的影响 | 处理 |
|---|---|---|
| 基于 Debian 13 Trixie | 模板名变成 `debian-13-standard_13.x_*.tar.zst`；容器内 `apt` 源要 `trixie` | 模板从 `pveam available` 实时拉，不硬编码；apt 源按容器内 `/etc/os-release` 判断代号 |
| **创建特权容器需要 `Sys.Modify`** | 我们**只用非特权容器**，不受影响 | 这是"不要为 tun 改成特权"的又一条理由——升级后还得额外开权限，纯属自找麻烦 |
| 厚置备 LVM 支持快照（技术预览） | 本来"厚 LVM 不支持快照"的判断在 9.x 上不成立了 | `doctor` 查 PVE 版本 + storage 的 `snapshot-as-volume-chain` 属性；是 tech preview 就打黄标，不当作稳定能力 |
| 网络接口名可能变化（新增 `nic` 前缀的 pinning） | 容器里 `eth0` 可能不是"第一块口" | 一律以 PVE 返回的 `net0`/`net1` 配置为准；容器内不按名字猜，按 `ip -j` 的 `ifindex` 排序 |
| cgroup v1 彻底移除 | 旧写法 `lxc.cgroup.devices.allow` 不再生效 | 我们用 `lxc.cgroup2.devices.allow`，本来就是对的 |

另外两条只影响使用习惯、不影响代码，但 README 要写：

- **AppArmor 4**：嵌套容器（LXC 里跑 Docker）有已知问题（bug #6538）。你在容器里装 Docker 自己负责，但报错会很怪，先知道这条
- **PVE 9 的 `pve-test` 仓库改名**（原 `pve-test` 沿用）：装依赖时别引用旧源名

CLI 侧没有变化：`pct`/`qm`/`pvesh` 的参数在 9.x 内稳定。**但"命令能跑"不等于"JSON 结构没变"**——`pvesh --output-format json` 序列化的就是 API 处理器返回的那份结构，字段跟着 API 走。所以 `doctor` 第一件事是 `pvesh get /version`，第二件是拿真实的 `/cluster/resources`、`/nodes/{n}/storage`、`/nodes/{n}/aplinfo` 各调一次，字段缺失就当场报，而不是等到 `apply` 跑到一半 `KeyError`。

不支持 8.x 的取舍：**8.x 上直接拒绝启动**，报"labyard 需要 PVE 9.0+"。写兼容层意味着每个端点都要两套参数、每个测试要跑两遍，而 8.x 会在 2026 年内逐步退场（8.4 支持到 2026 年底）。真要在 8.x 上跑，用旧目录那套手工脚本。

### 6.8 与 PVE 交互的两个硬事实

**一、不是需要凭据，而是需要 root + 宿主本机。**

本地 root 调 `pct`/`pvesh` **不过 ACL**，权限是全有或全无。这是明确接受的代价：平台本来就要写 `/etc/pve/lxc/*.conf`、改 `/etc/network/interfaces`、调 `systemctl`，这几件事任何 ACL 都给不了。审计仍然有——`/var/log/pve/tasks` 记录每次 CLI 调用的任务。

**为什么不用"最小权限 API Token"**：它看着更安全，实际是反的。token 落在 `/etc/labyard/config.toml`（0600），root 可读；一旦 labyard 进程被拿（它有 Web 界面），token 是**可移植**的凭据——能拿到别的机器上直接打 8006，而"本机 root"给不出这个能力。所以引入 token 只增加复杂度（凭证轮换、ACL 漏授诊断、`verify_tls=false`），换不来安全收益。真想收紧，正确做法是收紧 §8.4 的暴露面，不是给本地进程发令牌。

反过来说，**这也是"必须装在 PVE 宿主本机"的第二个理由**（第一个是 §6.8 二的直接写文件）：装到别的机器上，就必须 ssh 或 API，两层失败模式全回来了。

**二、`lxc.mount.entry` 只能直接写文件，API 不认。**

PVE 的 `PUT /nodes/{n}/lxc/{id}/config` 只接受它认识的那些 key（`net0`、`rootfs`、`features`…），`lxc.mount.entry` 和 `lxc.cgroup2.devices.allow` 不在其中。要加 tun 直通，只能直接写 `/etc/pve/lxc/<id>.conf`：

- 节点本地就是普通文件写入（`/etc/pve` 是 pmxcfs，但写文件是正常操作）
- 写之前重新读一遍文件，只追加缺的行，**不整文件覆盖**——pmxcfs 有并发写保护，整文件覆盖可能吞掉 PVE 刚写入的变更
- 写完必须 **Stop → Start**，`reboot` 不重读 config（这是旧实现踩过的坑，见 §1）

这也是为什么"平台必须装在 PVE 宿主机本机"：它要直接写这个文件。装到别的机器上就得走 SSH，多一层失败点且没有必要。

---

## 7. 技术选型

原则：**每一格都要能说出"用的是哪个成熟库"，说不出来就是自研，打回。**

### 7.1 后端

| 关注点 | 选择 | 版本 | 理由 | 否决与原因 |
|---|---|---|---|---|
| 语言 | **Rust** | 1.100 / edition 2021 | 用户选定（Python → Go → Rust，决策记录见 §7.5.1）。**实测常驻 12.0 MB / 峰值 44.9 MB**（峰值几乎全是 `scrypt` 的 32 MB 工作区）；单二进制 12.7 MB（含前端）；无 GC，长期内存曲线平坦 | Go（峰值 18.3 MB、9.9 MB 二进制、编译 1–2 s，但 GC 让 RSS 停在峰值水位）、Python（初版方案，对比数据保留） |
| 命令入口 | **clap 4** | 4.5 | **只有 4 条命令**（`serve`/`doctor`/`apply`/`destroy`），不做操作型 CLI；derive 宏生成 help 与补全 | 手写 `std::env::args` 匹配（3 条命令够用，但补全要自己写） |
| HTTP + 路由 | **axum 0.8** | 0.8 | `Router` 原生支持 `/runs/{id}` 路径参数与 extractor，与 tokio 同一 runtime。**OpenAPI：手写数据表 `API_DOCS`**（36 条路径）——不引 utoipa，而用一个契约测试 `every_api_route_is_documented` 反向扫源码里的 `.route(...)` 与它双向比对，漏一条就测试失败。手写不会漂移，前提是有一把会响的锁 | actix-web（actor 模型自成一套）、上 utoipa 再手写一遍（当前规模不划算） |
| PVE 通道 | **原生 CLI**：`pct` / `qm` / `pvesh` | PVE 自带 | 本地 root 直连，无凭证、无 TLS、无 task 轮询（§6.3） | 任何 API 客户端（`proxmoxer` / `go-proxmox` 等）——要建 token + ACL，换不来安全，见 §6.8 一 |
| 校验 | **serde** + 手写语义检查 | — | serde 反序列化即完成类型校验（缺字段/类型错直接拒绝，`deny_unknown_fields` 拦住拼错的字段）；跨字段规则（IP 在网段内、vmid 不重复）在 `resolve()` 里手写并单测 | `validator` crate（derive 表达不了"IP 属于网段"这类规则） |
| DB | **rusqlite**（`bundled`） | 0.32 | 6 张表、查询几乎只有"按 id 取一行"；同步 API 用 `spawn_blocking` 包一层即可，不必上 async SQL 的复杂度。`bundled` 编 C 源码，交叉编译靠 `cross` | sqlx（编译期要连库、依赖面大）、Diesel（ORM 黑盒）、纯 Rust 嵌入式 KV（丢掉 SQL 与事务） |
| 配置 | **serde_yaml_ng** | 0.10 | YAML 是 IaC 惯例；`serde_yaml` 已于 2024 归档，`serde_yaml_ng` 是活跃的继任 fork（API 兼容、已修补原始缺陷） | `config` crate（为读 5 个字段引一整套）、`saphyr`（更底层，要自己搭反序列化层） |
| 任务 | 自建薄 runner（tokio task + `CancellationToken` + `runs`/`steps` 表） | — | 无 broker、可断点续跑、状态可查；**`CancellationToken` 树把取消传到子进程** | 需要 Redis 的任务队列、定时器库（不是执行器） |
| 锁 | **fs2** | 0.4 | 跨进程 `flock`，异常路径由库处理 | 手写 `libc::flock`（要处理 EINTR 与异常路径） |
| 拓扑排序 | 手写 **Kahn**（约 30 行） | — | 步骤依赖是小图，40 年前的标准算法；不引数值库 | gonum/graph（为 40 行逻辑引一个大库） |
| 日志流 | **SSE**（`axum::response::sse`） | 内置 | 单向、浏览器自带重连；实现是 `broadcast` → `BroadcastStream`，落后时发注释行不断流。**没有做 `Last-Event-ID` 续传**（断开后靠 `/steps` 补齐） | WebSocket（双向不需要，还要处理心跳） |
| 认证 | **`scrypt`**（RustCrypto）+ `hmac`/`sha2` 签名 cookie | 0.11 | scrypt 是合格 KDF；cookie 签名 40 行、无第三方 | JWT（前端存 token 是 XSS 放大器）、`cookie` crate 的 private jar（多一层抽象换 40 行） |
| TLS | **`openssl req`** 生成自签 + **`axum-server` 的 rustls** | — | openssl 本就在系统里；rustls 是纯 Rust 实现，不链接 OpenSSL，没有版本与 CVE 牵连 | 内嵌证书生成（要给二进制塞密钥生成逻辑）、`native-tls`（又回到 OpenSSL 依赖） |
| 结构化日志 | **tracing** + **tracing-subscriber** | 0.1 / 0.3 | JSON layer 输出结构化行；span 能自动带上 `run_id` 与 `step_key`，这正是 run 日志要的东西 | `log` + `env_logger`（没有 span，手写上下文） |
| ttyd 反代 | **axum 的 WebSocket**（`ws` feature） | 内置 | 下游用 `WebSocketUpgrade`，上游用 `tokio-tungstenite` 连，双向搬运。**已实现**：真机验证页面 200、WS 握手 `101 Switching Protocols`（见 §7.4） | 手写 ws 帧解析（没必要） |
| 测试 | `#[cfg(test)]` + 真机回归 | — | 纯函数（脚本生成、哈希、YAML 编辑、拓扑）在同模块里单测，不需要 PVE；需要 PVE 的一律上真机（`scripts/api-regress.py` 逐接口跑）。**没有**"录制回放假 `pct`"这一层：fixture 不保真、会腐烂 | cargo-nextest（当前 134 项、秒级跑完，不值得多一层工具）、proptest（脚本生成的输入空间不大） |

### 7.2 前端

| 关注点 | 选择 | 理由 | 否决 |
|---|---|---|---|
| 框架 | **React 18 + TypeScript + Vite** | 用户指定；Vite 启动秒级 | Next.js（不需要 SSR） |
| UI | **Ant Design 5** | 管理后台事实标准，Table/Form/Descriptions/Tag 全都现成 | MUI（偏 C 端视觉）、shadcn（要自己拼装） |
| 数据层 | **TanStack Query** | 缓存、重试、失效重取、乐观更新 | 手写 `useEffect` + fetch |
| 路由 | **不引路由** | 六个入口切一个 `page` 状态就够；引路由要配套深链、面包屑、返回栈，纯成本 | React Router（早期引过，一次都没 import，已从依赖里删掉） |
| 拓扑图 | **@xyflow/react (React Flow)** | 节点/连线/缩放/拖拽全有，正是"网络-主机"图要的 | d3 手绘（两天起步）、Cytoscape（偏图论） |
| 图表 | **不引图表库** | 现在没有需要画曲线的页面；概览页用 AntD 的 `Statistic` 就够 | Recharts / Chart.js（为一个不存在的需求引包） |
| 日志终端 | **ttyd 自带 xterm.js** | 终端由 ttyd 在容器里渲染，前端只在 iframe 里嵌；我们不再单独引 xterm.js | 自己引 xterm.js 接 WS（等于把 ttyd 重写一遍） |
| 类型 | 手写 TS 类型（`web/src/api.ts`） | 与后端 struct 一一对应，改动时两侧一起改；有 `tsc -b` 兜底 | 引入代码生成（36 个端点仍不值得，且有 schema 漂移风险） |
| 状态 | TanStack Query 足够，**不引 Redux/Zustand** | 服务端状态为主，本地状态极少 | Redux（模板代码多） |

### 7.3 打包与部署

| 项 | 选择 |
|---|---|
| 依赖管理 | **Cargo**（提交 `Cargo.lock`，构建加 `--locked`；`rust-toolchain.toml` 锁编译器版本，两者一起保证可复现） |
| 前端构建 | `vite build` 输出到 `web/dist/`，用 **`rust-embed`** 编进二进制（`#[derive(RustEmbed)]` + `#[folder = "web/dist"]`） |
| 交叉编译 | **`cross build --release --target x86_64-unknown-linux-gnu`**（Docker 内自带 C 工具链，绕开 rusqlite 的 C 编译）→ `strip` 后 **约 5–8 MB 单文件** |
| 宿主安装 | `installer/install.sh`：装 `nftables`/`conntrack`/`tcpdump` + 把二进制放到 `/usr/local/bin/labyard` + 装 systemd 单元 + `labyard doctor`。**不装 Python、不建 venv、没有 `pip`** |
| 运行 | systemd `labyard.service`，`Restart=on-failure`（可选 socket 模式见 §7.3.1） |
| 升级 | 换二进制 → 迁移 DB → `systemctl restart`；`labyard upgrade` 与 `install.sh --update` 做同一件事 |
| 回滚 | 旧二进制留一份 `/usr/local/bin/labyard.prev`，DB 迁移前 `VACUUM INTO` 备份（保留 7 份，见 §9 风险 10） |

**不用 Docker**：容器里要 `pct`、`ip`、`nft`、`/dev/net/tun`，还要写 `/etc/pve/`——特权容器跑这个等于没隔离，还多一层网络排障。

#### 7.3.1 可选：按需启动（socket activation），把常驻内存降到 0

宿主紧张（≤ 8 GB、低功耗盒子、或要和别的服务抢内存）时，"让进程更小"是次优解——**更优的是让它不在**。systemd 原生支持，不引任何新依赖：

```ini
# labyard.socket
[Socket]
ListenStream=8443
Accept=no                     # 必须；Accept=yes 会每个连接起一个实例

[Install]
WantedBy=sockets.target
```

应用侧只要两件事（约 40 行，`core` 之外的一层薄壳）：

1. 启动时若存在 `LISTEN_FDS`，直接用继承来的 fd，**不再 bind**（否则端口冲突）
2. idle 计时：**无活跃 HTTP 连接 + 无运行中的 run + 无打开的终端**，持续 15 分钟后自行退出（用 `sd_notify` 停再用 `exit 0`，配 `SuccessExitStatus=0`，否则 `Restart=on-failure` 会把它当崩溃拉起来）

| 维度 | `service`（默认） | `socket`（可选） |
|---|---|---|
| 常驻内存 | 28–35 MB | **0** |
| 首次访问 | 即时（启动 21–28 ms） | +50–150 ms（进程启动 + 读 DB），比解释器方案快得多 |
| apply 期间 | 正常 | 正常（有 run 就不退） |
| 额外成本 | 无 | 一个 socket 单元 + 约 40 行 idle 逻辑 |

**这条为什么值得单列**：它让"更省内存"在 99% 的时间尺度上**失去意义**——常驻 18 MB 对常驻 0 MB，后者更小，且与语言无关。内存差异于是退化成"只在 apply 那几分钟里存在"的东西。这也是"资源不富裕"该有的正确答案：不是换语言，是换启动方式。**Rust 的冷启动只有 24 ms，这让 socket 模式几乎完全无感——按需启动在这里没有代价。**

### 7.4 Web 终端与文件传输

需求是"点一台机器就能进它的 shell，顺手能传个文件"。落地方案：容器里跑 ttyd，
labyard 做一层带认证的反代，前端用 iframe 嵌在主机页的抽屉里。

| 层 | 选择 | 形态 | 关键理由 |
|---|---|---|---|
| 终端服务 | **ttyd** | 1.7.7 静态二进制（1.3 MB） | C 写的，libuv + libwebsockets，前端 xterm.js；**CJK/输入法完整支持**（GoTTY 这块不行）；自带 ZMODEM/trzsz；一个进程多会话 |
| 分发方式 | `pct push` 进容器 + systemd 单元 | 每个容器一份 | 宿主只留 `/usr/local/bin/ttyd` 这一个文件；容器里没有才推（1.3 MB，每次 bootstrap 都推是浪费） |
| 终端接入 | labyard 反代 | `api/mod.rs` 里的 HTTP + WebSocket 双向转发 | 见下，这是唯一自写的一段 |
| 终端内传文件 | **ZMODEM（lrzsz）** | 容器内 `apt install lrzsz`（约 300 KB） | ttyd 前端自带 zmodem 解码，容器侧只要标准的 `rz`/`sz`，不装任何自研客户端 |

ttyd 的实际启动命令（`Pve::ensure_terminal` 生成的单元）：

```bash
/usr/local/bin/ttyd -p 7681 -i <容器内网 IP> -b /terminal/<zone>/<host> \
     -W -t fontSize=14 /bin/bash -l
```

四个决定都在这一行里：

- **`-i <内网 IP>` 而不是 `0.0.0.0`**：网关的 eth1 接 WAN，监听全网卡等于把终端
  暴露给外部网段。绑内网地址后，能连到它的人本来就已经在 zone 里了。
- **`-b /terminal/<zone>/<host>`**：ttyd 自带的前缀，和 axum 的反代路径一致，
  于是容器里的 ttyd 不需要知道自己在哪个子路径后面。
- **`-W` 可写**：默认只读，"能看不能敲"的终端没有意义。
- **`/bin/bash -l`**：登录 shell，`/etc/profile` 里的 locale、PATH 才会生效——
  少了 `-l`，中文目录名和 `apt` 的 PATH 都会怪。

**认证在反代这一层**（校验 labyard 的 session cookie，未登录直接 401），
ttyd 自己不做认证，因为它只监听内网地址——外部伪造不了对它的访问。

反代路径：`/terminal/{z}/{h}` / `/terminal/{z}/{h}/` / `/terminal/{z}/{h}/{*path}`
三条路由（axum 的 `{*path}` 不匹配空段，裸路径要单独一条）。HTTP 与 WebSocket
走同一条：`Upgrade` 头在就切成双向字节搬运。

**两个真机撞出来的坑**（都写进了回归用例）：

1. **裸路径少一条路由**：`/terminal/{z}/{h}` 不带斜杠时匹配不到 `{*path}`，
   ttyd 收到的路径缺前缀，页面白屏。
2. **`Host` 头必须换成容器视角**：ttyd 按 `Host` 做 DNS-rebinding 校验，
   转发时保留浏览器发来的宿主 IP 会被它拒掉。反代要把 `Host` 改写成
   `容器内网IP:7681`。

**多窗口**：ttyd 默认每个 WebSocket 连接 fork 一个独立 pty（`-Q` 共享模式不开），
所以开五个标签页就是五个独立 shell，互不干扰。前端是 iframe 嵌在抽屉里
（`/terminal/{z}/{h}/`，"点哪台看哪台"的连续感比开五个标签页好）。

**文件传输只有一条路：ZMODEM。** 容器里 `rz`/`sz`，ttyd 前端解码。
早先设计过一个"文件面板"（`pct push`/`pct pull` + tar 流 + 目录穿越校验），
**没有实现**——ZMODEM 覆盖了"顺手传个脚本/下个报告"的主场景，而做一个文件管理器
要处理 Range、断点、并发、临时文件清理与路径穿越，属于另一个功能的工作量。
大文件仍然建议直接在宿主上 `pct push`，比走浏览器快一个量级。

---

### 7.5 资源占用预算

目标：空闲 **≤ 20 MB**、压测峰值 **≤ 40 MB**，且**不随运行时长增长**。

**实测**（lab3：Debian 13 / arm64。Python 3.13.5 / Go 1.27.4 / Rust 1.100。三份代码做**同一件事**：HTTP + JSON + SQLite 查询 + 请求校验；同机、同负载、同一份数据；读 `/proc/PID/status` 的 `VmRSS` 与 `VmHWM`，轮换跑两轮取稳定值。复现脚本见 `bench/`。**这张表是本次选型的依据，不删**）

| 指标 | Python（FastAPI+Pydantic） | Go（net/http+modernc/sqlite） | **Rust（axum+rusqlite）** |
|---|---|---|---|
| 启动到可服务 | 15–19 ms | 21–28 ms | **24 ms** |
| 空闲 RSS | 34–44 MB | 11.4 MB | **4.7 MB** |
| **压测峰值**（500 请求 / 50 并发） | 56.3–59.2 MB | 18.3–18.6 MB | **6.4–6.6 MB** |
| 线程数 | 24–33 | 9 | **8** |
| fd 峰值 | 54–68 | 26–29 | **15–16** |
| 产物体积 | venv ≈ 90 MB | 9.9 MB | **2.6 MB** |

**两个台阶，价差完全不同**：

- Python → Go：省 **41 MB**（-69%），交付形态从"venv 90 MB + 解释器版本"变成"一个 9.9 MB 文件"
- Go → Rust：再省 **11.7 MB**（-64%），交付形态再减 7.3 MB

> 吞吐不作为比较依据：同一份代码两轮差 7 倍（767 ms vs 5546 ms），说明容器内 CPU 调度是主要变量，这个测量环境测不出真实吞吐差异。**内存两轮一致，可信；耗时不稳，不引用。**

**平台实测（容器内复现，HTTP 模式无 TLS）**：空闲 RSS **6.9 MB**、二进制 **8.0 MB**（含 1.07 MB 前端）、峰值 HWM **≈38 MB**（多次实测 37.9–38.1，波动来自分配器与 ASLR）。

> 这一组是**容器内 HTTP 基线**。生产形态（宿主上 HTTPS + 现在的拓扑页/规格页前端）实测是：
> 常驻 **12.0 MB**、二进制 **12.7 MB**（前端从 1.07 MB 涨到 1.46 MB，加上 rustls）、峰值 HWM **44.9 MB**。
> 两者差 5 MB，差在 TLS 会话缓存与更大的前端资源常驻。下面表格里的推理两者都成立。

**38 MB 这个数字必须解释清楚，否则会被误读成"Rust 也不小"。** 诊断结果：`VmHWM` 在**启动完成时就已经是 38.1 MB**，之后 200 个请求（20 并发，混合 preview 与 state）一个字节都没顶上去。来源是 `scrypt`——`N=2¹⁵, r=8` 的工作内存是 `128·N·r = 32 MB`，且是瞬时的（`scripts/measure.sh` 可复现）：

| 时刻 | RSS | HWM | 说明 |
|---|---|---|---|
| 启动后空闲 | 6.9 MB | 37.9–38.1 MB | 启动时生成初始密码，跑了一次 scrypt |
| 200 请求 / 20 并发 | 7.5 MB | 不变 | **HWM 纹丝不动**，只涨 0.7 MB RSS |
| 8 个并发登录 | 7.7 MB | 39.0–39.2 MB | 只涨 **1.1 MB**——`hash_gate` 生效的直接证据 |

最后一行是这次新增的关键测量。**8 个并发登录只涨 1.1 MB，说明 32 MB 哈希区是被复用而不是复制的**；若串行化没生效，8 × 32 MB 会把峰值推到 256 MB 以上，`MemoryMax` 再大也扛不住。这条不是推断，是量出来的。

所以正确的描述是：**常驻 7 MB，峰值 38 MB，差额 31 MB 是一次性的哈希工作区**。这和 Go 的"峰值由 GC 水位决定"是两回事——它是可预测的、固定的、且只在登录那一瞬间存在。另一个附带结论：glibc 把这块 32 MB 用 `mmap` 直取直还，`free` 后 RSS 立刻回落（压测后 7.5 MB，不是 38 MB），所以 `memory.current` 也不会长期挂在 38 MB。

**这个发现改了三件事**（都是实测推翻了估算）：

1. `MemoryMax` 不能按"常驻 × 2"取，也不能只按 labyard 自己的峰值取——**账里还有子进程**。
   `pct` / `pvesh` 是 perl 进程，`/usr/bin/time -v` 实测：

   ```
   pvesh get /version    Maximum resident set size: 176 336 kB
   pct list              Maximum resident set size: 151 532 kB
   pct exec 101 -- true  Maximum resident set size: 150 644 kB
   ```

   它们是 labyard 的子进程，**同属一个 cgroup**，所以 `MemoryMax` 管得住它们，
   也必须为它们留额度。这条搞错过两次：

   - 取 64 MB 时：单是一次 `pvesh` 就能把 cgroup 顶爆，内核疯狂 reclaim + 换页，
     scrypt 的 32 MB 工作区分配不到，`verify_password` 挂掉返回 false。表现是
     **"密码没记错但一直提示密码错误"，重启就好**（重启后第一个请求之前，
     cgroup 还没被页缓存占满）。当时猜过"swap thrash"，真正的原因是账本算错了。
   - 取 192 MB 时：把 `doctor` 里四个 `pvesh` 改成并发取数（本来是想把 6.7 秒压下去），
     四个 perl 进程一起 charged，`Failed with result 'oom-kill'`，15 秒内重启一轮。
     所以 `doctor` 现在是**串行**的，快的部分靠缓存和"storages 只问一次"补。

   现在取 **384 MB**：`3 × 70`（apply 批内并发 3）+ `12`（常驻）+ `32`（scrypt）+ 余量。
2. **密码哈希必须串行**。两个并发登录 = 64 MB 工作区，正好顶穿限制。已实现 `hash_gate`（容量 1）+ `spawn_blocking`，避免卡住 runtime worker。
3. **preview 也要限流**。虽然实测显示它只涨 0.7 MB，但那是 6 台机的规格；规格放大到 50 台机时，解析 + 规划的对象量会线性增长。闸门固定 4。

**Rust 的增量分解**（估算，标注清楚，不冒充测量）：tokio runtime + 调度器 ≈2 MB、axum/hyper ≈2 MB、rustls ≈4–6 MB（含会话缓存与会话票据）、rusqlite + page cache ≈4 MB、serde/serde_json ≈1 MB、SSE 与日志缓冲 ≈4 MB。合计 18–22 MB，与上面的估算区间吻合。**估算只用于排序，最终以真机 `systemctl show labyard -p MemoryCurrent` 为准。**

四条硬性纪律（前三条与语言无关，第四条是 Rust 特有）：

1. **不引 ORM**。6 张表、单用户、查询几乎只有"按 id 取一行"，ORM 省下的那点代码不值得一层黑盒。迁移是编号 SQL 文件 + `schema_version` 表（约 60 行 runner），用 `include_str!` 编进二进制顺序执行。代价说清楚：没有 autogenerate、没有 downgrade；收益是每条 DDL 都看得见、可 review。
2. **禁止无界缓冲**。`Vec<u8>` 收整个 `pct exec` 输出、把镜像读进内存、`read_to_end` 读整个请求体——都和语言无关，都会炸。规则是子进程输出逐行转 SSE 且不落内存；`pct push` 用文件路径而不是把内容读进内存。
3. **日志队列有上限**。SSE 推 `pct exec` 输出用有界 channel（`mpsc::channel(500)` 或 `broadcast` 带容量），满了丢**中间**行并插一条 `[已省略 N 行]`。这是唯一会让 RSS 随时间上涨的地方，也是"跑三天之后变卡"的典型成因。
4. **没有 GC，但有分配器**。Rust 不回收垃圾，所以不存在 Go 那种"堆涨到峰值水位不回落"。系统 malloc（glibc ptmalloc2）在多线程下确有 arena 碎片隐患，所以做了对照实测（§7.5.2）：**结论是不要换**——glibc 的 idle 4.4 MB / churn 后 5.5 MB 全面优于 jemalloc（6.2 / 10.8）与 mimalloc（10.4 / 27.5）。真正管用的是"禁止无界缓冲"，它比换分配器重要一个量级。

#### 7.5.1 决策记录：为什么最终是 Rust

这一节是为了**以后不再重开这个话题**。三个候选都实测过，结论如下：

| 候选 | 结论 | 一句话理由 |
|---|---|---|
| **Rust** | ✅ **采用** | 峰值 6.5 MB、单二进制 2.6 MB、无 GC 抖动、内存曲线平坦 |
| Go | ❌ 否 | 峰值 18.3 MB、单二进制 9.9 MB、GC 让 RSS 停在由峰值决定的水位 |
| Python | ❌ 初版方案，已换 | 胶水代码最省，但交付形态最差（venv 90 MB + 解释器耦合） |

**决策过程要如实记录，因为它三轮翻转**：初版是 Python（理由：Pydantic 白送校验、自动 OpenAPI）；后来因为交付形态与内存改成 Go；最后改成 Rust。前两次翻转的理由都是"省资源 + 改善交付形态"，Rust 在这两项上都比 Go 更彻底——**11.7 MB 和 7.3 MB 的差距，在倍数上是 2.8 倍和 3.8 倍**。当"资源占用越小越好"被当作首要目标时，Rust 是这条轴上的终点，Go 只是中间站。

**这个选择换来了什么**（都是真的）：

- 峰值内存 **6.5 MB vs 18.3 MB**，差距 2.8 倍
- 二进制 **2.6 MB vs 9.9 MB**，差距 3.8 倍
- **无 GC**：没有"RSS 停在峰值水位"这个现象，长期常驻的曲线是平的；也没有 GC 停顿带来的尾延迟抖动
- 启动 24 ms，socket activation 模式下冷启动几乎无感

**代价也如实写**（不粉饰，这是排期依据）：

| 项 | 影响 |
|---|---|
| 三层覆盖（defaults/zone/host）+ 默认值 | serde 没有 Pydantic 那套"声明式默认值 + 合并"，要手写 merge，约 +3～4 天 |
| 自动 OpenAPI | 当前手写 20 行 `openapi.json`；端点增长后再引入 utoipa（每个 handler 加注解，约 +1 天） |
| 异步子进程管理 | `tokio::process` + `pre_exec` 设进程组 + 取消传播，是最容易写错的一块，约 +2 天 |
| 交叉编译 | rusqlite 要过 C 编译器，必须用 `cross`（Docker 内工具链），比 Go 的一行命令重，约 +0.5 天 |
| 编译等待 | 全量首次 60–90 s，增量 10–15 s（Go 是 1–2 s）。摩擦落在"改一行试一次"的真机调试循环上 |

**净结论**：多花约 **1.5–1.8 倍工期**（≈ +1.5～2 周，相对 Go），换来 2.8 倍的峰值内存差距和 3.8 倍的产物体积差距，以及一条平坦的长期内存曲线。这个交换在"资源不富裕的宿主 + 长期常驻"这个前提下成立——**前提变了结论就要重算，但前提是用户定的，不是估算定的**。

**协程/线程数不决定内存**（顺带写死一个直觉误区）：

- Linux 线程栈默认 8 MB 是**虚拟**地址空间，按页惰性分配，实占每线程 8–16 KB。Rust 跑 8 线程峰值 6.5 MB——线程本身几乎不占。
- 省内存的不是"少开线程"，是"别攒数据"：8 路并发上传各自的分块缓冲、8 条 SSE 的队列，是数据不是线程，跟你开几个 worker 无关。
- 同理，"异步所以能 1 线程"是假命题：SQLite 写、模板 sha256、tar 解包都是阻塞调用，放在单 reactor 上会卡住 SSE 心跳，真实下限是 3–4 个线程（1 reactor + spawn_blocking 池）。

#### 7.5.2 内存分配器：实测结论

Rust 默认使用系统 malloc（Linux 上是 glibc 的 ptmalloc2）。它在**多线程 + 长期运行**下有两个已知问题：每个线程一个 arena，arena 之间不能复用空闲块；大小分布很散的对象释放后，空闲块留在各自的 arena 里，`malloc_trim` 之外不会还给 OS。表现就是 **RSS 只涨不跌**。

这正是"常驻三年"最需要避免的形态，所以必须实测，不能靠"Rust 没有 GC 所以没问题"推断。

测试方法：同一份 axum + rusqlite 代码，只换 `#[global_allocator]`，三种构建各跑一遍。负载分三段——500 请求 / 50 并发（稳态）、**300 轮 × 1000 个对象**的分配释放（碎片压力，分布按真实比例：90% 64–512 B / 9% 1–4 KB / 1% 64–128 KB）、每轮后静置 3 秒。**连跑三轮 churn**，看 RSS 是收敛还是持续上涨。指标取 `VmRSS` 与 `VmHWM`。复现：`bench/alloc/run-all.sh`。

**实测**（Debian 13 / aarch64 / 7 核 / glibc 2.41 / rustc 1.100.0）：

| 分配器 | idle | 峰值 HWM | churn ① | churn ② | churn ③ | 二进制约 |
|---|---|---|---|---|---|---|
| **glibc ptmalloc2（默认）** | **4.4 MB** | **5.0 MB** | **5.4 MB** | **5.4 MB** | **5.5 MB** | **3.6 MB** |
| jemalloc（tikv 0.6） | 6.2 MB | 8.2 MB | 10.6 MB | 10.6 MB | 10.8 MB | 4.0 MB |
| mimalloc（0.1） | 10.4 MB | 16.4 MB | 26.4 MB | 28.1 MB | 27.5 MB | 3.7 MB |

**结论：不换，用默认的 glibc。** 三项全赢，而且赢得不小。

**为什么和"jemalloc 碎片控制更好"的常识相反。** 那句话没错，但它成立的前提是**内存量级足够大**——Redis、ScyllaDB 那种几百 MB 到几百 GB、几十个线程持续混合分配的进程。jemalloc 的代价是**固定开销**：默认 `narenas = 4 × CPU`（这台 4×7 = 28 个 arena），每个 arena 有 chunk 元数据与预留；mimalloc 每个线程一个 heap 与 segment 池。对一个只用到 5 MB 的服务，**这些固定开销比它省下的碎片还多**——mimalloc 的 idle 就是 glibc 的 2.4 倍，还没开始干活。

**更重要的发现：真实分配分布比分配器影响大一个量级。** 第一轮测试用的是均匀分布（64 B ~ 128 KB 等概率），glibc 的 churn 后 RSS 冲到 13.6 MB；换成真实分布（90% 是小对象）后，同样 300 轮只有 5.5 MB。**差了 8 MB，比三款分配器的全部差异都大。** 所以真正的纪律是 §7.5 第 2 条"禁止无界缓冲"，不是换分配器。

**"RSS 只涨不跌"的担心在这个负载下不成立。** 三者都收敛：glibc 的 churn②与 churn①完全相同（5.4 MB），churn③只多 100 KB。jemalloc 也持平（10.6 → 10.6 → 10.8），mimalloc 有明显波动（26.4 → 28.1 → 27.5），说明它的分配模式还没稳定。**收敛比低水位更重要，而 glibc 两项都做到了。**

**这条实测有一个明确的局限，写清楚**：它跑的是分钟级，不能证明"三年后"的碎片行为。但有三条间接证据支持这个结论——① 两次 churn 完全持平，说明没有单调增长的趋势；② glibc 对超过 `M_MMAP_THRESHOLD` 的块走 `mmap`，`free` 时直接 `madvise(MADV_DONTNEED)` 还给内核，不留碎片；③ 本项目的负载里不存在"长期存活、大小不断变化的对象池"，那才是 glibc 真正会输的场景。

**如果哪天要换**（比如常驻集合涨到几百 MB，或多节点 agent 铺开），判断依据不是"jemalloc 更有名"，而是重跑 `bench/alloc/` 看 churn③是否还平。**换分配器是一行 `#[global_allocator]`，但要不要换必须由数字决定。**

#### 7.5.3 实现期查出来的真实缺陷（记录，不删）

这些都**不是设计问题，是接线问题**——字段写了、解析了、展示了，但没接到执行路径上。它们有一个共同特征：**单测全绿、预览正常、真机必炸**。之所以能被查出来，是因为把编译器的 dead-code 警告当成了必读项，而不是噪音。

| 缺陷 | 表现 | 根因 | 现在 |
|---|---|---|---|
| **退出码没人看** | `pct create` 因存储满失败，run 报"全部完成"，容器不存在 | `Pve::cmd` 原样返回 `Output`，注释写"调用方自己判断"，但**没有任何调用方判断** | `cmd` 在非零/被信号杀时直接返回 `RunError::NonZero`；这是命令层唯一能保证"真的成功了"的位置 |
| **容器建完是停着的** | 下一步 `pct exec` 全部失败 | `pct create` 不带 `--start` | 带 `--start 1`；已存在但停着的容器补一次 `pct start` |
| **网关只有一块网卡** | 网关脚本 `ip link set eth1 up` 直接失败 | `ensure_ct` 从不建 `net1`，而脚本通篇配 `eth1` | `Container` 步骤带 `uplink_bridge`（仅网关），建机时加 `--net1`；老容器用 `pct set --net1` 补 |
| **`nat` 字段是装饰品** | 设 `nat: false` 仍然全程 NAT | 参数没传到 `gateway_script`，`nat44` 硬编码 `true` | 见 §5.8.1，默认值同时改成 `true` |
| **`password` 字段是装饰品** | 设了密码，容器里登不进去 | `ensure_ct(..., None, ...)` 写死 | 进 `Container` 步骤并透传 |
| **`dns` 字段是装饰品** | 写了内网 DNS，容器里解析的还是公共 DNS | `host_script` 把 `resolv.conf` 写成了常量 | `resolv_conf()` 用 spec 的列表，空才退回公共 DNS |
| **bootstrap 环境变量名对不上** | v6 前缀、zone 名永远传不进容器 | plan 传 `ipv6_prefix`，脚本读 `LABYARD_V6_PREFIX`；`zone` 压根没被读 | 键名对齐，并加契约测试：从步骤参数反推变量名，断言脚本源码里真的有 |
| **同角色主机撞 IP** | 一个 zone 里两台 `server` 都是 `.20`，后建的覆盖前面的 | 直接用角色的默认主机位，没有占用表 | 按角色默认位往后找第一个空位；显式重复直接报 `DuplicateIp` |
| **`specs` 目录建了从没用** | 刷新页面 YAML 全丢 | 前端只存在 `useState` 里 | `GET/PUT /api/spec`，原子写，校验后才落盘 |

教训只有一条：**"字段存在"和"字段生效"是两件不同的事**，而类型系统不区分它们。所以 `deny_unknown_fields` 拦住的只是拼写错误，拦不住"写了但没人读"。

### 7.6 资源峰值与限流

§7.5 说的是"常驻别浪费"，真正会出事故的是**峰值**。这一节只谈峰值。

#### 7.6.1 峰值场景清单

| 场景 | 内存 | CPU | 磁盘 IO | 真实后果 |
|---|---|---|---|---|
| 一次 apply 8 台机（并行 provision） | 中 | **高**（容器内 apt × 8） | **极高**（8 路 dpkg 写同一块盘） | 宿主 IO 打满，PVE 管理面和 SSH 一起卡 |
| 模板下载 + 8 个 `pct create` | 低 | 高（zstd 解压 × 8） | 极高（读同一份模板 8 次） | 创建排队，`vzdump` 被拖慢 |
| 3 个 run 同时 SSE 推日志 | **高**（若缓冲无上限） | 中 | 中（日志落盘） | RSS 持续上涨 → OOM |
| 上传 2 GB 镜像到容器 | **极高**（若不用流式） | 低 | 高 | OOM + `/var` 撑满 |
| 拓扑页高频刷新 × 20 台 | 低 | 低 | **中**（pmxcfs 串行读） | pmxcfs 变慢，全节点受影响 |
| 10 个 Web 终端同时开 | 低 | 低 | 低 | fd 与 ttyd 进程数 |

#### 7.6.2 唯一能打挂宿主的是磁盘 IO

三个维度不对称，必须区别对待：

- **内存不够** → 容器起不来，报错明确，退出重来即可
- **CPU 不够** → 全部变慢，但系统仍响应
- **磁盘 IO 打满** → **宿主 SSH、PVE 管理界面、别的 VM 一起卡死**，而且这时候你登不进去救

所以限流的重心在 IO，不在 CPU。来源只有两个：多台同时 `apt install` 写盘、多个 `pct create` 从同一模板做 zstd 解压。

实测（8 台机同批 provision，并发 1/2/3/4）：

| 并发 | 总耗时 | 磁盘写峰值 |
|---|---|---|
| 1 | 200 s | 30 MB/s |
| 2 | 105 s | 60 MB/s |
| 3 | 70 s | 120 MB/s |
| 4 | 95 s | 240 MB/s |

**并发 4 之后耗时反而回升，磁盘峰值仍线性翻倍**。这就是重步骤默认并发 3 的依据，不是拍脑袋。

#### 7.6.3 pmxcfs：一个容易被忽略的单点

`/etc/pve` 是 **pmxcfs（单线程 FUSE）**，所有读写串行处理，且要同步到集群日志。这意味着：

- **读**：一律用 `pvesh get /cluster/resources --type vm --output-format json` 一次拿全部 CT 的状态（CPU、内存、磁盘、uptime），**不经过 pmxcfs 逐个读**。配置详情（`net0` 等）按需拉 + 缓存 10 秒。**禁止**"N 台机调 N 次 `pct config`"这种写法
- **写**：同一台机的所有变更**合并成一次 `pct set`**。写次数 = 机器数，不是参数数。分五次 `pct set` 会让 pmxcfs 日志涨五倍，还可能触发五次配置重载

这条比"用哪个语言"重要：一个 N+1 的 probe 循环，换任何语言都会把 pmxcfs 拖慢。

#### 7.6.4 并发闸（实际实现的三道）

| 闸门 | 默认 | 实现位置 | 为什么需要 |
|---|---|---|---|
| apply 里重步骤的批内并发 | 3（`--concurrency`） | `Executor::execute(.., conc)` | `pct create` 会解压模板写盘；并发 3 是"能明显变快"与"不把 IO 打满"之间的折中 |
| 计划/预览的并发 | 4 | `AppState::preview_gate` | 每次出计划都要读集群现状，不限会一起压 pmxcfs |
| scrypt 计算 | 1 | `AppState::hash_gate` | 一次登录哈希约 100 ms 满载一个核；并发几次就能把宿主 CPU 顶住，串行化最省事 |

**没做的**：早期设想是"全局并发 run 2 / 轻步骤池 8 / 重步骤池 3"三池分离。实际实现里
一个 run 内的步骤是**顺序**走的，重步骤只在批内受 `concurrency` 限制，轻步骤没有单独
的池子——它们本身几毫秒，分池要引入两套队列、两套优先级和一类新的死锁，
换不来能测出来的收益。"跨 zone 同时 apply"目前靠人：实验环境里同时点两个 apply
本来就是误操作。

#### 7.6.5 限流措施：哪些做了，哪些没做

设计时列了八条。实现完之后逐条对账，**没做的照实写在这里**，免得被当成已有保障：

| # | 措施 | 状态 |
|---|---|---|
| 1 | SSE 背压：事件队列容量 500，满了丢**中间**行并插一条"已省略 N 行" | **已实现**。`mpsc::channel(500)` 满了由 runner 汇总成 `skipped` 事件；`broadcast` 落后时发一条 `lagged` 注释行，不断流。丢中间不丢尾部——报错通常在尾部 |
| 2 | 单 step 日志落盘上限 8 MB | **不适用**。日志不落盘：实时走 SSE，落库只有每步一行 `message`（尾部若干行）。容器里的完整输出要去 `journalctl -u labyard` 或容器自身日志看 |
| 3 | 子进程套 `ionice -c2 -n7 nice -n 10` | **未实现**。宿主的 IO 压力来自 `pct create`/`apt` 写盘，靠"批内并发 3"和"同模板串行"控制；真要限，`systemd` 的 `IOWeight=50`（installer 已写）比 `ionice` 更彻底 |
| 4 | 上传流式 + 单文件 ≤ 2 GB | **不适用**。没有上传接口：规格是文本 YAML（JSON body），文件传输走 ttyd 的 ZMODEM，不经过本服务 |
| 5 | 临时文件 TTL 1 小时；磁盘 < 10 GB 拒绝上传 | **部分实现**。临时文件只有 `pct pull` 的中转目录与生成脚本，任务结束即删，没有定时清理器；"磁盘不足拒绝上传"不适用（没有上传） |
| 6 | 终端并发 ≤ 8，超出 429 | **未实现**。`/terminal/*` 是纯转发，没有连接计数；ttyd 本身按会话起进程，超了也是宿主先撑不住 |
| 7 | 拓扑轮询 ≥ 10 秒 + 页面隐藏时暂停 | **部分实现**。轮询间隔：拓扑 15 s、运行历史 10 s、概览 10 s、主机 8 s、网关 6 s；`visibilitychange` 暂停**没做** |
| 8 | 日志 + DB > 5 GB 自动清理 | **不适用**。没有日志文件；DB 只有 `step_state` / `vmids` / `runs` / `run_steps` / `retired` 五张小表，每步一行，增长以 KB 计 |

唯一"不依赖代码正确性"的兜底是 `systemd` 的资源上限（`MemoryMax=384M` / `MemorySwapMax=128M` /
`CPUQuota=50%` / `IOWeight=50` / `TasksMax=256`），由 `installer/install.sh` 写进 unit，见 §7.6.6。

#### 7.6.6 用 systemd 兜底（唯一不依赖代码正确性的措施）

```ini
[Service]
MemoryMax=384M
MemorySwapMax=128M
CPUQuota=50%
IOWeight=50
TasksMax=256
```

代码可能写错（比如某天忘了给某个 channel 设容量）。有了这几行，内核会先杀 labyard，而不是拖死宿主——**保宿主优先于保服务**，这是一个管理平台应有的默认值。Rust 没有 GC 可以调，所以只有 `MemoryMax` 这一层。

**384 MB 这个值是调出来的，而且调过三次，每次都是因为算漏了子进程。**

| 取值 | 结果 | 原因 |
|---|---|---|
| 64 MB | 跑几小时后登录稳定报"密码错误"，重启恢复 | 单个 `pvesh`/`pct` 子进程 charged 60–80 MB，顶穿上限后 cgroup 极端回收，scrypt 的 32 MB 工作区分配失败 |
| 192 MB | `doctor` 改成四个 `pvesh` 并发后直接 `oom-kill` | 4 × 70 MB 超出额度 |
| **384 MB** | 串行取数 + 读缓存，`MemoryPeak` 实测 152 MB | 3 × 70（apply 并发 3）+ 12 + 32 + 余量 |

配 `MemorySwapMax=128M` 兜极端情况，但不给热工作区留"整块被换出去"的空间。
详细推理在 §7.5 第 1 条。

#### 7.6.7 apply 前的资源预检

```
需要内存 = Σ(本次新建或扩容机器的 memory) × 1.2
需要磁盘 = Σ(disk) × 1.3
条件： 宿主 MemAvailable ≥ 需要内存
      目标存储 avail ≥ 需要磁盘
      load1 < 核数 × 0.7
```

不满足时**拒绝并给出差额**（"还差 1.8 GB 内存、42 GB 磁盘"），而不是跑到第 5 台才失败。半成品状态比拒绝更贵——它要你手工收拾。

#### 7.6.8 峰值预算

| 维度 | 常规 | 峰值 | 硬上限 |
|---|---|---|---|
| labyard RSS | 12 MB（VmRSS） | ~45 MB（`VmHWM`，登录瞬间的 scrypt 工作区）；**外加一个 `pct`/`pvesh` 子进程 ~176 MB** | 384 MB（`MemoryMax`，含子进程与页缓存口径） |
| labyard CPU | < 1% | ~5%（日志序列化密集时） | 50%（`CPUQuota`） |
| 磁盘写 | 可忽略 | 全在子进程 | `ionice` 降级 |
| 磁盘空间 | ~200 MB | 日志累积 | 5 GB 触发清理 |
| 文件描述符 | ~20 | ~150 | `TasksMax=256` |

**关键结论**：labyard 自身的峰值极低，因为重活全在子进程和 PVE 侧。所以"后端会不会卡"这个问题在峰值维度上更不重要——**峰值 CPU 从来不是 labyard 贡献的**，是它启动的那些 `pct exec` 里的 `apt` 贡献的。

#### 7.6.9 空闲功耗：影响 C-state 的不是内存

7×24 常驻时"越省电越好"是对的，但省电的杠杆不在进程大小上：

- x86 的空闲功耗由 **package C-state** 决定。所有核进 C6/C7 时整包可降到 1–3 W。**一个睡在 `epoll_wait` 里的进程不阻止这件事**，它占多少 MB 都不影响
- 唯一把 CPU 拽出深睡眠的是**定时器唤醒**与中断。所以真正的变量是"每秒醒几次"，不是"占多少 MB"
- 量化：Python → Rust 的 50 MB 常驻内存差异对应 DRAM 约 0.015 W；CPU 从 1% 降到 0.3% 单核在 N100 这类芯片上约 0.04 W。合计 **< 0.1 W，一年不到 ¥0.3**。这个数字记录在这里是因为它**从来不是选型理由**——选 Rust 的理由是绝对值与曲线形态，不是电费

同一台机器上的对照：

| 项 | 功耗 | 一年电费（¥0.6/kWh） |
|---|---|---|
| 一块常转的 2.5" HDD | 3–5 W | ¥16–26 |
| 一个 120 mm 风扇 | 1–2 W | ¥5–10 |
| 一个开着但不用的 512 MB 容器 | ~0.15 W | ¥0.8 |
| **Python 与 Rust 的差异（约 50 MB）** | **< 0.1 W** | **< ¥0.3** |

所以功耗这件事该写成设计约束，而不是选语言：

1. **所有定时器 ≥ 1 s 且尽量合并**。禁止 `<1s` 循环、禁止忙等轮询
2. **空闲时零唤醒**：不用 `sleep(0.1)` 这类自旋，SSE 靠事件驱动而不是定时拉
3. **页面隐藏即停轮询**（§7.6.5 第 7 条）—— 这是最大的一笔，能砍掉约 90% 唤醒
4. **socket 模式（§7.3.1）**：进程不存在时连定时器都没有，是唯一能做到"零唤醒"的形态
5. **别打断磁盘**：日志与 DB 放 SSD；WAL 模式下 `PRAGMA synchronous=NORMAL`，避免每秒 fsync 把盘反复唤醒

反直觉的一条：**换语言不自动更省电**。tokio 空闲时也就是一次 `epoll_wait`，与 Go 的 netpoller、Python 的 selector 循环没有本质差别；真正的差别只在"每秒醒几次"。**省电取决于设计，不取决于语言。**

---

## 8. Web 面板设计

### 8.1 页面结构

前端**没有路由表**（也就没有 `react-router`）：顶栏六个入口切换一个 `page` 状态，
刷新永远回到默认页。单机运维面板没有"分享某个页面链接"的需求，为它引入路由
（以及随之而来的深链、面包屑、返回栈）是纯成本。

| 入口 | 组件 | 做什么 |
|---|---|---|
| **编排** | `Editor.tsx` + `SpecVisual.tsx` | 默认**可视化**：默认配置 + 每个 zone 一张卡（上联/网段/NAT/出网 + 主机表格），增删 zone 与主机都是点出来的；`YAML` 是同一张卡上可切换的高级选项。两种模式都走「预览计划 → 应用」 |
| **规格** | `Specs.tsx` | 多份规格的文件级 CRUD（新建 / 复制 / 改 / 删），以及文件内 zone 片段的增删改；片段编辑走 CST 就地改写，保留注释与缩进 |
| **拓扑** | `TopologyPage.tsx` | 全局拓扑图（React Flow + dagre），**可操作**：点节点开抽屉改运行态与采集配置（监控机抽屉逐网络开关）、网络右键开关整网监控、悬停监控机高亮它的采集链路。懒加载 |
| **主机** | `Hosts.tsx` | 主机表（角色、IP、规格归属、电源、retired）+ 单机操作（开/关/重启、快照、retire/revive、终端） |
| **运行历史** | `Runs.tsx` | run 列表 + 步骤明细 + SSE 实时日志 + retry / cancel |
| **概览** | `Dashboard.tsx` | PVE 健康、`doctor` 结果统计、规格与主机计数 |

**全局拓扑图是主视图**，不是每个 zone 一张。因为 zone 之间的关系（`suba` 挂在 `main-inner` 上）只有放到一张图里才看得出来；单看一个 zone，它和别人的连接就丢了。`/api/topology` 已经把"谁挂谁""哪条镜像线"解析成 nodes + edges，前端只负责画。

**拓扑的模型（v2，改过一次）**。第一版把 `zone` 当作节点、把 `mirror` 当作一个假节点
`mirr`，边有四种。这一版后来被推翻了——
不对"：人看图问的是**"这台机接在哪个网段上"**，而图回答的是"这个对象属于哪个 zone"。
zone 不是网络，它是一个配置作用域。现在只有两类节点：

| 节点 | 是什么 | 图上显示 |
|---|---|---|
| `net:wan` / `net:{zone}` | 外网桥、各 zone 的内网桥 | 紫虚线框；网段、桥名、NAT/出网/映射数 |
| `host:{zone}:{host}` | 容器 | 网关绿底、监控机蓝底、主机白底；IP、配置、CT 号、运行状态点 |

边只有两条语义，**方向都是数据流方向**：

- `attach`：谁插在谁上面。网关因此有两条——`net_up → gw`（上联口 eth0）与
  `gw → net_zone`（内网口 eth1）。正是这一对边让 dagre 顺着排出了
  `外网 → 网关A → 内网A → 网关B → 内网B` 这条链，不需要额外指定 rank。
- `mirror`：`源主机 → 监控机` 的虚线单向箭头（一条主机一条线）
  **聚合成一条**，label 里列出被抓的主机名——否则三条虚线完全重合，看起来只有一条。



**终端内嵌在主机页的抽屉里**（`<iframe src="/terminal/{z}/{h}/">`），不是新标签页。
选择内嵌的理由是"点哪台看哪台"的连续感——真开一堆标签页反而找不回来。
需要独立窗口时 iframe 里的 ttyd 自带"新窗口打开"。**文件传输不做独立页面**：
ttyd 里的 `lrzsz`（`sz`/`rz` + ZMODEM）就是文件通道，`/terminal/{z}/{h}/{*path}`
反代已经把 ZMODEM 控制序列一并通过。原因见 §7.4。

> 中间有一版是"只能直接写 YAML"，理由是怕表单和 YAML 长出两套语义。
> 那个担心是对的，但结论错了：真正的问题是**表单要自己解析 YAML**。
> 现在的做法是让后端提供 `YAML → JSON 投影`（`/api/specs/{name}/model`），
> 解析规则仍然只有一份，表单只是在改一棵 JSON 树，保存时再变回 YAML 并走
> **同一条校验**（`Spec::parse` + `resolve_all`）。于是"图形化能存、应用却失败"
> 这种事不会发生。
>
> 代价必须说清楚：**可视化保存会重排 YAML 并丢掉注释**（YAML 库不保留注释）。
> 要留注释就切到 YAML 模式手工改，或者用「规格」页的 zone 片段编辑（那是 CST
> 就地改写，注释不动）。界面上直接写了这句话，而不是让人踩一次才知道。

### 8.2 关键交互

**编排页是"改结构 → 看计划 → 应用"三段式**，改结构有两种入口：

```
① 可视化（默认）              ①' YAML（高级，同一张卡右上角切换）
   默认配置 + zone 卡片           直接编辑原文
   主机表格点着改
② 预览计划：POST /api/preview，出一张步骤表（不落库、不执行）
③ 应用：POST /api/apply，起 run，SSE 里看每一步的实时输出
```

①和①' 写的是**同一个文件**，随时可以来回切：可视化保存后会自动把生成的 YAML
灌进 YAML 模式，切过去就能看。两条路都落在规格文件上，是"网页不绑架你"的保证
——页面上做出来的东西随时能存进 git 或贴进 issue。

规格页的 zone **片段**编辑走的是第三条路：CST 就地改写（`yaml-edit` / rowan），
**注释和缩进原样保留**，不会因为"在网页上改了一个 IP"就把整份文件重排。
三种入口的分工是刻意的：

| 入口 | 适合 | 注释 |
|---|---|---|
| 可视化卡片 | 从零搭、批量加主机 | 会丢（整份重写） |
| YAML 全文 | 精细调整、复制粘贴 | 会丢（整份重写） |
| zone 片段 | 只改一个 zone | **保留**（就地改写） |

**拓扑图上的操作**（都在图上，不跳页）：

| 操作 | 结果 |
|---|---|
| 点节点 | 右侧抽屉：主机可开机/关机/重启/终端/retire/revive；网络可看出网、加删端口映射；监控机抽屉里有**采集的网络**清单，逐网络开关采集 |
| 悬停监控机 | 它的采集链路高亮成实色、其余节点淡化，一眼看出"这台抓了哪几个网段"；悬停单条虚线只高亮那一条 |
| 悬停单条镜像线 | 只高亮这一条——比点击更轻，点击留给"开抽屉" |
| Shift + 拖动 | 框选多台主机 |
| 右键（网络节点） | 开/关整网流量监控（写规格 + 立刻落地抓包口）、开/关出网、复制网段 |
| 右键（单节点） | 打开详情、终端、开/关机、复制网段 |
| 拖动节点 | 只改画布位置，**不写回规格**；结构没变时轮询刷新不会重置位置 |

两个布局/锚定规则：**主机节点底部画出每个网口 chip**，所有边都锚到具体
网口上（attach 边接主机的 eth0/eth1，镜像线从源主机的内网口出发、接到监控
机对应网络的抓包口）——"线从哪块口出发"和"图上是哪块口"是同一个东西；
**布局只用 attach 边算层级**，镜像线不参与——否则监控机会被推到所有主机
后面一档，看起来像挂在别的机器下面，而它其实和同网主机平级。

镜像的"加"收敛到**网络级**（右键整网监控 / 监控机抽屉逐网络开关）：主机
级零散连线与"一个网络一个抓包口、自动覆盖全网主机"的总体设计冲突，已下
线。底部表格保留，作为总账，并显示后台巡检的状态。

**Plan 表的呈现**：

| 动作 | 对象 | 变化 | 影响 |
|---|---|---|---|
| 新建 | bridge `main-inner` | 10.3.3.0/24 | — |
| 新建 | CT 1001 `main-gateway` | 2c/1G/8G, net0=vmbr0, net1=main-inner | — |
| 新建 | CT 1002 `main-probe`（监控机） | 2c/1G/8G, net0=vmbr0, net1=mir{zone} | — |
| 新建 | 采集 `主机接口 → 监控机抓包口` | tc mirred（每源主机一条实例） | — |
| 新建 | CT 1003 `suba-gateway` | net0=main-inner(10.3.3.40), net1=suba-inner | 子单元，接上联 |
| 更新 | CT 1004 `main-server1` | memory 512→1024 | 需重启 |
| 跳过 | `provision:main-server1` | 哈希未变 | — |

**运行时操作（出网开关、端口映射、VPN、镜像）由后端直接 `pct exec` 进容器调 `labgw`/`labvpn`，不经过任何容器内 HTTP 服务**。理由：容器内面板要网络可达 + 独立认证 + 一套 CSRF 防护，而 `pct exec` 全都不需要；输出用 `--json` 解析，稳定。这也是"不移植旧 `labpanel`"的技术原因。

### 8.3 API 骨架

下面是**实际实现的全部路由**（与 `src/api/mod.rs` 的 `router()` 逐条对应，
`GET /api/openapi.json` 返回同一份清单；`src/api/mod.rs` 里有一个契约测试
`every_api_route_is_documented` 双向比对两边，漏写一条就编译测试失败）。

```
                      —— 认证与状态 ——
GET    /api/health                       PVE 连通、版本、能力矩阵（不需要登录）
POST   /api/auth/login                   登录，成功返回 Set-Cookie
POST   /api/auth/logout                  登出，清 cookie
GET    /api/auth/me                      当前用户
GET    /api/state                        已登记步骤的状态（幂等判定的输入）

                      —— 规格：文件级 ——
GET    /api/spec                         默认规格
PUT    /api/spec                         保存默认规格（先校验，再原子写）
GET    /api/specs                        列出全部规格文件
POST   /api/specs                        新建规格（可复制自另一份）
GET    /api/specs/{name}                 读指定规格
PUT    /api/specs/{name}                 保存指定规格
DELETE /api/specs/{name}                 删规格文件（不动 PVE 资源）

                      —— 规格：结构化视图（可视化编排）——
GET    /api/specs/{name}/model           YAML 的 JSON 投影；不存在 exists:false
PUT    /api/specs/{name}/model           按 JSON 保存：生成 YAML → 校验 → 原子写

                      —— 规格：zone 级（编辑框的种子）——
POST   /api/specs/{name}/zones           新增一个 zone
GET    /api/specs/{name}/zones/{z}       取 zone 片段；不存在也是 200，exists:false
PUT    /api/specs/{name}/zones/{z}       用片段替换整个 zone
DELETE /api/specs/{name}/zones/{z}       从规格里删掉 zone（不动 PVE 资源）

                      —— 计划与执行 ——
POST   /api/preview                      解析并返回计划（不执行；逐步骤标注 create/update/skip）
GET    /api/catalog                      资源目录：可用模板（按存储分组）与存储清单
POST   /api/apply                        执行，返回 run_id
GET    /api/runs                         run 历史
GET    /api/runs/{id}/steps              run 的步骤明细
GET    /api/runs/{id}/events             SSE 日志流
POST   /api/runs/{id}/retry              从失败的步骤续跑
POST   /api/runs/{id}/cancel             取消运行
GET    /api/doctor                       环境自检
GET    /api/topology                     全局图：zone / bridge / host / mirror 四类节点

                      —— 主机运维 ——
GET    /api/hosts                        清单（含电源状态、角色、规格归属、retired 标记）
POST   /api/hosts/{z}/{h}/power          {action: start|stop|reboot}
POST   /api/hosts/{z}/{h}/retire         废弃：停机并摘镜像，保留磁盘
POST   /api/hosts/{z}/{h}/revive         从废弃状态恢复
GET    /api/snapshots/{z}/{h}            列出快照
POST   /api/snapshots/{z}/{h}            {action: create|rollback|delete, name}
POST   /api/destroy                      销毁 zone（body 的 confirm 必须等于 zone 名）

                      —— 网关 ——
GET    /api/zones/{z}/gateway/status     策略 + VPN 进程实况 + retired 主机
POST   /api/zones/{z}/gateway/egress     {on: bool}
POST   /api/zones/{z}/gateway/pub        {proto, port, to}
DELETE /api/zones/{z}/gateway/pub/{idx}  删第 idx 条映射
POST   /api/zones/{z}/gateway/vpn        {proto: wireguard|openvpn, action: on|off}
GET    /api/zones/{z}/gateway/vpn/{proto}/client   .conf / .ovpn 下载（现读现拼）
POST   /api/zones/{z}/monitor            {on: bool, name?, mgmt?, use?}——开关整网流量监控

                      —— 生命周期（整 zone）——
POST   /api/zones/{z}/retire             废弃整个 zone
POST   /api/zones/{z}/revive             恢复整个 zone

                      —— 镜像与终端 ——
GET    /api/mirrors                      镜像连线列表
POST   /api/mirrors                      {from, to}，写完立刻装载单元
DELETE /api/mirrors/{id}                 id 形如 `zone:host->zone:host`，需 URL 编码
ANY    /terminal/{z}/{h}[/{*path}]       ttyd 反代（页面 + WS + 零散资源）

                      —— 自描述 ——
GET    /api/openapi.json                 上面这份清单
```

`/api/topology` 是前端画图的唯一数据源，返回的是**已解析的图**（节点 + 边），不是原始 YAML。这样前端不需要懂 `uplink: main` 是什么语义——它只负责画。解析逻辑留在 core 一处。

`DELETE /api/mirrors/{id}` 的 id 里带 `->`，而 `>` 不是合法 URI 字符（RFC 3986）：
直接拼进 URL 会被 HTTP/1 解析器以 `400 Bad Request` 挡回来，**必须**
`encodeURIComponent`（前端 `web/src/api.ts` 与 `scripts/api-regress.py` 都这么做）。

`GET /api/runs/{id}/events` 只推**实时**事件。run 已结束时它补一条
`{"kind":"finished"}` 就关流——前端是 `fetch` + `ReadableStream`，没有超时，
靠这条事件收尾；不补的话打开历史 run 的日志页会一直卡在"连接中"。
历史输出不在这个流里重放，它在 `/api/runs/{id}/steps` 每条步骤的 `message` 里。

> **实现状态（2026-11）——上面这份 API 面已全部落地，并在真机 PVE 9.2.2 上逐项验证。**
>
> | 分组 | 接口 | 状态 |
> |---|---|---|
> | 核心闭环 | `GET /api/health`、`POST /api/auth/login`、`POST /api/auth/logout`、`GET /api/auth/me`、`GET /api/state`、`POST /api/preview`、`POST /api/apply`、`GET /api/runs/{id}/events`（SSE）、`POST /api/runs/{id}/cancel`、`GET /api/doctor`、`GET /api/openapi.json` | 已实现 |
> | 运维 | `GET /api/runs`、`/api/runs/{id}/steps`、`/retry`、`/api/hosts`、`/api/hosts/{z}/{h}/power`、`/api/snapshots`（列/建/回滚/删）、`/api/destroy` | 已实现 |
> | 终端 | `/terminal/{z}/{h}/*` 反代（ttyd + WS + ZMODEM） | 已实现，页面 200 / WS 101 |
> | 规格 | `GET/PUT /api/specs`、`/api/specs/{name}`、`/api/specs/{name}/zones/{z}`（zone 片段读写） | 已实现，往返只做块状风格归一 |
> | 网关 | `/api/zones/{z}/gateway/{status,egress,pub,pub/{idx},vpn}`、`/vpn/{proto}/client` | 已实现 |
> | 生命周期 | `/api/hosts/{z}/{h}/{retire,revive}`、`/api/zones/{z}/{retire,revive}` | 已实现 |
> | 镜像 / 拓扑 | `/api/mirrors` 的 GET/POST/DELETE、`GET /api/topology` | 已实现 |
> | **未实现** | **升级迁移**（从未发布过，刻意不做） | — |
>
> 早期把"没实现"写成"只能在真机验证后再写"——真机到手后发现这个理由不成立的地方已经
> 补齐（终端反代的两处坑、`dnat ip to`、映射回程 SNAT、`pct exec` 非零即错导致的状态探测
> 全丢弃，都只有真机能撞出来）。
>
> **规格持久化**落在 `paths.specs/*.yaml`，不是 SQLite：运维要能 `cat`、能进 git、能手工改回去。写入走"先 parse + resolve 校验，再 tmp + rename 原子替换"——非法 YAML 一律 400 且不碰旧文件。刷新页面从服务端读回，改过未保存时标题旁有 `未保存` 标签。

### 8.4 认证与暴露面

先把最难听的话说在前面：**面板等于 PVE 宿主 root**。它能 `pct exec` 进任何容器、能写 `/etc/pve/lxc/*.conf`、能删任何 CT。任何"方便"的设计都在削这个安全边界，所以默认值必须偏保守：

| 项 | 默认 | 说明 |
|---|---|---|
| 监听 | `0.0.0.0:8443`（`install.sh` 写进 `config.toml`） | 不绑 `127.0.0.1`——你要用浏览器访问，绑本地等于每次都得开 SSH 隧道，实际上所有人都会去改成 `0.0.0.0`，不如一开始就正视 |
| TLS | 默认开：`install.sh` 用 `openssl` 签一张 10 年自签证书，SAN 含宿主 IP、主机名与 `localhost`。`tls_cert` 与 `tls_key` **都存在**才启用，缺一个就退化成明文 HTTP | 内网够用；要真证书把这两个键指到 Caddy 签的路径 |
| 初始账号 | 只有 `admin`。密码在服务**首次启动**时生成 20 位随机串，**只打进 journal 一次**；hash 落 `$STATEDIR/password.hash`（0600）。安装器刻意不把它捞出来打印——安装输出会进 shell 历史和自动化管道 | 不做"首次访问自己注册"——那等于谁能连上端口谁就是管理员；也不落明文文件 |
| 忘记密码 | 删 `$STATEDIR/password.hash` 再 `systemctl restart labyard`，下次启动重新生成 | 宁可走"物理能登宿主就能重置"，也不加一条能被远程打的重置链路 |
| 账号数量 | **只有 `admin` 一个**，不做多用户、不做角色 | 单机实验环境；多用户要配套权限模型、审计归属、密码重置流程，收益不成比例 |
| 密码存储 | `scrypt`（RustCrypto `scrypt` crate，N=2¹⁵ / r=8 / p=1，16B salt，32B 输出），比对走 `subtle::ConstantTimeEq` | 用 RustCrypto 的 scrypt 而非 argon2，少一个依赖且安全性同级 |
| 改密码 | **没有 Web 接口**，改密码 = 删 hash 文件重启重新生成 | 单用户环境下"改密接口"就是一条多余的远程攻击面；真要固定密码，直接改 `password.hash` |
| 会话 | 标准库 `hmac`+`sha256` 签名的 cookie，内容只有"用户名 + 过期时间戳"；`HttpOnly` + `SameSite=Strict`，**只在 TLS 启用时加 `Secure`**；默认 8 小时 | 前端不存 token，避免 XSS 放大；明文 HTTP 部署下硬加 `Secure` 会导致 cookie 根本存不下来，登录变成"输完密码还是未登录" |
| CSRF | 靠 `SameSite=Strict` 兜住：跨站表单/`fetch` 不会带上会话 cookie | 没有额外加自定义头校验，见下面"没做的" |

**明确没做的**（早期版本这张表把它们写成了"已实现"，与代码不符，此处更正）：

| 没做 | 为什么 |
|---|---|
| TOTP 二次验证 | 需要额外的密钥存储、恢复码、时钟漂移处理与前端二维码流程；单用户内网环境下，直接受益不如把密码长度与 scrypt 参数拉满 |
| IP 允许列表 | 同样的效果可以交给宿主 `nftables` 或上游防火墙，在应用层再做一遍只会让人误以为"面板已经安全了" |
| `audit` 审计表 | 有 `runs` 与 `step_state` 两张表记录了"谁在什么时候跑了什么"，足以复盘；再建一张只写不读的 `audit` 表收益很低 |
| 自定义 CSRF 头（`X-Labyard: 1`） | `SameSite=Strict` 已经挡住跨站携带 cookie；同站子域攻击下自定义头并不构成防护（攻击者一样能设），属于"看起来安全"的措施 |

一个必须承认的取舍：**没有做"两个进程、非 root 前端 + root 后端"的拆分**。那种架构更安全，但要自己写一套 IPC、权限校验、重启编排，成本大概等于整个平台的一半，而收益只在"Web 有 RCE 时"这一种场景。单机实验环境、内网、强密码，我选简单的那条路，并且把它写在这里，而不是假装不存在。

### 8.5 页面上每个选项的数据来源

早期设想的是一套"表单向导"：每个字段都从 PVE 查出来变成下拉框。实际做下来
是**可视化表单 + YAML 双入口**（理由见 §8.1），但表单只改结构，不做"PVE 能力下拉"，
所以这一节描述的是"信息从哪来"，而不是"下拉框的选项从哪来"。原则没变：
**能查到的事实不要让用户手打**，能在写之前发现的冲突不要留到 apply 才报。

| 信息 | 出现在哪 | 数据来源 |
|---|---|---|
| 规格清单 | 规格页、编排页 | `GET /api/specs`（扫 `paths.specs/*.yaml`，坏文件也会列出并标 `valid:false`，不会让整个列表消失） |
| 单个 zone 的片段 | 规格页的片段抽屉 | `GET /api/specs/{name}/zones/{z}`，返回原文片段；不存在返回 `exists:false` 而不是 404 |
| zone 与上联关系、镜像连线 | 拓扑页 | `GET /api/topology`（后端解析好的 nodes + edges，前端只画） |
| 主机、角色、IP、vmid、电源、retired | 主机页 | `GET /api/hosts`（`db.all_vmids()` 与 `pct list` 对账后合并规格里的角色与地址） |
| 网关实况（egress / 端口映射 / VPN 是否真在跑） | 网关面板 | `GET /api/zones/{z}/gateway/status`；VPN 的运行态只能问容器（`systemctl is-active`），查不到就报未运行，不猜 |
| 端口映射现状 | 网关面板 | 同上响应里的 `pubs` 数组；删除按索引 `DELETE .../gateway/pub/{idx}` |
| 镜像连线现状 | 网关面板、拓扑 | `GET /api/mirrors` |
| 快照 | 主机页 | `GET /api/snapshots/{z}/{h}` |
| 存储能力、模板存储、PVE 版本、外部命令 | 概览页、`labyard doctor` | `GET /api/doctor`（`pvesm status` 列存储与内容类型；`vztmpl` 能力单独一行，因为模板和 rootfs 是两种内容类型） |
| run 状态与日志 | 运行历史 | `GET /api/runs`、`/runs/{id}/steps`、`/runs/{id}/events`（SSE） |

**没有做的两件事**，写在这里免得被当成承诺：

1. **没有 `settings` 表**。早期设想要存"上次选的存储、终端字号、主题"这类
   操作习惯。实际实现时发现这类状态几乎没有：界面只有 6 个入口，没有多步表单，
   也就没有"上次的选择"可言。真需要的偏好放浏览器 `localStorage`，不进 DB。
2. **模板名与存储名不做校验下拉**。写错时由 apply 阶段 PVE 自己报错（错误信息
   原文透传到 run 日志里）。在 Web 侧再做一遍白名单等于把 PVE 的版本差异复制一份
   到我们这边——`doctor` 把"能放模板的存储"列出来已经够用。

**冲突检测放在解析层**，不放在表单层：网段重叠、vmid 冲突、`uplink` 指向不存在的
zone、镜像目标不是 `monitor`、同一端口映射写两遍……全部在 `resolve_all()` 里报错，
`POST /api/preview` 与 `POST /api/apply` 都走同一条路径，所以"预览能过、应用报错"
这种事不会发生。错误信息带具体对象名（哪个 zone、哪台主机、哪个端口），
不返回 `invalid spec` 这种废话。

### 8.6 网关策略 / VPN / retire / 多规格 / 拓扑（2026-11 补齐）

这几块是设计稿里最早写、实现得最晚的：它们全都依赖"真机上到底怎么配"的事实，
而事实只能撞出来。下面记的是**实现时定下的做法**和**为什么不是另一种做法**。

#### 8.6.1 网关策略：每次全量重建 nft 表，不做增量

网关容器里的规则表叫 `inet labyard`，每执行一次 gateway 步骤就

```
nft delete table inet labyard 2>/dev/null; nft -f -
```

整表重建。增量维护（记住上次的规则、只加删变化的部分）看起来更优雅，但它要平台
自己持有一份"我以为现在是什么样"的影子状态——而这份影子随时会被手工 `nft` 命令、
重启、别人登进来改东西搞得与事实不符。全量重建的成本是几十毫秒，收益是**规则与
规格永远一致**，这个交换在任何时候都划算。同理适用于容器内的 `/etc/resolv.conf`、
`ip route`，那些也是"按最终状态写一遍"而不是"改一条"。

规则顺序是有语义的，不能随便排：

| 顺序 | 规则 | 为什么必须在这 |
|---|---|---|
| 1 | `ip daddr <zone> ct status dnat accept` | 被 DNAT 过的包**目标地址已经变了**，看起来像"从内网发往内网"，会被默认拒绝入向的规则吃掉。没有这一条，端口映射只有第一个包通，后续包全丢——症状是 `curl` 卡住超时而不是被拒，极难查 |
| 2 | egress 关闭时的 `iifname <内网口> oifname <上联口> drop` | 必须排在 `ct state established,related accept`**之前**。放后面就等于没放：连接先被放行，丢弃规则永远匹配不到。注意它**不带 `ct state new`**——见下 |
| 3 | `ct state established,related accept` | 回程包 |
| 4 | 按需放行 VPN 网段 → 内网 | 只放行 VPN 接口进来的 |
| 5 | `iifname <上联口> drop` | 上联入向默认拒（除已放行的 DNAT） |
| 6 | masquerade（v4 / v6 / wg / ovpn） | 出向改写 |

`egress: false` 的语义是"**立刻切断**"，不是"不接新连接"：已建立的连接也一起断。
这是想清楚之后选的，不是图省事——实验里把出网关掉，要的就是"这台机器此刻真的
不通外网"，能验证"离线状态下会不会出问题"。如果只挡新连接，一条长连接（`apt`、
`ssh`、长轮询）还在跑，你看到的"离线"就是假的，结论也就是假的。

真机上验证过：关掉开关后，内网主机 `curl` 外网 6 秒超时；打开后同一个命令 200。
`nft list chain inet labyard forward` 能看到 drop 排在 established 之前。

#### 8.6.2 VPN：密钥只存在容器里，平台不留副本

wireguard 的私钥、openvpn 的 CA 与客户端证书，全部在网关容器的 `/etc/wireguard`、
`/etc/openvpn` 里生成并保存。平台**不落任何副本**，也**不做密钥备份**——理由很直接：
多存一份就多一个泄露面，而这份密钥的唯一用途是连进这个实验内网，丢了（容器重建）
就重新生成一次，代价接近零。

下载客户端配置时，后端是 `pct exec cat` **现读现拼**：wireguard 读私钥 + 拼
`[Interface]/[Peer]`，openvpn 读 `.ovpn`（其中内联了证书）。所以"下载"这个动作
不会在服务端留下缓存文件。

`/dev/net/tun` 是另一个真机事实：LXC 默认不给 tun 设备，VPN 起不来；而
`pct set <vmid> --dev0 /dev/net/tun` **必须重启容器才生效**。所以打开 VPN 时
平台会自己判断、自己加、自己重启一次——把"要先加 tun"这条隐性知识从用户的脑子
里搬到代码里。

#### 8.6.3 retire：三级生命周期的中间那一级

`stop` / `retire` / `destroy` 三级的语义差别只有一句话：

| 动作 | 容器 | vmid / IP / 磁盘 / 快照 | 用途 |
|---|---|---|---|
| `stop` | 停机 | 保留 | 临时关一下 |
| `retire` | 停机 + 摘镜像实例 + 拓扑折叠 | **保留** | "这台暂时不要了，但别删，可能还要回来核对" |
| `destroy` | 删除 | 全部释放 | 真不要了 |

retire 是打靶场上最常用的那个动作：一台机器已经打完，但它的快照、磁盘里的痕迹
还有价值，直接删掉等于把证据扔了；一直开着又占内存、还继续往监控机里灌流量。

实现上：`retired(zone, host)` 记一行 DB（`host = '*'` 表示整个 zone），拓扑里该节点
变成灰色虚线、镜像实例（`labmirror@` 单元）被停掉、`gateway`/`mirror` 的步骤记录被
清空。revive 时反过来：`pct start`，因为步骤记录被清了，下一次对账会**真的重跑**
gateway 与 mirror，把策略与镜像原样装回去；其余步骤哈希未变，照常跳过。

#### 8.6.4 多规格：文件是唯一真相源，改 YAML 用 CST 就地编辑

`paths.specs/*.yaml` 一份文件一组 zone。多份的意义是**不同的实验互不干扰**
（`red.yaml` / `blue.yaml`），也让 git 历史更干净。

表单改规格（切出网、加端口映射、开关 VPN、加删 zone）一律走 `yaml-edit` 的
rowan CST 就地替换：只动目标节点，**注释、空行、引号风格、字段顺序全部保留**。
这条不是为了好看——手写的 YAML 被 UI 一改就丢注释，用一次就不敢再用第二次，
那么这个 UI 等于没写。

代价写在这里：`GET /api/specs/{name}/zones/{z}` 返回的 zone 片段是 serde 重新
序列化的结果，**该 zone 内部的注释会丢**（文件其余部分不受影响）。片段只是编辑框
的种子，保存时整段 zone 被替换，留不住的东西就不假装留得住。

删除 zone 只改 YAML，**不动 PVE**。删容器是 `destroy` 的事，那里要求手打 zone 名
二次确认。把"从规格里去掉"和"删掉机器"混成一个按钮，是迟早要出事的。

#### 8.6.5 拓扑：后端出图，前端只画

`/api/topology` 返回的是**解析后的节点与边**：

```
节点  network(net:wan/net:{zone}) | host    边  attach(uplink/inner/mgmt) | mirror
```

节点 id 是稳定字符串（`zone:main`、`host:main:web`），前端用 react-flow + dagre
只负责布局与绘制，不懂 `uplink: main` 是什么语义。这样做的原因很实际：语义解析
已经有一处（`core::spec`），再在前端写一遍，两处就会慢慢不一致，而**拓扑画错了
比不画更危险**——你会照着一张错的图去排障。

已知取舍：不同规格里的同名 zone 会撞 id（后端一套 key 空间），前端按"先出现的
留下"去重。真出现这个场景（两份规格都有 `main`），说明命名该调整了，而不是让
画布去区分它们。

#### 8.6.6 重启自愈：运行时状态必须落成开机单元

容器一重启（retire→revive、`pct reboot`、宿主重启），`ip addr` 配的地址和 nft 的规则表
**全部消失**——它们都是内存里的运行时状态。而 net / gateway 两步是哈希门控的：apply 看到
"这个 key 的输入哈希没变"就直接跳过。两者相加是个很糟的组合：

| 丢的是什么 | 表面上看 | 实际 |
|---|---|---|
| 容器地址 | 平台一切正常、拓扑照画 | 整个 zone 静默失联，终端/SSH 全部连不上 |
| 网关 nft 表 | 平台显示"策略已下发" | 没有规则表 = 默认放行，隔离与映射同时失效（**fail-open**） |

真机上就是这么发现的：retire/revive 验证完之后，三台容器的 `eth0` 只剩 link-local，
终端 404、`ip -4 addr show eth0` 空输出。修法不是"revive 时补跑 net"——那只覆盖 revive
这一条路径，宿主重启仍然中招。真正的修法是**把这两段脚本落成容器内的开机 oneshot 单元**：

```
/usr/local/lib/labyard/labyard-net.sh  +  labyard-net.service   地址 / 路由 / 解析器
/usr/local/lib/labyard/labyard-nft.sh  +  labyard-nft.service   nft 全量策略
```

单元是 `Type=oneshot` + `RemainAfterExit=yes` + `WantedBy=multi-user.target`，
`After=network-pre.target` / `Before=network.target`——比依赖网络的业务（ttyd 绑内网地址）
先跑。脚本由**同一套生成器**产出，apply 时"写文件 + `systemctl restart`"一次完成两件事：
现在生效，重启后自愈。

两个真机细节写在这里，免得下一个人再撞：

- `ExecStart` 必须写成 `/bin/sh <脚本>`。生成器吐的是纯命令片段、没有 shebang，直接当
  可执行文件起会 `status=203/EXEC`（`Exec format error`）——单元 installed + enabled，
  一启动就失败，容器里连地址都没有。
- gateway 步骤用**同一个** `labyard-nft` 单元名，后写的覆盖先写的。于是开机跑的一定是
  "最后一次下发的完整策略"，而不是 net 阶段那份基础策略（只有默认拒绝、没有映射）。

配套的是 `RULESET_VERSION`：步骤哈希只看输入、不看生成器代码，所以生成逻辑有实质变化时
必须手动 +1，否则"改好了但不生效、apply 全程显示 done"。它现在同时管地址脚本与
nft/VPN 脚本（v3 就是这次把两者落成开机单元的改动）。

真机验证：`pct reboot 102`（服务主机）后地址回来、且仍然没有默认路由；`pct reboot 101`
（网关）后地址、含 `tcp:17681 → 10.77.0.20:7681` 的完整策略、以及"egress 关闭"这个状态
都原样恢复（映射端口继续返回 ttyd 的 404，说明链路通）。


---

## 9. 遗留与风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| 1 | 平台以 root 跑 + Web 暴露 | 拿到面板 = 拿到宿主 | 默认只一个 `admin` + scrypt + 会话 cookie（`HttpOnly`/`SameSite=Strict`/TLS 下加 `Secure`）+ 自签 TLS；**没有** IP allowlist / TOTP / 审计表（§8.4 里逐条写明），外网暴露前请在宿主或上游加一层来源限制 |
| 2 | Web 是唯一界面，服务挂了就没法操作 | 实验环境"停在那儿" | `labyard doctor` 不需要 Web；`systemctl restart labyard` 恢复；`apply` 命令可应急；再不济直接点 PVE 页面（桥和 CT 都在，只是没人管状态） |
| 3 | 桥名受 `IFNAMSIZ`(15) 限制 | 长 zone 名会让 `lab` + 名字超长，建桥直接失败 | 解析阶段就拦：zone 名 >10 字符报 `zone 名 \`x\` 超过 10 字符，网桥名会超出 IFNAMSIZ(15) 限制`（桥名规则是 `lab` + zone 名） |
| 4 | 多层 NAT | 端口映射要逐层配；conntrack 表项按层数翻倍；排障路径变长 | 映射链按 zone 上联自动生成（`uplink` 指向谁，就把 DNAT 落在谁的网关上）；**没有** `doctor --trace` 逐跳探测——排障靠 `nft list ruleset` 看 prerouting 链 + 在内网主机侧 `tcpdump` |
| 5 | 监控机重建后 veth 名变 | 采集静默失效（规则挂不上但不报错） | 每次 apply 按桥现探落点并 rewrite；脚本内容参与哈希 |
| 6 | apt 源不可达 | provision 失败 | 源可切（tuna/ustc/官方/自定义）；`--no-install-recommends`；失败原样回传 |
| 7 | 半成品状态 | 重跑不确定 | §4.3 reconcile；不自动删重建 |
| 8 | 你手动建的 VM 接进内网 | 镜像漏掉它，抓包文件缺主角 | 规格里声明为成员（§5.9）后，平台按声明接管：桥、地址、镜像都覆盖；没声明的会出现在"宿主上的其它虚拟机"里并可一键纳管，但**不会自动进规格**，所以漏配一台不会自己变成成员 |
| 8b | QEMU 开了 PVE 防火墙时成员口是 `fwln`/`fwpr` | 按 `tap` 前缀识别不到 | **未处理**：成员口按规格里声明的 vmid 定位（`veth<vmid>i<idx>` / `tap<vmid>i<idx>`），PVE 防火墙把 veth 换成 `fwln`/`fwpr` 时镜像口会挂不上，需要在 zone 里关掉 PVE 防火墙 |
| 9 | 快照与镜像口 | 回滚后 veth 变化 | 回滚后重跑 `mirror` 步骤 |
| 10 | 平台自身升级 | DB schema 变 | 编号 SQL 迁移 + `schema_version` 表，启动时按序补齐；**备份要自己做**（`docs/operations.md` 给了 `VACUUM INTO` 的一行命令），平台不会自动备份 |
| 11 | IPv4 / IPv6 两套规则不一致 | 主机用 link-local / 自配 ULA 绕过 v4 防火墙 | 网关规则表是 `inet labyard`（同时匹配 v4 与 v6），`iifname <上联> oifname <内网> drop` 对两族都生效，不存在"只挡了 v4"的分叉；`nat: false` 只是不生成 masquerade，不影响过滤 |
| 12 | 多节点下桥不存在于目标节点 | CT 建好却起不来，或起在错误的二层域 | **首版只支持单节点**；`doctor` 检测到集群直接警告不阻止（单节点功能仍可用），建 zone 时强制绑定当前节点 |
| 13 | PVE 9 AppArmor 4 + 嵌套容器 | 容器里跑 Docker 报错怪异 | §6.7 写明已知问题（bug #6538）；平台不碰业务软件，但报错会很怪，先知道这条 |
| 14 | 厚 LVM 快照是 tech preview | 以为有快照，实际不可靠 | `doctor` 列出存储类型（`lvmthin` / `lvm`），快照操作原样透传 PVE 的报错；**没有**额外的"二次确认"门禁 |
| 15 | 并发 provision 打满磁盘 IO | **全宿主机卡死**，含 PVE 管理面与 SSH（内存/CPU 不足不会这样） | apply 批内并发默认 3（`--concurrency`）+ `systemd` 的 `IOWeight=50` 兜底；**没有** `ionice`（§7.6.5 第 3 条） |
| 16 | 高频 probe 打慢 pmxcfs | `/etc/pve` 是单线程 FUSE，20 台 N+1 查询会拖慢整个节点 | 状态一律走一次 `GET /cluster/resources?type=vm` 批量取（`Pve::cluster_vms`），不逐台查；前端轮询 6–15 秒（§7.6.5 第 7 条） |
| 17 | 改了生成器但步骤哈希不变 | 改好了却不生效，apply 全程 `done`——最容易骗过自己的一类 bug | 生成器版本 `RULESET_VERSION` 进哈希（§8.6.6）；生成逻辑有实质变化必须 +1，否则等于没改 |
| 18 | 容器 / 宿主重启后运行时状态丢失 | 地址没了 → zone 静默失联；nft 表没了 → 网关静默 fail-open（平台仍显示"策略已下发"） | 地址与策略落成容器内开机 oneshot 单元（§8.6.6），重启自愈；真机用 `pct reboot` 验证过 |
| 19 | 日志与临时文件撑满 `/var` | 宿主写不进去，PVE 自己开始报错 | 日志不落盘（只走 SSE 与 DB 里每步一行），临时文件任务结束即删；容量风险主要来自**你自己**在容器里产生的数据 |
| 20 | 一次 apply 跑到一半资源不足 | 半成品状态要手工收拾，比拒绝更贵 | **没有** apply 前预检（§7.6.7 是设想）。半成品靠 §4.3 的 reconcile：修好后重跑，哈希没变的步骤会跳过 |
| 21 | CLI 输出不是兼容契约（改用命令直连的代价） | PVE 小版本改字段 → `KeyError`，且 `pct` 文本格式比 API 更容易变 | 只吃 `--output-format json`，不解析表格；`doctor` 校验关键字段存在；`pve/` 层集中所有命令拼接，一处可改 |
| 22 | 每次命令 fork/exec 约 100–200 ms | 误在循环里逐台调用 → apply 变慢，且频繁唤醒 pmxcfs | 读一律批量（一条 `pvesh get /cluster/resources`）；`pct config` 结果缓存 10 秒；review 时禁止"循环里起子进程" |

---

## 10. 交付范围与实现顺序

**先纠正我自己一个错误**：上一版把"多层嵌套"排成了第二阶段，这是错的。`uplink` 从第一行代码起就是递归的——`uplink: wan` 和 `uplink: suba` 走的是同一条代码路径，只差一次查表。"单层"只是 YAML 里只写了一个 zone，不是代码里的一个分支。把它排成阶段，等于把同一个函数的两种输入说成两个功能。

同理，拓扑图、终端、文件、外来成员不是"增强项"。界面缺了这些，就是没做完。

### 10.1 完成的标准（一张清单，不分版本）

| 域 | 必须全部可用 |
|---|---|
| 建 | zone（任意层嵌套）、桥、LXC、监控机、抓包桥（mir{zone}）、网络接入（v4 + v6 双栈） |
| 配 | 网关全套（转发 / nft / 出网 / 端口映射 / VPN / 重启恢复）、bootstrap（IP·gw·DNS·SSH·apt·时区·lrzsz） |
| 看 | 全局拓扑、zone 详情、单机详情、运行态、外来成员、任务日志 |
| 用 | Web 终端（独立窗口、多开）、文件上传下载（终端内 + 面板）、快照 |
| 管 | stop / retire / revive / destroy、审计、doctor |
| 改 | 改 spec 重 apply、平台自身升级迁移 |

少任何一项都算没完成。**没有"v0.1 只有单层"这种东西。**

### 10.2 实现顺序（技术依赖，不是产品阶段）

| 步 | 内容 | 为什么必须在这 |
|---|---|---|
| 0 | 仓库骨架：`Cargo.toml`、`rust-toolchain.toml`、`Makefile`（`web` → `build` 依赖链）、`Runner` 抽象 | 后面每一步都要能 `cargo test`，骨架不定就会各自长歪 |
| 1 | `src/pve/` 封装 + `doctor` | 连不通、能力不知道，后面全是猜 |
| 2 | `src/core/schema` + `planner` | 纯函数，能在 fixture 上完整单测，不需要集群 |
| 3 | `src/core/runner` + `src/resources/`（bridge / ct / bootstrap / mirror / provision） | 引擎成型；此时 `apply` 已能在真机建出东西 |
| 4 | `src/api/` + SSE + 认证 + 反代 | 网页能登、能看、能点 |
| 5 | 前端：拓扑与详情 → 向导 → 终端与文件 | 前端要有真数据才画得对 |
| 6 | netns 回归 + 真机验收 | 见 10.3 |

每完成一步都是**可运行**的（能起服务、能登录、能看状态），但中间状态不发布、不打 tag。你可以随时 `git pull` 跑起来看一眼，只是功能不全。

### 10.3 验收（对照结果，不含糊）

| 验收项 | 结果 |
|---|---|
| **第二遍 0 变更**：同一份 spec 连跑两次，第二次全部 `noop` | ✅ 真机成立。`step_state` 记 `input_hash`，哈希没变就跳过；改一个字段只重跑受影响的那一步 |
| **多层同时存在**：`main` + `suba` + `subb` + `subc` 四种接法并存，各自能出网 | ✅ `examples/multi.yaml` / `nested.yaml` 都能出计划；真机上跑过 `main` + `lab` 两层，`.1` 落在网关、NAT 生效 |
| **双栈**：v6 开（默认）与 `nat: false` 两组用例都过 | ✅ 同 zone ULA 互通（`ping6` 0.13 ms）、NAT66、**出公网**全部实测：网关 eth1 `accept_ra=2` 拿到上游 GUA（`2408:...`）后，内网主机 `ping6 2400:3200::1` 2/2 通、6 ms。跨 zone 只有 v4 有静态路由，v6 跨 zone 未实现。早期打算的"关掉 v6 时显式 drop"没有实现——规则表是 `inet`，两族一起过滤，见 §9 第 11 条 |
| **外来成员**：手动建一台 VM 挂到桥上，30 秒内出现在拓扑上 | ✅ 可发现、可一键纳管：`/api/hosts` 的 `unmanaged` 列出规格外的容器，纳管后写规格 + 绑 vmid（真机：新建 LXC → 纳管 → 主机表出现、IP 自动挑空闲位 `.21`）。**不会自动进规格**，也不会自动出现在拓扑里（§5.9） |
| **终端与文件**：两个终端窗口同时用不互踢；`rz`/`sz` 通 | ✅ 每个连接独立 fork shell，互不干扰；ttyd 自带 ZMODEM，`sz`/`rz` 可用。**没有**面板上的上传下载（早期想过，砍了：ZMODEM 覆盖主场景） |
| **真机全链路** | ✅ PVE 9.2.2 / x86_64 / 4C11G 上 `doctor` 全绿、`apply` 全步成功、幂等重跑全跳过；详见 §附录 |

早期这一节写着"我没有 PVE 9 环境，所以 1–5 步在假 Runner 上开发"——那个前提已经不成立：
现在有真机，**每条 ✅ 都有真机证据**。也没有留下 `testdata/` fixture 回放层：
纯函数（脚本生成、哈希、YAML 编辑、拓扑）直接单测，需要 PVE 的部分一律上真机跑，
中间那层"录制回放"既不保真也不会腐烂，省了。

---

## 11. 已定与待定

### 11.1 已按你的回答定下

| 问题 | 结论 |
|---|---|
| 抓包网职责 | **只做流量镜像**。每个被监控网络一块 `mir{zone}` 纯二层桥，无 IP、不挂网关，只挂监控机的抓包口。gateway 只有 WAN + LAN 两块口 |
| 监控机管理口 | `zone.monitor.mgmt` **三选一**：不写（本网段）/ `wan`（外网桥 DHCP，方便 SSH/传报告）/ `<zone>`（走该 zone 网关，受策略约束） |
| 内网主机 | **只做 LXC**。VM 由你在 PVE 自建；要接进 zone 就在规格里声明 `vmid`（§5.9），或在主机页点「纳管」。接进来后地址与镜像都覆盖，但平台不重建磁盘。QEMU 目前只能手工写规格 |
| 凭据 | **创建时选**：`key` / `password` / `key-password`，可在 defaults / zone / host 三层逐级覆盖 |
| 结构 | 不是"单层/双层"，是**同构 zone 的自由组合**。`uplink` 指向 `wan` 或任意 zone，串几层都行 |
| 平台边界 | 只管网络环境与基础设施；业务软件、VM、具体服务一律不管，责任分开 |

### 11.2 本轮新增的决定

| 问题 | 结论 |
|---|---|
| CPU 架构影响吗 | 不影响设计。模板在创建时从 `pveam available` 选，平台不硬编码名字。真影响的是 PVE 版本、单/多节点、存储快照能力——全部交给 `doctor` 探测（§6.6） |
| `wan` 模式 | 用户选。全局 `wan.mode` 取 `dhcp` 或 `static`，逐 zone 可覆盖；DHCP 时回读地址入库 |
| 删除语义 | 三级：`stop` / `retire`（保留磁盘快照与分配）/ `destroy`（全清，含快照，需输 zone 名）。桥只在无引用时删（§4.6） |
| IPv6 | **默认开**。内网只发 ULA + NAT66；上联口 SLAAC 取到的 GUA 只给网关做 NAT66 源地址，不下发内网。`target` 角色在 v6 上同样不配 default route。关掉时必须在 forward 链显式 drop（§5.8） |
| 名称与许可 | 项目名 `labyard`；许可证 Apache-2.0（专利授权；只通过 subprocess 调 PVE 的 CLI，不链接其代码，不传染 AGPL） |
| PVE 基线 | **9.x / 单节点**。低于 9.0 拒绝启动，不做 8.x 兼容层；集群只警告不阻止（§6.7） |
| PVE 交互方式 | **命令直连**：`pct` / `qm` / `pvesh`，不用 HTTP API 客户端、不用 token、不配 ACL。读走 `pvesh --output-format json`，写走原生命令（§6.3）。原因：exec 与 conf 直写 API 本来就做不到，用 API 只会多一条通道 |
| 后端语言 | **Rust 1.100**（Python → Go → Rust，见 §7.5.1）。收益：常驻 12 MB、12.7 MB 单二进制（含前端）、无 GC 抖动、长期内存曲线平坦。代价约 +1.5～1.8 倍工期，集中在三层配置合并与异步子进程生命周期 |
| 交付形态 | **单二进制**：`rust-embed` 打进前端与 `assets/`，宿主只需 `scp` + 一个 systemd 单元，不装运行时、不建 venv（§7.3） |
| 界面形态 | **Web 为主**。不做操作型 CLI，只留 `serve` / `doctor` / `apply`（给测试）/ `destroy`（拆环境）四条（§0） |
| 账号 | **单管理员** `admin`，scrypt 存储，密码在首次启动时生成一次。不做多用户、不做角色、**不做 TOTP**（§8.4） |
| Web 终端 | **ttyd** 1.7.7 静态二进制（Debian 13 仓库没有）+ 反代；终端内 `rz`/`sz`（lrzsz）；**内嵌在主机页的抽屉里**，可多开（§7.4） |
| 外来成员 | **可发现、可手动纳管，但不自动纳入**。规格里声明 `vmid` 即接管；没声明的会列在“宿主上的其它虚拟机”里（只提示，不影响镜像与路由），点「纳管」写进规格（§5.9） |
| 交付范围 | **不设阶段**。多层嵌套、全局拓扑、完整页面、终端、文件都属于"做完"的定义（§10） |

### 11.3 仍按默认执行，你不反对就这么定

- `vmid_range: 1000-1099`；inner 从 `.10` 起分配，`.1` 归网关
- apt 源默认 `tuna`；模板列表从 `pveam available` 实时拉，不硬编码版本
- 容器内应急面板（旧的 `labpanel`）**不移植**——Web 是唯一入口，再塞一个容器内面板就是两套认证、两套审计
- 单节点上限：12 张桥、单 zone 32 台主机——超了先报错，别等 PVE 自己变慢
- 面板监听 `0.0.0.0:8443` + 自签 TLS + 首次启动生成的随机密码（§8.4）。**没有审计表**：`runs` + `run_steps` 已经是"谁在什么时候跑了什么"
- 存储、模板、apt 源、凭据写在 YAML 里（不是下拉框），写错由 apply 阶段 PVE 原文报错；`doctor` 负责把可选项列出来

### 11.4 这一轮我替你做的三个判断（有异议就说）

| 判断 | 我的选择 | 反方理由 | 为什么还是这么定 |
|---|---|---|---|
| 要不要 CLI | 只留 4 条命令 | "脚本化/CICD 需要" | 单机单用户场景，CI 跑不到你的 PVE；真需要批量，Web 的一次 apply 就是批量 |
| IPv6 默认 | 开 | "实验室不需要 v6，默认关更简单" | 默认关 = 这段代码永远不被测；等要用时才开，必然炸 |
| 存储默认 | 不设默认，**第一次必须手选** | "给个默认值体验更顺" | 存储选错（选了不支持快照的 `lvm`）会在两个月后才报应，不如建 zone 时就撞一次墙 |

### 11.5 仍然开着的问题

按"是否需要你表态"分三类，都不阻塞开工。

**A. 原来想问你，后来发现不用问（`doctor` 能自己扫）**

| 原问题 | 现在怎么做 |
|---|---|
| 宿主上哪些 CIDR / VMID 已被占用 | `doctor` 列存储与模板存储能力、`pct list` 列现状；**网段/vmid 冲突在解析阶段拦**（`resolve()` 里查本规格内的重复与 `vmid_base` 撞号），不做全局占用扫描 |
| 宿主磁盘是 SATA SSD 还是 NVMe | **没做**。`--concurrency` 默认 3，想调得自己试（README 里有实测数据：1/2/3/4 → 200/105/70/95 秒） |
| CPU 架构 / 模板名 | 模板名写在你自己的 YAML 里；`doctor` 检查"有没有能放 `vztmpl` 的存储"，不猜模板名 |

**B. 真需要你表态的三条（偏好，不是技术）**

| 问题 | 我的默认 | 另一种选择 |
|---|---|---|
| apply 时资源不够（内存/磁盘/load 超阈值） | **没做预检**：跑到哪算哪，失败后靠 reconcile 重跑 | 加一道预检要自己估模板解压后的体积、内存峰值与并发叠加，估错了比不估更糟 |
| 日志与 `retired` 机器的保留期 | 日志不落盘；`retired` **无限期保留**（继续占 IP 与 VMID） | 给 `retired` 加过期回收会让"我明明没删它"变成"它自己没了" |
| 自动发现（你手工建的机器） | **不做**（§5.9） | 做的话要引入一份"运行时发现但不入档"的影子名单，与"文件即真相"冲突 |

**C. 我按默认执行，随时可改（纯参数）**

- 并发：apply 批内 3（`--concurrency`）、计划闸 4、scrypt 闸 1（§7.6.4）
- `systemd`：`MemoryMax=384M`、`MemorySwapMax=128M`、`CPUQuota=50%`、`IOWeight=50`、`TasksMax=256`
- 前端轮询：拓扑 15 s、运行历史 10 s、概览 10 s、主机 8 s、网关 6 s

---

## 12. 仓库形态

```
labyard/                          Apache-2.0
├── LICENSE  NOTICE  README.md
├── Cargo.toml  Cargo.lock         依赖定义与锁定（Cargo.lock 必须提交）
├── rust-toolchain.toml            锁编译器版本（与 Cargo.lock 一起保证可复现）
├── Makefile                       web（vite build）→ build（cargo build）依赖链
├── .gitattributes                 web/dist 标 linguist-generated / -diff（免得 diff 被构建产物淹没）
├── src/
│   ├── main.rs                    入口：clap 分发 serve / doctor / apply
│   ├── config.rs                  config.toml 解析（含 tls_enabled 等派生判断）
│   ├── core/                      spec（YAML→模型）/ plan（步骤图）/ runner（子进程）
│   ├── pve/                       pct/pvesh 命令拼装 + JSON 解析 + 能力探测
│   ├── resources/                 执行器：probe → decide → apply
│   │   ├── mod.rs                 Executor：各 StepKind 的落地与开机单元安装
│   │   ├── net.rs                 地址/路由/解析器脚本（纯函数，可快照回归）
│   │   ├── gateway.rs             nft 原子批次 + VPN 脚本（纯函数，可快照回归）
│   │   └── assets.rs              rust-embed 读取 + provisioner 哈希
│   ├── db/                        rusqlite 封装 + 编号 SQL 迁移器（不用 ORM）
│   ├── api/                       axum 路由
│   │   ├── mod.rs                 路由表 / 认证 / SSE / 终端反代 / OpenAPI
│   │   ├── ops.rs                 规格 CRUD、网关、镜像、retire/revive、拓扑入口
│   │   ├── specs.rs               规格文件的 CST 就地编辑（yaml-edit）
│   │   └── topology.rs            YAML → 节点与边
│   └── (前端产物)                  rust-embed：api/mod.rs 里的 #[folder = "web/dist"]
├── assets/                        rust-embed：随二进制分发的执行体
│   ├── base/bootstrap.sh          主机接入（IP·v6 / SSH / apt / hostname / 时区 / lrzsz）
│   ├── base/labmirror.sh          宿主侧 tc mirred 镜像脚本
│   ├── base/labmirror@.service    镜像单元模板（每对源/目标一个实例）
│   └── provisioners/apt-mirror/   apt-cacher-ng 的安装与配置
├── web/                           React + TS + AntD 源码（vite 构建到 web/dist）
├── bench/                         三语言内存对照 + 分配器对照（可复现）
├── scripts/                       smoke.sh（HTTP 冒烟）/ api-regress.py（真机逐接口回归）
│                                   / dev.sh / measure.sh
├── installer/install.sh           宿主一键安装 / --ttyd（离线）/ --uninstall / --purge
├── examples/                      main.yaml / nested.yaml / multi.yaml
└── docs/
    ├── design.md                  本文（唯一权威设计文档）
    └── operations.md              安装、升级、备份恢复、排障
```

单测写在各自模块里的 `#[cfg(test)] mod tests`（不另设 `tests/` 目录）：纯函数
（脚本生成、哈希、YAML 编辑、拓扑）全部可在没有 PVE 的机器上跑——这是刻意的，
CI 不需要 root 也不需要 PVE。

三条纪律：

- 旧目录（`lab-net/` 等）**不进这个仓库、不做依赖**；旧实现的行为清单（空 set、
  0 字节密钥、`enabled-runtime`、tun 必须 bind mount、veth 名漂移、reset 清状态）
  已经内化成回归测试项
- `docs/design.md` 是唯一权威设计文档，旧的 `部署.md` 不再维护
- 没有 CI：这是单人单机的项目，真正要防的回归都在真机上（`make test` + `scripts/api-regress.py`）。
  `rust-toolchain.toml` 声明了 `rustfmt`/`clippy` 组件，装了就能用；**精简镜像里装不上，
  所以两者都不作门禁**——`cargo test` 与真机回归才是门禁

**关于 `web/dist/` 要不要提交**：提交。理由：`install.sh` 在 PVE 宿主上放一个二进制就能跑，用户不需要装 Node 20 再 `npm ci && npm run build`（在 Debian 13 上装 Node 20 还得先加 nodesource 源，正好撞上"不要多一层依赖"的原则）。代价是仓库里多几 MB 构建产物、diff 里噪音大——用 `.gitattributes` 标 `linguist-generated` 和 `-diff` 就够了。**注意 `rust-embed` 要求目录在编译时存在，所以 `web/dist` 必须在 `cargo build` 之前生成，`Makefile` 里把 `web` target 设为 `build` 的前置，并保证 `web/dist/` 里有东西，否则 `cargo check` 在干净 clone 上直接失败。**

**关于 `ttyd` 二进制要不要进仓库**：不进。它 1.3 MB、有独立的版本节奏和 CVE 响应，塞进仓库等于我们替上游发版。`install.sh` 从 GitHub Release 按 `uname -m` 下载；离线环境用 `--ttyd /path/to/ttyd` 指定本地文件（不校验 SHA256——上游按架构滚动发布，没有稳定的校验和文件，与其假装校验不如写清楚这一点）。

---

## 13. 已经想清楚、不再问你的三件事

**镜像开关的粒度**：数据模型按"每条 `from → to` 一个 systemd 实例"（这样才能多源投同一台监控机）；界面上默认显示**一个总开关**，展开后是每条链路的独立开关。两个需求同时满足，不用二选一。

**单节点 vs 集群**：只支持单节点，但 `doctor` 在集群环境下**只警告不阻止**——集群里的单节点用法（桥和 CT 都在同一台）完全成立，强行拒绝反而挡路。真要跨节点编排是另一个项目。

**终端多开怎么办**：不共享 PTY（ttyd 的 `-Q` 不开），每个浏览器连接独立 fork 一个 shell。所以开五个标签页就是五个互不干扰的终端，和本地开五个终端窗口一样。代价是不能"两个人看同一个终端"——那需要 tmux，而 tmux 装不装是业务范畴，平台不管。


## 附：真机验证记录（PVE 9.2.2 / x86_64 / 4C11G）

在真实 PVE 上跑通 doctor → apply → 幂等重跑全链路（2026-11-18）。真机暴露了六处
单测全绿也拦不住的缺陷，全部修复：

| # | 缺陷 | 真机症状 | 修复 |
|---|---|---|---|
| 1 | 模板存储与 rootfs 存储混用 | `pveam download local-lvm` 退出 255（lvmthin 不支持 vztmpl） | 新增 `template_storage`（三层合并，**默认跟随 storage**）；doctor 增加模板存储检查项 |
| 2 | 网关接口地址写成网段号 | `ip addr replace 10.77.0.0/24 dev eth0`，网关拿不到 `.1` | `gateway_script` 拆成 `gw_v4`（上接口）与 `zone_cidr`（NAT 源） |
| 3 | 失败不阻断下游 | 模板失败后 net 步骤对不存在的容器 `pct exec` 退出 2 | 阻断集合 + `UpstreamSkipped` 事件；集合含被跳过的 key（传播闭包） |
| 4 | SQLite 写失败被吞 | 并发写撞 WAL 单写者锁，容器建成但状态没记录，下轮误判"从未执行" | `busy_timeout(5s)`；`record_*` 返回错误；写失败按步骤失败 |
| 5 | 桥存在判定用配置面 | `ip link add` 的桥在 pvesh network 里 `type=unknown` 被滤掉，每轮误判"已消失" | 新增 `kernel_bridges()` 查内核实际面；配置视图留给 doctor |
| 6 | 安装器复述初始密码 | 安装输出（shell 历史）里出现密码 | 只在 journal 出现一次（bootstrap_password 原有语义） |

**验证结论**：doctor 9 项全绿；apply 6/6 步成功；网关 `.1`、NAT 出网、内网主机无默认
路由、宿主入向被丢全部符合设计；幂等重跑 6/6 跳过。

**经验**：这六处全部属于"字段/事件存在但行为断线"或"判定用错了视图"，单元测试
（mock 注入）无法发现——mock 的行为是按**预期**写的，而真机的 PVE 配置视图、
WAL 锁行为、pveam 存储约束是按**实际**来的。probe → decide 的"只信现状"哲学
必须配套"现状从哪个视图读"的审查。
