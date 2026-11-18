# labyard · 实验室编排平台设计稿

> 项目名：**labyard**（`lab` + `yard`，铁路编组场——按需把"网络单元"编组起来）
> 许可证：**Apache-2.0**（带专利授权；Proxmox VE 本体是 AGPL-3.0，但我们只通过 subprocess 调用其 CLI、读写其配置文件，不链接其代码，不传染）
> 基线环境：**Proxmox VE 9.x（Debian 13 Trixie）/ 单节点**。8.x 只做明确报错，不做兼容层
> 状态：**设计稿，尚未实现**。本文只定架构、数据模型、接口和取舍，不含代码。

---

## 0. 结论先行

四句话：

1. **Web 是唯一的产品界面**。所有需要"选"的东西（存储、模板、网段、凭据、网卡）都在网页上用下拉和表单完成，没有等价 CLI。理由很实际：PVE 里可选的存储、模板、网卡是**运行时才知道的**，写进命令行就是一堆 `--storage local-lvm --template debian-13-standard_13.2-1_amd64.tar.zst`，还容易写错版本号。
2. **但核心必须是库，不是网页的一部分**。`src/core` 不 import `axum`，Web 只是它的一个调用者。这样 E2E 测试可以驱动引擎（`labyard apply -f x.yaml --yes`），却不需要维护第二套用户界面。
3. **全新实现，但继承经验**。旧目录（`lab-net/` 等）全部废弃，不进依赖链；`assets/gateway/` 按旧实现的行为清单重写。旧代码里踩过的六个坑（空 set、0 字节密钥、`enabled-runtime`、tun 必须 bind mount、veth 名漂移、reset 清状态）直接变成回归测试项——这些比代码值钱。
4. **幂等的单位是"步骤 + 输入哈希"，不是"跑没跑过"**。每次都先 probe 实际状态、再 diff、再 apply；配置没变就 skip，脚本改了哈希变了就重跑。这是"更新"和"重复执行"能同时成立的唯一办法。

**关于"要不要 CLI"**，我的取舍是：**不做操作型 CLI，只留三条运维命令**——

| 命令 | 用途 | 为什么不能砍 |
|---|---|---|
| `labyard serve` | 起服务（systemd 调它） | 服务入口 |
| `labyard doctor` | 打印环境探测结果（PVE 版本/存储能力/tun/桥/依赖） | 出问题时唯一能贴给别人的东西；Web 挂了它还活着 |
| `labyard apply -f <file> --yes` | 非交互执行一份 spec | **给自动化测试用**，不是给用户用 |

除此之外没有任何 `labyard zone create` / `host add` 之类的子命令。两套界面维护同一套语义，成本远大于收益，而且必然有一天会不一致。

---

## 1. 起点：旧目录全部废弃

`lab-net/`、`网关+镜像环境模拟/`、`pve-lab/` 一律**不再维护、不作为运行时依赖**。labyard 是全新实现，自带 provisioner，不 `pct push` 任何旧文件。

但旧代码里踩过的坑不能丢——它们变成新平台的**回归测试项**，比代码本身值钱：

| 旧实现 | 教训 | 在 labyard 里的位置 |
|---|---|---|
| `labgw` 的 named set 端口闸 | 空 set 让规则永不匹配，端口"看起来是关的"，排查方向全错 | 端口开关仍用 set；`doctor` 校验 set 非空 |
| `labvpn` 的 0 字节 `server.key` | `wg genkey > f` 在 `wg` 缺失时留下空文件，之后 `[ -f ]` 判定存在、跳过重建 | 所有密钥生成走 `mktemp` → `mv` → `[ -s ]` |
| `openvpn@server` 模板单元 | `is-enabled` 返回 `enabled-runtime` 且 exit 0，"据此判断已启用"是错的 | 只认持久化 wants 链接或运行态，不认 `is-enabled` 退出码 |
| 无特权 LXC 的 tun | 容器内 `mknod` 必然 EPERM；必须 bind mount + `cgroup2.devices.allow`，且 Stop→Start 才重新读 config | 建网关时直接写 `/etc/pve/lxc/<id>.conf` 两行 |
| `labmirror` 硬编码 DST veth | analysis 重建后 veth 名变，规则静默失效 | 每次 apply 重算，且按 mirr 桥探测 |
| `labgw reset` 清空 set | 重置后出网/映射全丢，需要手工恢复 | 状态恢复做成独立步骤，reset 后自动 replay |

边界说清楚：**旧目录不进新仓库的依赖链**；其中的实现可以按需搬运（同一作者、同样 Apache-2.0），但搬运前必须补齐上面的测试项，否则宁可重写。

关键判断没变：**缺的不是"脚本能力"，是"编排能力"**——谁先建、建在哪、建过没有、改过没有、失败停在哪一步。重心在 §4 的引擎。

---

## 2. 目标与非目标

### 2.1 做

- 在网页上一次性建出：桥（内网 / 镜像网）、LXC（网关 / 分析 / 服务端 / 客户端）、容器内网络、基础系统配置、镜像接线
- zone 列表、全局拓扑图、创建向导（表单驱动 + YAML 预览）、任务日志、运行时操作（出网 / 端口映射 / VPN / 镜像开关）
- 重跑安全：已完成的跳过，改过的更新，失败的中断点续跑
- 自由组合：`main` 上挂 `suba`、`subc`，`subb` 再挂 `suba`；每个 zone 的网关 WAN 接上一级的内网桥
- 统一命名与地址分配（v4 + v6），分配结果持久化，重跑不重排

### 2.2 不做（明确写死，避免无限膨胀）

- **不做 VM（qemu）**。只建 LXC；要 Windows / 带内核的靶机，你自己用 PVE 建，平台负责把它接进哪张桥。责任分开
- **不装业务软件**。server 上跑 nginx、client 上装 nmap、analysis 上装 ELK，都不管（见 §6.2）
- **不做操作型 CLI**。只有 `serve` / `doctor` / `apply` 三条命令（§0）
- 不做多租户 SaaS、不做计费、不做镜像仓库
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
                    │  · SQLite: desired / observed / runs     │
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
| desired | 用户写的 `lab.yaml`（原样 + 解析后模型） | SQLite `zones.spec` + 文件 |
| observed | 每次 probe 拿到的实际状态（CT 配置、桥成员、CT 内版本） | 内存 + `resources.last_seen` |
| applied | 每个步骤最后一次成功时的输入哈希 | SQLite `steps` |

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
- 跨 zone 共享资源（被引用的父内网桥、被复用的 analysis）→ 加引用计数，删除时检查

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
4. 共享 `analysis` 仍被其他 zone 的 `mirrors` 引用 → 拒绝，先删连线

`retire` 与 `destroy` 的区别只有一个但很关键：**retire 保留 IP 和 VMID 的占用**。否则一下轮重建，同样的 `main-server1` 可能拿到 `10.3.3.14`，你笔记里的地址就全废了。

---

## 5. 数据模型

### 5.1 命名规则（统一，可推导）

| 对象 | 规则 | 示例 |
|---|---|---|
| 网络单元 zone | 小写字母数字，≤10 | `main`、`suba`、`subb` |
| 主机名 | `{zone}-{role}{n}` | `main-gateway`、`suba-gateway`、`main-client1` |
| 内网桥 | `{zone}-inner` | `main-inner`、`suba-inner` |
| 镜像桥 | `mirr`（全局一张，纯二层） | `mirr` |
| 桥名 | 同上，**≤15 字符**（内核 IFNAMSIZ） | `suba-inner`(10) ✅ `longzonename-inner`(18) ❌ |
| 镜像实例 | `labmirror@{bridge}` | `labmirror@main-inner` |
| 共享分析机 | `analysis{n}`（不带 zone 前缀） | `analysis1` |
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
  enabled: true                    # 默认开：只发 ULA + NAT66，不给 GUA
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

# ============ 分析机：全局资源，不属于任何 zone ============
analysis:
  - name: main-analysis            # 全局唯一，mirrors 用它引用
    storage: local-lvm
    egress: wan                    # wan | none | <zone>  → 第二块口接哪
    tools: [tcpdump]               # tcpdump | tshark（可选装）

# ============ 流量镜像：源桥 → 分析机，多源可投同一台 ============
mirrors:
  - from: main-inner
    to: main-analysis
  - from: suba-inner
    to: main-analysis              # 多个源 → 同一台 analysis，不冲突
```

字段语义，逐条说清：

- `uplink: wan | <zone>`：`<zone>` 等价于 `<zone>-inner`。网关的第一块网卡接上联、第二块接自己的内网，这个顺序是**固定的**（nftables 的 `define WAN/LAN` 依赖它，`ports` 表用 `ord` 固定住）。
- `uplink: <zone>` 时，网关 WAN 的 IP 从**被引用 zone 的 `inner.cidr`** 里分配，网关指向该 zone 的网关地址。不需要你手写。
- `inner.cidr` 必填；`inner: null` 表示纯二层单元（不建网关、不分配地址），给"只挂几台机器互打"的场景留口子。
- `node`：zone 固定在一个 PVE 节点上。桥是 per-node 资源，跨节点要 SDN/VXLAN，明确不做。
- `gateway.wan`：不写就继承全局 `wan.mode`；写 `{mode: static, ip, gw}` 或 `{mode: dhcp}` 覆盖。DHCP 时平台等租约并回读地址入库。
- `inner.ipv6`：`auto` 从全局 `ula_prefix` 派生 `/64`；`false` 表示这个 zone 不配 v6（即使全局开了）。
- **`analysis[].egress`**：`wan` = 第二块口直接挂外网桥（拿管理网地址，方便 SSH 和传报告，但也就暴露在管理网）；`none` = 只有 `mirr` 一块口，完全离线（数据靠 PVE 的 `pct pull` 或快照取出）；`<zone>` = 挂到某个 zone 的内网、通过该 zone 的网关出网（能联网但受网关策略约束）。默认 `wan`。
- **`mirrors`** 是独立的连线表，不属于任何 zone。所以"给 suba 也抓包"只是在列表里加一行，不需要动 suba 的定义。
- `hosts[].role`：`server | client | target`，只决定"配不配 gw、装不装基础工具"，**不决定装什么业务**（见 §6.2）。
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
| 唯一性 | 按 network 唯一；不同 zone 不同网段，天然隔离 |
| CIDR 重叠 | **全局检测**：任意两个 `inner.cidr` 不得重叠（`ipaddress.overlaps`，标准库） |
| `mirr` / `wan` | 不参与分配（前者无 IP，后者由 DHCP 或你指定） |
| 持久化 | `ip_allocations(network, host, ip)`，**只增不重排** |

vmid 同理：`vmid_range` 内取最小空闲，写 `vmid_allocations`。**不用 PVE 的自动分配**——它会在你删掉 1001 后把 1001 给别人，快照名、镜像口、你的笔记就全对不上了。

### 5.6 镜像接线

先把 `analysis` 的归属定清楚：**它是"观测节点"，不属于任何 zone**。

- 一张全局 `mirr` 桥：纯二层、无 IP、不挂网关（这条是你明确要求的："只连接 analysis，gateway 不连"）
- analysis 一块口挂 `mirr`（无 IP），另一块口**可选**挂 `wan`（让它自己能 SSH、能拉包、能出报告）。这个"可选"就是你说的"analysis 可能还会挂外网，可选配"
- analysis 可以多台，都挂同一张 `mirr` 桥

`mirrors` 里每一条，对应宿主机上一条 `labmirror@<bridge>` 实例：

```
SRC_BRIDGE=main-inner        # 源桥：每个成员口的入向
DST_IF=veth1002i1            # 落点：analysis 在 mirr 桥上的宿主侧口
```

自动化四步：

1. 建 analysis 时记下 vmid，在 `mirr` 桥上按 `veth<vmid>i*` 找宿主侧口
2. 写 `/etc/labmirror/<instance>.conf`
3. `systemctl enable --now labmirror@main-inner`
4. `DST_IF` 留空则让 `labmirror` 自己按 `mirr` 桥探测唯一成员口

两条推论：

- **多源 → 同一台 analysis 不冲突**。三条实例各管一张源桥，DST 都指向同一个 veth，tc 的 mirred 允许多条 filter 指向同一目标口，互不干扰。所以 `main`、`suba`、`subc` 可以共用一台 `analysis1`。
- **analysis 重建会静默打断镜像**。vmid 变了，veth 名从 `veth1002i1` 变成 `veth1005i1`，旧 conf 里的 `DST_IF` 指向一个不存在的口，`labmirror` 规则挂不上但**不报错**。所以这一步不能靠输入哈希跳过，每次 `apply` / `doctor` 都要重算并 rewrite，然后 `restart`。

`labmirror` 的最小改造（约 40 行）：

- 读 `/etc/labmirror/%i.conf` 而不是写死的 `/etc/default/labmirror`
- 改成 `labmirror@.service` 模板单元，`%i` 即实例名（`main-inner`）
- `DST_IF` 探测改为"按 `MIRR_BRIDGE` 找唯一成员口"，不再依赖 vmid 前缀——analysis 换了也能自愈

### 5.7 数据库表

```sql
zones(id, name UK, uplink, inner_bridge UK, inner_cidr, spec_json, created_at)
networks(id, name UK, bridge UK, cidr, kind)            -- kind: inner | mirr | wan
hosts(id, zone_id NULL, name UK, role, vmid UK, state)  -- analysis 的 zone_id 为 NULL
ports(id, host_id, iface, network_id, ip, gw, ord)      -- ord 直接映射 PVE 的 net0/net1
ip_allocations(network_id, host, ip, UK(network_id, ip))
vmid_allocations(vmid UK, zone, host)
mirrors(id, src_network_id, dst_host_id, instance UK, enabled)
runs(id, zone_id NULL, status, started_at, ended_at, trigger, summary_json)
steps(run_id, step_key, status, input_hash, exit_code, log_path, detail_json)
users(id, name UK, pass_hash, totp_secret)
audit(id, ts, user, action, target, detail_json)
```

三个设计说明：

- **`spec_json` 是输入，`networks/hosts/ports` 是展开后的执行视图**。前者是你写的 YAML，后者的作用是"能查询、能分配、能给前端画图"。两者由 apply 同步，类似 Terraform 的 config 与 state。不要指望只留一份——只留 YAML 就没法高效查"10.3.3.11 是谁"。
- **`ports` 表必须有 `ord`**。`eth0` 是 WAN 还是 LAN，决定了 `nftables.conf` 里的 `define WAN/LAN` 对不对。这个顺序不能靠"创建时碰巧对"，要显式存、显式校验。
- **`hosts.zone_id` 允许为 NULL**，专门给共享 `analysis` 用。这样它不会被 zone 的级联删除带走（§4.6 的引用计数就查这张表）。

### 5.8 IPv6（默认开，ULA + NAT66，不给 GUA）

默认开。理由不是"v6 普及率高"，而是更硬的一条：**默认关 = 这条代码路径永远没人测**。等到某天有人打开 `ipv6: true`，面对的是从未跑过的 nft 规则和从未配过的 `ip -6 route`，第一次必然炸。默认开，双栈从第一天就在每条回归测试里。

但"默认开"不等于"随便开"，三条硬约束：

| 约束 | 做法 | 为什么 |
|---|---|---|
| **只给 ULA，不给 GUA** | 全局 `ula_prefix: fdxx:xxxx:xxxx::/48`，安装时随机生成并存 DB | 如果直接从上联拿运营商 GUA，内网机器能凭 v6 地址绕过网关直连外网，NAT 日志全失效。"靶机不联网"这条保证会当场破功 |
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

`nat: false` 只做转发、不做转换——用于上游已经把整个网段路由过来的情形（宿主加了静态路由，或 v6 拿到的是真正可分发的前缀）。这时内网地址在外部可见，靶机可以直接从实验网外部被访问，这是"要抓外部回连"的实验必需的。

**默认值必须是 `true`。** 第一版实现里这个字段被解析、被展示在预览里，却从来没传到执行器——`gateway_script` 的 `nat44` 参数是硬编码的 `true`。也就是说 `nat: false` 是一个纯装饰字段，写了没反应。现在的做法是：`ResolvedZone.nat` → `StepKind::Net { nat }` → `gateway_script(..., nat)`，三步都带值，并且有回归测试盯着"`nat: false` 时脚本里不能出现任何 `masquerade`"。

同时改掉的默认值：`nat` 原先 serde 默认 `false`，与"靶机要靠网关出网装包"直接矛盾（不写 `nat` 的实验网会全部断网）。现在默认 `true`。


地址规划：

- `ula_prefix` 安装时随机生成（`fd` + 40 bit 随机），不用固定的 `fd00::/8` 子段，避免两个 lab 撞车
- zone 序号 `i` → `{prefix}:{i}::/64`；`0` 段留给 `mirr`
- 网关 `::1`，主机从 `::10` 起，与 v4 分配规则一一对应
- `mirr` 桥不配 v6（镜像流量是二层复制，与地址无关）
- **不做 SLAAC、不发 RA**：LXC 里 SLAAC 需要上游发 RA，而宿主桥不能发（会把整个二层域带偏）；静态 ULA 确定、可复现，与 v4 模型一致

两个验收陷阱：

1. `ping` 通不代表 v6 通。要显式 `curl -6`、`ping -6`，并 `dig AAAA` 确认外部域名解析结果
2. 上联是双栈时，容器内 `apt` 可能优先走 v6。若 NAT66 没配好，表现是"apt 卡住 30 秒后回退 v4"。`doctor` 里加一项 `curl -6 -m 5` 直连测试，超时直接报错，不要留给用户猜

---

### 5.9 外来成员：你手动建的机器怎么进来

**只认自己建的是对的，但"不认"不等于"看不见"。** 你在 PVE 网页上手动建一台 VM，把网卡挂到 `main-inner` 上——这台机器的流量会立刻出现在桥上，镜像如果漏掉它，你就得到一份缺了主角的抓包。所以平台必须**看见**它，只是不去**管**它。

三个层次，别混：

| 层次 | 含义 | 平台行为 |
|---|---|---|
| **managed** | 平台建的（`hosts` 表里有记录，PVE tag 为 `labyard:zone=<z>`） | 全生命周期：建、改、启停、退役、删除 |
| **observed** | 桥上能看见、但不在 `hosts` 表里 | **只读**：拓扑里显示、镜像自动覆盖、可加进网关封禁列表；不能改配置、不能删 |
| **unknown** | 桥上看见的成员口，PVE 里也没有对应 vmid（手工 `ip link add` 之类） | 只在一个"未识别成员"清单里列出 ifname + MAC，供你判断 |

发现机制（每 30 秒或拓扑刷新时跑一次，只在宿主本机执行两条命令）：

```bash
ip -o link show master main-inner          # 成员口 ifname + MAC + 状态
# 对照 PVE：pct list（LXC）+ qm list（QEMU）
# 命名规则推断归属：veth<vmid>i<idx> → LXC，tap<vmid>i<idx> → QEMU
```

命名规则在 PVE 里是稳定的（LXC 用 `veth`，QEMU 用 `tap`，后缀 `i<序号>` 对应 `net<序号>`），所以能准确反推出"这个口属于 vmid 105"。反推不出来、或者 vmid 在 PVE 里根本不存在，才归为 `unknown`。**这个推断只用于展示，不作为任何写操作的依据**——写之前一定重新查一次。

镜像为什么天然覆盖：`labmirror` 抓的是**桥上每个成员口的 ingress**，不看 vmid。所以外来 VM 一挂上去就被抓到了，不需要额外配置。这也解释了为什么 `DST_IF` 必须自动探测而不是写死 vmid 前缀——同一个机制同时解决了"analysis 重建"和"外来成员"两个问题。

能对它做什么：

- **封**：把它加进网关的封禁列表（按 IP 或 MAC），这在演练里很实用——"这台机器是谁的？先封了再说"
- **标记**：给它起个别名（"同事的 win10"）、涂个颜色，只存在本地 DB，不碰 PVE
- **接管**（可选，后续）：点一下"纳入管理"，平台读它的现有配置、补上 IP 记录，从此当 managed 对待。**但不重建、不格式化**——这条必须写死，否则一次误点就是数据丢失

不能对它做什么：不改网卡、不改 IP、不开机、不删除。它不在 `hosts` 表里，所有写操作路径天然够不到它——这是**结构性保证**，不是靠"记得判断一下"。

一个必须承认的限制：如果外来的是一台 **QEMU VM 且开了 PVE 防火墙**，桥上的成员口会是 `fwln<vmid>i<idx>` + `fwpr<vmid>i<idx>` 一对，`tap` 口本身不在桥上。这时的识别规则要额外匹配这两个前缀，且镜像抓的是 `fwln`（靠内那侧）。`doctor` 会检查是否存在这类口并打印说明。

---

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
| gateway | 全套：依赖、转发、nftables、`labgw`/`labvpn`、恢复 unit | 不建桥、不挂靶机 |
| analysis | 抓包基础件（`tcpdump`，`tshark` 可选）、抓包目录、`mirr` 接口 | 不装 ELK / Suricata / Zeek，不写规则、不管分析 |
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

- **AppArmor 4**：嵌套容器（LXC 里跑 Docker）有已知问题（bug #6538）。你在靶机里装 Docker 自己负责，但报错会很怪，先知道这条
- **PVE 9 的 `pve-test` 仓库改名**（原 `pve-test` 沿用）：装依赖时别引用旧源名

CLI 侧没有变化：`pct`/`qm`/`pvesh` 的参数在 9.x 内稳定。**但"命令能跑"不等于"JSON 结构没变"**——`pvesh --output-format json` 序列化的就是 API 处理器返回的那份结构，字段跟着 API 走。所以 `doctor` 第一件事是 `pvesh get /version`，第二件是拿真实的 `/cluster/resources`、`/nodes/{n}/storage`、`/nodes/{n}/aplinfo` 各调一次，字段缺失就当场报，而不是等到 `apply` 跑到一半 `KeyError`。

不支持 8.x 的取舍：**8.x 上直接拒绝启动**，报"labyard 需要 PVE 9.0+"。写兼容层意味着每个端点都要两套参数、每个测试要跑两遍，而 8.x 会在 2026 年内逐步退场（8.4 支持到 2026 年底）。真要在 8.x 上跑，用旧目录那套手工脚本。

### 6.8 与 PVE 交互的两个硬事实

**一、不是需要凭据，而是需要 root + 宿主本机。**

本地 root 调 `pct`/`pvesh` **不过 ACL**，权限是全有或全无。这是明确接受的代价：平台本来就要写 `/etc/pve/lxc/*.conf`、改 `/etc/network/interfaces`、调 `systemctl`，这几件事任何 ACL 都给不了。审计仍然有——`/var/log/pve/tasks` 记录每次 CLI 调用的任务。

**为什么不用"最小权限 API Token"**：它看着更安全，实际是反的。token 落在 `/etc/labyard/config.toml`（0600），root 可读；一旦 labyard 进程被拿（它有 Web 界面和上传功能），token 是**可移植**的凭据——能拿到别的机器上直接打 8006，而"本机 root"给不出这个能力。所以引入 token 只增加复杂度（凭证轮换、ACL 漏授诊断、`verify_tls=false`），换不来安全收益。真想收紧，正确做法是收紧 §8.4 的暴露面，不是给本地进程发令牌。

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
| 语言 | **Rust** | 1.100 / edition 2021 | 用户选定（Python → Go → Rust，决策记录见 §7.5.1）。**实测空闲 4.7 MB / 峰值 6.5 MB**；单二进制 2.6 MB；无 GC，长期内存曲线平坦 | Go（峰值 18.3 MB、9.9 MB 二进制、编译 1–2 s，但 GC 让 RSS 停在峰值水位）、Python（初版方案，对比数据保留） |
| 命令入口 | **clap 4** | 4.5 | **只有 3 条命令**（`serve`/`doctor`/`apply`），不做操作型 CLI；derive 宏生成 help 与补全 | 手写 `std::env::args` 匹配（3 条命令够用，但补全要自己写） |
| HTTP + 路由 | **axum 0.8** | 0.8 | `Router` 原生支持 `/runs/{id}` 路径参数与 extractor，与 tokio 同一 runtime。**OpenAPI：当前手写 `openapi.json`**（端点只有 10 个，手写 20 行且不会漂移）；端点增长到 30+ 时再换 utoipa | actix-web（actor 模型自成一套）、上 utoipa 再手写一遍（当前规模不划算） |
| PVE 通道 | **原生 CLI**：`pct` / `qm` / `pvesh` | PVE 自带 | 本地 root 直连，无凭证、无 TLS、无 task 轮询（§6.3） | 任何 API 客户端（`proxmoxer` / `go-proxmox` 等）——要建 token + ACL，换不来安全，见 §6.8 一 |
| 校验 | **serde** + 手写语义检查 | — | serde 反序列化即完成类型校验（缺字段/类型错直接拒绝，`deny_unknown_fields` 拦住拼错的字段）；跨字段规则（IP 在网段内、vmid 不重复）在 `resolve()` 里手写并单测 | `validator` crate（derive 表达不了"IP 属于网段"这类规则） |
| DB | **rusqlite**（`bundled`） | 0.32 | 10 张表、查询几乎只有"按 id 取一行"；同步 API 用 `spawn_blocking` 包一层即可，不必上 async SQL 的复杂度。`bundled` 编 C 源码，交叉编译靠 `cross` | sqlx（编译期要连库、依赖面大）、Diesel（ORM 黑盒）、纯 Rust 嵌入式 KV（丢掉 SQL 与事务） |
| 配置 | **serde_yaml_ng** | 0.10 | YAML 是 IaC 惯例；`serde_yaml` 已于 2024 归档，`serde_yaml_ng` 是活跃的继任 fork（API 兼容、已修补原始缺陷） | `config` crate（为读 5 个字段引一整套）、`saphyr`（更底层，要自己搭反序列化层） |
| 任务 | 自建薄 runner（tokio task + `CancellationToken` + `runs`/`steps` 表） | — | 无 broker、可断点续跑、状态可查；**`CancellationToken` 树把取消传到子进程** | 需要 Redis 的任务队列、定时器库（不是执行器） |
| 锁 | **fs2** | 0.4 | 跨进程 `flock`，异常路径由库处理 | 手写 `libc::flock`（要处理 EINTR 与异常路径） |
| 拓扑排序 | 手写 **Kahn**（约 30 行） | — | 步骤依赖是小图，40 年前的标准算法；不引数值库 | gonum/graph（为 40 行逻辑引一个大库） |
| 日志流 | **SSE**（`http.Flusher`） | 标准库 | 单向、浏览器自带重连、`Last-Event-ID` 续传；`Flush()` 逐条推 | WebSocket（双向不需要，还要处理心跳） |
| 认证 | **`scrypt`**（RustCrypto）+ `hmac`/`sha2` 签名 cookie | 0.11 | scrypt 是合格 KDF；cookie 签名 40 行、无第三方 | JWT（前端存 token 是 XSS 放大器）、`cookie` crate 的 private jar（多一层抽象换 40 行） |
| TLS | **`openssl req`** 生成自签 + **`axum-server` 的 rustls** | — | openssl 本就在系统里；rustls 是纯 Rust 实现，不链接 OpenSSL，没有版本与 CVE 牵连 | 内嵌证书生成（要给二进制塞密钥生成逻辑）、`native-tls`（又回到 OpenSSL 依赖） |
| 结构化日志 | **tracing** + **tracing-subscriber** | 0.1 / 0.3 | JSON layer 输出结构化行；span 能自动带上 `run_id` 与 `step_key`，这正是 run 日志要的东西 | `log` + `env_logger`（没有 span，手写上下文） |
| ttyd 反代 | **axum 的 WebSocket**（`ws` feature） | 内置 | 下游用 `WebSocketUpgrade`，上游用 `tokio-tungstenite` 连，双向搬运。**当前版本未实现**（见 §7.4 的状态标注） | 手写 ws 帧解析（没必要） |
| 测试 | 标准库 **`testing`** + `testdata/` 回放 | — | 所有 PVE 交互走一个可替换的 `Runner` 接口；注入"回放 fixture 的假 `pct`/`pvesh`"，不碰真机也能测引擎 | testify（表驱动断言够用）、ginkgo（BDD 不降低本项目风险） |

### 7.2 前端

| 关注点 | 选择 | 理由 | 否决 |
|---|---|---|---|
| 框架 | **React 19 + TypeScript + Vite** | 用户指定；Vite 启动秒级 | Next.js（不需要 SSR） |
| UI | **Ant Design 5** | 管理后台事实标准，Table/Form/Descriptions/Tag 全都现成 | MUI（偏 C 端视觉）、shadcn（要自己拼装） |
| 数据层 | **TanStack Query** | 缓存、重试、失效重取、乐观更新 | 手写 `useEffect` + fetch |
| 路由 | **React Router 7** | 成熟 | 手写 hash 路由 |
| 拓扑图 | **@xyflow/react (React Flow)** | 节点/连线/缩放/拖拽全有，正是"网络-主机"图要的 | d3 手绘（两天起步）、Cytoscape（偏图论） |
| 图表 | **Recharts** | 流量曲线、资源占用 | Chart.js（要自己包 React） |
| 日志终端 | **xterm.js** | 真实终端观感、支持 ANSI 色 | `<pre>`（44 项断言全是彩色的） |
| 表单 | AntD Form + 手写 TS 类型（`web/src/api.ts`） | 类型与后端 struct 一一对应，改动时两侧一起改 | 引入代码生成（当前 10 个端点不值得） |
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

需求是"单独窗口打开、能同时开好几个、要能传文件"。这三条直接排除"在页面里塞一个 xterm 组件"——嵌入式终端在切换标签页时会断线、多开会抢焦点，而独立标签页天然隔离。

| 层 | 选择 | 版本/形态 | 关键理由 |
|---|---|---|---|
| 终端服务 | **ttyd** | 1.7.7 **静态二进制**（1.3 MB） | C 写的，libuv + libwebsockets，前端 xterm.js + WebGL2；**CJK/输入法完整支持**（GoTTY 在这块不行，中文输入会乱）；自带 ZMODEM/trzsz；一个进程多会话 |
| 终端接入 | **反代 + 一次性校验** | labyard 内 `~90 行` WebSocket 转发 | 见下，这是唯一自写的部分 |
| 终端内传文件 | **ZMODEM（lrzsz）** | 容器内 `apt install lrzsz`（约 300 KB） | ttyd 前端自带 zmodem 解码，容器侧只需要标准的 `rz`/`sz`，不装任何自研客户端 |
| 大文件 / 目录 | **`pct push` / `pct pull`** | PVE 自带命令 | 走 HTTP 面板而非终端，不受终端窗口和 zmodem 吞吐限制；目录用 tar 流 |

> **实现状态（2026-11）**：ttyd 反代与文件面板**尚未实现**。当前版本提供核心链路（预览 → 执行 → SSE 日志），终端留到真机验证阶段再做——它依赖 ttyd 的 header 信任行为，而那只能在真机上确认。

**ttyd 不在 Debian 13 的仓库里**（Debian 只从 forky/sid 才开始打包），所以 `install.sh` 从 GitHub Release 下 `ttyd.<arch>` 静态二进制装到 `/usr/local/bin`。这和"少一层依赖"一致：一个 1.3 MB 的静态文件，没有 apt 源、没有 .so 依赖、没有版本冲突。

启动方式（单个实例，`-p 0` 不要）：

```bash
ttyd -i 127.0.0.1 -p 7681 -b /tty \
     -H X-Labyard-User \        # 认证交给反代，ttyd 只认这个头
     -O \                       # 校验 Origin，防跨站 WebSocket 劫持
     -W \                       # 可写（不然只能看）
     -a \                       # 允许 URL 参数 ?arg=<vmid>
     -t fontSize=14 -t 'theme={"background":"#1e1e1e"}' \
     /usr/local/lib/labyard/tty-attach
```

`tty-attach` 是一个 20 行的 shell：从 `$1` 取 vmid，**只接受纯数字且必须是 hosts 表里的 managed 主机**（防止有人拿 `?arg=../..` 或任意 vmid 进别人的容器），然后 `exec pct exec "$vmid" -- login -f root`。

反代路径：`/tty/*` → `127.0.0.1:7681/*`，HTTP 与 WebSocket 都转。认证在**反代这一层**做：校验 labyard 的 session cookie，通过后注入 `X-Labyard-User: admin` 再转发。ttyd 绑 `127.0.0.1`，外部伪造不了这个头。

为什么自己写这 90 行而不用 nginx/Caddy：

| 方案 | 代价 |
|---|---|
| 引 nginx | 多一个常驻服务 + 一份要生成的 conf + 证书要共享；而且认证还得回到 labyard 做（nginx 不认识我们的 session），等于照样要写一段 |
| 引 Caddy | 40 MB 二进制，为 90 行转发引入，不成比例 |
| 自写 ws 转发 | 纯字节搬运，无业务逻辑，用 axum 的 `WebSocketUpgrade` + `tokio-tungstenite` 做双向桥接，出错面很小 |

**多窗口**：ttyd 默认每个 WebSocket 连接 fork 一个独立 pty（`-Q` 共享模式我们**不开**），所以开五个标签页就是五个独立 shell，互不干扰。点"终端"时前端 `window.open('/tty/?arg=<vmid>', '_blank')`，用完关标签页即断。

**文件传输的两条路都要，不是二选一**：

- 终端里 `rz` / `sz`：适合"顺手传个脚本"，符合肌肉记忆，但 zmodem 在 vmconsole 之外没有断点续传，几百 MB 会很难受
- 文件面板：上传走 `r.MultipartReader()` 分块流式落 `/var/lib/labyard/tmp/` → `pct push`；下载走 `pct pull` → `http.ServeContent`（自动处理 Range）；目录打 tar 流。**必须设大小上限和临时文件清理定时器**，否则 `/var` 会被悄悄撑满

一个坑要先说：`pct pull` 的源路径是**容器内路径**，要防目录穿越（`../../etc/shadow` 之类）。所有路径参数先 `normpath` 再校验必须以 `/` 开头且不含 `..`，然后在容器内用 `pct exec -- test -f` 确认存在——不要让 `pct pull` 自己报错，它的报错信息会泄露宿主路径。

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

**平台实测（容器内复现，HTTP 模式无 TLS）**：空闲 RSS **6.9 MB**、二进制 **8.0 MB**（含 1.07 MB 前端）、峰值 HWM **38.1 MB**。

**38 MB 这个数字必须解释清楚，否则会被误读成"Rust 也不小"。** 诊断结果：`VmHWM` 在**启动完成时就已经是 38.1 MB**，之后 200 个请求（20 并发，混合 preview 与 state）一个字节都没顶上去。来源是 `scrypt`——`N=2¹⁵, r=8` 的工作内存是 `128·N·r = 32 MB`，且是瞬时的（`scripts/measure.sh` 可复现）：

| 时刻 | RSS | HWM | 说明 |
|---|---|---|---|
| 启动后空闲 | 6.9 MB | 38.1 MB | 启动时生成初始密码，跑了一次 scrypt |
| 200 请求 / 20 并发 | 7.5 MB | 38.1 MB | **HWM 纹丝不动**，只涨 0.7 MB RSS |
| 8 个并发登录 | 7.7 MB | 39.2 MB | 只涨 **1.1 MB**——`hash_gate` 生效的直接证据 |

最后一行是这次新增的关键测量。**8 个并发登录只涨 1.1 MB，说明 32 MB 哈希区是被复用而不是复制的**；若串行化没生效，8 × 32 MB 会把峰值推到 256 MB 以上，`MemoryMax=64M` 必然 OOM。这条不是推断，是量出来的。

所以正确的描述是：**常驻 7 MB，峰值 38 MB，差额 31 MB 是一次性的哈希工作区**。这和 Go 的"峰值由 GC 水位决定"是两回事——它是可预测的、固定的、且只在登录那一瞬间存在。另一个附带结论：glibc 把这块 32 MB 用 `mmap` 直取直还，`free` 后 RSS 立刻回落（压测后 7.5 MB，不是 38 MB），所以 `memory.current` 也不会长期挂在 38 MB。

**这个发现改了三件事**（都是实测推翻了估算）：

1. `MemoryMax` 不能按"常驻 × 2"取。若取 16 MB，第一次登录就会被内核杀掉。**64 MB 是必须的**：32 MB 哈希 + 7 MB 常驻 + 余量（实测峰值 39.2 MB，余量 24 MB）。
2. **密码哈希必须串行**。两个并发登录 = 64 MB 工作区，正好顶穿限制。已实现 `hash_gate`（容量 1）+ `spawn_blocking`，避免卡住 runtime worker。
3. **preview 也要限流**。虽然实测显示它只涨 0.7 MB，但那是 6 台机的规格；规格放大到 50 台机时，解析 + 规划的对象量会线性增长。闸门固定 4。

**Rust 的增量分解**（估算，标注清楚，不冒充测量）：tokio runtime + 调度器 ≈2 MB、axum/hyper ≈2 MB、rustls ≈4–6 MB（含会话缓存与会话票据）、rusqlite + page cache ≈4 MB、serde/serde_json ≈1 MB、SSE 与日志缓冲 ≈4 MB。合计 18–22 MB，与上面的估算区间吻合。**估算只用于排序，最终以真机 `systemctl show labyard -p MemoryCurrent` 为准。**

四条硬性纪律（前三条与语言无关，第四条是 Rust 特有）：

1. **不引 ORM**。10 张表、单用户、查询几乎只有"按 id 取一行"，ORM 省下的那点代码不值得一层黑盒。迁移是编号 SQL 文件 + `schema_version` 表（约 60 行 runner），用 `include_str!` 编进二进制顺序执行。代价说清楚：没有 autogenerate、没有 downgrade；收益是每条 DDL 都看得见、可 review。
2. **禁止无界缓冲**。`Vec<u8>` 收整个 `pct exec` 输出、把 2 GB 镜像读进内存、`read_to_end` 读上传——都和语言无关，都会炸。规则是**上传与下载一律流式 `copy`**，子进程输出逐行转 SSE 且不落内存。
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

#### 7.6.4 三级并发闸

| 层级 | 默认 | 说明 |
|---|---|---|
| 全局并发 run | **2** | 跨 zone 排队；第 3 个起显示"前面还有 N 个"，不是转圈 |
| 单 run 内**重**步骤 | **3** | `pct create`、`apt install`、`pct push` 大文件、快照 |
| 单 run 内**轻**步骤 | **8** | `pct set`、写文件、`systemctl enable` |

外加一条：**同模板的 `pct create` 串行**（解压读同一份模板，并行只会互相拖慢，且放大写峰值）。

轻步骤可以开到 8，因为它们几乎不碰磁盘——`pct set` 只改配置。把轻重混在一个池子里是错的，那样要么轻步骤被拖慢，要么重步骤打爆 IO。

#### 7.6.5 八条限流措施

1. **所有子进程包 `ionice -c2 -n7 nice -n 10`** —— 保证宿主管理面、SSH、PVE 自己不被饿死。这是成本最低、收益最大的一条
2. SSE 背压：`chan []byte` 容量 500，满了丢**中间**行，插一条 `[已省略 N 行]`。丢中间不丢头部，因为报错通常在尾部
3. 单 step 日志落盘上限 8 MB，超出后保留头 2 MB + 尾 2 MB（中间截断并注明）
4. 上传必须流式：`r.MultipartReader()` 分块落临时文件，**禁止 `io.ReadAll(r.Body)`**（那会把整个文件塞进内存并直接顶到 `MemoryMax`）；单文件 ≤ 2 GB。反向同理：`pct pull` 的输出直接 `io.Copy` 到响应体，不落内存
5. 临时文件 TTL 1 小时清理；磁盘剩余 < 10 GB 时直接拒绝上传并说明原因
6. 终端并发 ≤ 8，超出返回 429 + "关掉不用的标签页"
7. 拓扑轮询间隔 ≥ 10 秒；页面 `visibilitychange` 隐藏时**暂停轮询**（这条能砍掉 90% 的无谓 probe）
8. 日志 + DB 总占用 > 5 GB 时自动清理最老的 run 日志并告警

#### 7.6.6 用 systemd 兜底（唯一不依赖代码正确性的措施）

```ini
[Service]
MemoryHigh=40M
MemoryMax=64M
CPUQuota=50%
IOWeight=50
TasksMax=256
```

代码可能写错（比如某天忘了给某个 channel 设容量）。有了这几行，内核会先杀 labyard，而不是拖死宿主——**保宿主优先于保服务**，这是一个管理平台应有的默认值。Rust 没有 GC 可以调，所以只有 `MemoryMax` 这一层。**64 MB 这个值是算出来的，不是拍的**：32 MB（scrypt 工作内存）+ 7 MB（常驻）+ 25 MB 余量。取小了会在第一次登录时被 OOM kill——这是实测发现的，不是推测。

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
| labyard RSS | 14 MB | ~30 MB（实测 18.3 + SSE/终端/文件面板余量） | 64 MB（`MemoryMax`） |
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

```
/                        总览：zone 卡片 + 全局健康（PVE 连通、模板、桥、镜像实例、外来成员数）
/zones/{z}               zone 详情：主机表、端口表、运行态、成员列表
/topology                全局拓扑图（React Flow）：所有 zone + 外来成员串成一张图
/zones/{z}/new           新建 zone 向导：模板/存储/网段/上联 → YAML 预览 → Plan → Apply
/runs                    任务列表（状态、耗时、触发者）
/runs/{id}               任务详情：步骤树 + 实时日志（SSE + xterm）
/zones/{z}/gateway       网关操作：出网、端口映射、VPN、封禁列表
/zones/{z}/hosts/{h}     单机详情：配置、IP、终端、文件、快照、开机/关机
/mirrors                 镜像：源桥 → analysis 的连线表、包计数、启停
/observed                外来成员：桥上看见但平台没管的机器（vmid / MAC / 所在桥 / 动作）
/settings                PVE 连接、apt 源、凭据、TLS、账号
```

**全局拓扑图是主视图**，不是每个 zone 一张。因为 zone 之间的关系（`suba` 挂在 `main-inner` 上）只有放到一张图里才看得出来；单看一个 zone，它和别人的连接就丢了。外来成员在图上用**虚线边框 + 灰色**区分，一眼能看出"这台不是平台建的"。

**终端和文件不在页面内嵌，一律新标签页**：点"终端"→ `window.open('/tty/?arg=<vmid>')`；点"文件"→ 打开该主机的文件页（复用同一套认证，但独立窗口，可以同时开好几台）。原因见 §7.4。

### 8.2 关键交互

**创建向导不是"一堆表单直接建"**，而是三步：

```
① 填参数  → ② 看生成的 YAML（可下载、可回贴）→ ③ 看 Plan 表 → 确认
```

第②步很重要：网页上点出来的东西随时能变成一份 YAML 存进 git 或贴进 issue。这是"网页不绑架你"的保证——**能力上不绑架，入口上仍然只有网页**。

**Plan 表的呈现**：

| 动作 | 对象 | 变化 | 影响 |
|---|---|---|---|
| 新建 | bridge `main-inner` | 10.3.3.0/24 | — |
| 新建 | CT 1001 `main-gateway` | 2c/1G/8G, net0=vmbr0, net1=main-inner | — |
| 新建 | CT 1002 `main-analysis` | 2c/1G/8G, net0=vmbr0, net1=mirr | — |
| 新建 | 镜像 `main-inner → main-analysis` | tc mirred | — |
| 新建 | CT 1003 `suba-gateway` | net0=main-inner(10.3.3.40), net1=suba-inner | 子单元，接上联 |
| 更新 | CT 1004 `main-server1` | memory 512→1024 | 需重启 |
| 跳过 | `provision:main-server1` | 哈希未变 | — |

**运行时操作（出网开关、端口映射、VPN、镜像）由后端直接 `pct exec` 进容器调 `labgw`/`labvpn`，不经过任何容器内 HTTP 服务**。理由：容器内面板要网络可达 + 独立认证 + 一套 CSRF 防护，而 `pct exec` 全都不需要；输出用 `--json` 解析，稳定。这也是"不移植旧 `labpanel`"的技术原因。

### 8.3 API 骨架

```
GET    /api/health                        PVE 连通、版本、能力矩阵
GET    /api/zones
POST   /api/zones                         上传/粘贴 YAML，只保存不执行
GET    /api/zones/{z}
PUT    /api/zones/{z}/spec                改 YAML
POST   /api/zones/{z}/plan                返回动作列表（不落库）
POST   /api/zones/{z}/apply               起 run，返回 run_id
POST   /api/zones/{z}/destroy             需 body 带 zone 名确认
GET    /api/topology                      全局图：zone / bridge / host / mirror 四类节点
GET    /api/runs?zone=
GET    /api/runs/{id}
GET    /api/runs/{id}/events              SSE
POST   /api/runs/{id}/cancel
POST   /api/runs/{id}/retry
GET    /api/zones/{z}/hosts
POST   /api/zones/{z}/hosts/{h}/power     {action: start|stop|shutdown}
POST   /api/zones/{z}/gateway/egress      {on: bool}
GET    /api/zones/{z}/gateway/status
POST   /api/zones/{z}/gateway/pub         {proto,port,to}
POST   /api/zones/{z}/gateway/vpn         {proto, action}
GET    /api/zones/{z}/gateway/vpn/{proto}/client   .conf / .ovpn 下载
GET    /api/mirrors
POST   /api/mirrors                       {from, to}
DELETE /api/mirrors/{id}
POST   /api/zones/{z}/snapshot            {name}
```

`/api/topology` 是前端画图的唯一数据源，返回的是**已解析的图**（节点 + 边），不是原始 YAML。这样前端不需要懂 `uplink: main` 是什么语义——它只负责画。解析逻辑留在 core 一处。

> **实现状态（2026-11）——这张表是设计稿，不是交付清单。** 上面这份 API 面里，当前版本只实现了核心闭环：
>
> | 已实现 | `GET /api/health`、`POST /api/auth/login`、`POST /api/auth/logout`、`GET /api/auth/me`、`GET /api/state`、`GET/PUT /api/spec`（规格持久化）、`POST /api/preview`、`POST /api/apply`、`GET /api/runs/{id}/events`（SSE）、`POST /api/runs/{id}/cancel`、`GET /api/doctor`、`GET /api/openapi.json` |
> |---|---|
> | **未实现** | zone 的多份管理与 CRUD（当前只有一份 `lab.yaml`）、`/api/topology`、`/api/runs` 列表与 `retry`、`/hosts` 与 `power`、`gateway/egress`、`gateway/pub`、`gateway/vpn`、`/mirrors` 的 CRUD、`/snapshot`、destroy |
>
> 未实现的这些**不是遗漏，是刻意的边界**：它们全部依赖真机行为（`pct exec` 进容器调 `labgw` 的输出格式、ttyd 的 header 信任、VPN 的密钥分发），在没有 PVE 9 可测的条件下写出来只能是猜。核心闭环（写 YAML → 看计划 → 幂等执行 → 实时日志）是唯一能自证的部分，所以先把它做扎实。
>
> **规格持久化**落在 `paths.specs/lab.yaml`，不是 SQLite：运维要能 `cat`、能进 git、能手工改回去。写入走"先 parse + resolve 校验，再 tmp + rename 原子替换"——非法 YAML 一律 400 且不碰旧文件。刷新页面从服务端读回，改过未保存时标题旁有 `未保存` 标签。

### 8.4 认证与暴露面

先把最难听的话说在前面：**面板等于 PVE 宿主 root**。它能 `pct exec` 进任何容器、能写 `/etc/pve/lxc/*.conf`、能删任何 CT。任何"方便"的设计都在削这个安全边界，所以默认值必须偏保守：

| 项 | 默认 | 说明 |
|---|---|---|
| 监听 | `0.0.0.0:8443` | 不绑 `127.0.0.1`——你要用浏览器访问，绑本地等于每次都得开 SSH 隧道，实际上所有人都会去改成 `0.0.0.0`，不如一开始就正视 |
| TLS | 自签（首次启动生成，10 年） | 内网够用；要真证书就把 `cert_file`/`key_file` 指到 Caddy 签的路径 |
| 初始账号 | `install.sh` 生成 20 位随机密码，**打印在终端**并写入 `/etc/labyard/initial-password`（0600，首次登录后自动删除） | 不做"首次访问自己注册"——那等于谁能连上端口谁就是管理员 |
| 账号数量 | **只有 `admin` 一个**，不做多用户、不做角色 | 单机实验环境；多用户要配套权限模型、审计归属、密码重置流程，收益不成比例 |
| 密码策略 | 登录后可改；**`scrypt`（`scrypt` crate，RustCrypto，N=2¹⁵/r=8/p=1）** 存储；最短 12 位 | 用 RustCrypto 的 scrypt 而非 argon2，少一个依赖且安全性同级 |
| TOTP | 可选，**totp-rs** | 开了之后 `install.sh` 打印的二维码只显示一次 |
| IP 允许列表 | `install.sh` 探测当前 SSH 客户端 IP，填入 `/32` 或 `/24` | 留空 = 不限制，UI 上用红字提示 |
| 会话 | 标准库 `hmac`/`sha256` 签名 cookie（`HttpOnly` + `SameSite=Strict` + `Secure`），8 小时 | 前端不存 token，避免 XSS 放大 |
| 审计 | 所有写操作进 `audit` 表：谁、何时、改了哪个 zone、run_id、结果 | 只读接口不记，否则表会爆 |
| CSRF | 所有 `POST/PUT/DELETE` 要求 `X-Labyard: 1` 自定义头 | 简单有效；跨站表单发不出自定义头 |

一个必须承认的取舍：**没有做"两个进程、非 root 前端 + root 后端"的拆分**。那种架构更安全，但要自己写一套 IPC、权限校验、重启编排，成本大概等于整个平台的一半，而收益只在"Web 有 RCE 时"这一种场景。单机实验环境、内网、强密码 + allowlist，我选简单的那条路，并且把它写在这里，而不是假装不存在。

### 8.5 页面上每个选项的数据来源

"选的地方多，所以更适合 Web"这句话要落地，就是把每一项都变成**运行时从 PVE 查出来**的下拉框，而不是让用户在 YAML 里手打字符串。这张表是前端表单的规格说明：

| 选项 | 出现在哪 | 数据来源 | 是否记忆上次选择 |
|---|---|---|---|
| 节点 | 建 zone | `pvesh get /nodes` | 单节点自动选；集群弹下拉 |
| **存储** | 建 zone、每台主机可覆盖 | `pvesh get /nodes/{n}/storage`（前端按 `content=rootdir` 过滤） | **不记忆**（§11.4：选错代价太大，强制每次确认） |
| 存储能力标记 | 同上，每个选项后跟标签 | 同上命令的 `type` + 版本判断 | — |
| 模板 | 建 zone、每台主机可覆盖 | `pvesh get /nodes/{n}/aplinfo` 与 `pvesh get /nodes/{n}/storage/{s}/content`（筛 `vztmpl`）合并 | 记忆 |
| 网卡（WAN 桥） | 建 zone | `pvesh get /nodes/{n}/network`，只列 `type=bridge` 且无 IP 冲突提示 | 记忆 |
| 内网网段 | 建 zone | 用户填，实时查冲突（已存在桥、已分配 zone） | 不记忆 |
| 上联 | 建 zone | 现有 zone 列表 + `wan` | 记忆 |
| apt 源 | 建 zone、设置页 | 内置清单；也可填自定义 URL，实时 `HEAD` 探测可达性 | 记忆 |
| SSH 公钥 | 设置页 | 读宿主 `~/.ssh/*.pub`，也可粘贴 | 记忆 |
| 时区 | 设置页 | 内置 `zoneinfo` 列表（标准库，无网络依赖） | 记忆 |
| analysis 的出口 | 新增分析机时 | `wan`（拿管理网地址）/ 某 zone 的 inner（走该 zone 网关）/ `none`（完全离线） | 记忆 |
| 外来成员 | `/observed`、拓扑图 | `ip -o link show master <桥>` + PVE 的 LXC/QEMU 清单（§5.9） | 不记忆（每次实时扫） |
| 终端字号 / 主题 | 终端页 | 浏览器 `localStorage` | 记忆（存前端，不进 DB） |
| VIP 端口范围 | 建 zone | 用户填，查宿主 `ss -ltn` 冲突 | 记忆 |

"记忆"存在 `settings` 表（key-value），不进 YAML——它属于**操作习惯**，不属于**期望状态**。这样换个人用不会互相干扰，清空浏览器也不影响。

**冲突检测必须实时**，不能等到 Apply 才报。输入网段时就查：这张网段在别的 zone 用过吗？桥上已有的 IP 会不会撞？这类错误在表单上标红一句话就能解决，等到 Plan 阶段就变成"我填了半小时，最后告诉我重来"。

---

## 9. 遗留与风险

| # | 风险 | 影响 | 缓解 |
|---|---|---|---|
| 1 | 平台以 root 跑 + Web 暴露 | 拿到面板 = 拿到宿主 | §8.4 全部默认值 + 审计 + allowlist + 自签 TLS |
| 2 | Web 是唯一界面，服务挂了就没法操作 | 实验环境"停在那儿" | `labyard doctor` 不需要 Web；`systemctl restart labyard` 恢复；`apply` 命令可应急；再不济直接点 PVE 页面（桥和 CT 都在，只是没人管状态） |
| 3 | 桥名 15 字符 | 长 zone 名直接失败 | zone 名限 ≤10 + plan 阶段校验 + 给出建议名 |
| 4 | 多层 NAT | 端口映射要逐层配；conntrack 表项按层数翻倍；排障路径变长 | 映射链自动生成；`labyard doctor --trace <src> <dst>` 逐跳 ping/tcpdump（只读，Web 上也有按钮） |
| 5 | analysis 重建后 veth 名变 | 镜像静默失效（规则挂不上但不报错） | 每次 apply 重算 `DST_IF`；doctor 校验 filter 数 = 成员口数 |
| 6 | apt 源不可达 | provision 失败 | 源可切（tuna/ustc/官方/自定义）；`--no-install-recommends`；失败原样回传 |
| 7 | 半成品状态 | 重跑不确定 | §4.3 reconcile；不自动删重建 |
| 8 | 你手动建的 VM 接进内网 | 镜像漏掉它，抓包文件缺主角 | §5.9 observed 机制：桥上看见、拓扑显示、镜像自动覆盖、可封禁，但**不能改不能删** |
| 8b | QEMU 开了 PVE 防火墙时成员口是 `fwln`/`fwpr` | 按 `tap` 前缀识别不到 | 识别规则含 `fwln`/`fwpr`；`doctor` 检测到这类口时打印说明 |
| 9 | 快照与镜像口 | 回滚后 veth 变化 | 回滚后重跑 `mirror` 步骤 |
| 10 | 平台自身升级 | DB schema 变 | 编号 SQL 迁移 + `schema_version` 表；升级前用 `VACUUM INTO` 做一致性备份（保留 7 份）——**不依赖外部的 `sqlite3` 命令**，宿主上未必装了 |
| 11 | 关了 IPv6 但没显式 drop | 主机用 link-local / 自配 ULA 绕过 v4 防火墙，封禁形同虚设 | 默认开；即便是 `false` 也在 forward 链显式 drop v6，`doctor` 用 `nft list ruleset` 校验这条规则在 |
| 12 | 多节点下桥不存在于目标节点 | CT 建好却起不来，或起在错误的二层域 | **首版只支持单节点**；`doctor` 检测到集群直接警告不阻止（单节点功能仍可用），建 zone 时强制绑定当前节点 |
| 13 | PVE 9 AppArmor 4 + 嵌套容器 | 靶机里跑 Docker 报错怪异 | README 写明已知问题（bug #6538）；平台不碰业务软件，但报错信息里给这条链接 |
| 14 | 厚 LVM 快照是 tech preview | 以为有快照，实际不可靠 | `doctor` 打黄标；创建快照时二次确认 |
| 15 | 并发 provision 打满磁盘 IO | **全宿主机卡死**，含 PVE 管理面与 SSH（内存/CPU 不足不会这样） | 三级并发闸（§7.6.4）+ 重步骤并发 3 + `ionice -c2 -n7 nice -n 10` + 同模板 `pct create` 串行 |
| 16 | 高频 probe 打慢 pmxcfs | `/etc/pve` 是单线程 FUSE，20 台 N+1 查询会拖慢整个节点 | 一律用 `pvesh get /cluster/resources --type vm` 批量取状态；配置详情缓存 10 秒；拓扑轮询 ≥10 秒且页面隐藏时暂停 |
| 17 | 日志与临时文件撑满 `/var` | 宿主写不进去，PVE 自己开始报错 | 单 step 日志 8 MB 上限、临时文件 TTL 1 小时、总量 5 GB 触发清理、`systemd MemoryMax` 兜底 |
| 18 | 一次 apply 跑到一半资源不足 | 半成品状态要手工收拾，比拒绝更贵 | apply 前预检内存/磁盘/load，不满足直接拒绝并报出差额（§7.6.7） |
| 19 | CLI 输出不是兼容契约（改用命令直连的代价） | PVE 小版本改字段 → `KeyError`，且 `pct` 文本格式比 API 更容易变 | 只吃 `--output-format json`，不解析表格；`doctor` 校验关键字段存在；`pve/` 层集中所有命令拼接，一处可改 |
| 20 | 每次命令 fork/exec 约 100–200 ms | 误在循环里逐台调用 → apply 变慢，且频繁唤醒 pmxcfs | 读一律批量（一条 `pvesh get /cluster/resources`）；`pct config` 结果缓存 10 秒；review 时禁止"循环里起子进程" |

---

## 10. 交付范围与实现顺序

**先纠正我自己一个错误**：上一版把"多层嵌套"排成了第二阶段，这是错的。`uplink` 从第一行代码起就是递归的——`uplink: wan` 和 `uplink: suba` 走的是同一条代码路径，只差一次查表。"单层"只是 YAML 里只写了一个 zone，不是代码里的一个分支。把它排成阶段，等于把同一个函数的两种输入说成两个功能。

同理，拓扑图、终端、文件、外来成员不是"增强项"。界面缺了这些，就是没做完。

### 10.1 完成的标准（一张清单，不分版本）

| 域 | 必须全部可用 |
|---|---|
| 建 | zone（任意层嵌套）、桥、LXC、analysis、网络接入（v4 + v6 双栈） |
| 配 | 网关全套（转发 / nft / 出网 / 端口映射 / VPN / 重启恢复）、bootstrap（IP·gw·DNS·SSH·apt·时区·lrzsz） |
| 看 | 全局拓扑、zone 详情、单机详情、运行态、外来成员、任务日志 |
| 用 | Web 终端（独立窗口、多开）、文件上传下载（终端内 + 面板）、快照 |
| 管 | stop / retire / revive / destroy、审计、doctor |
| 改 | 改 spec 重 apply、平台自身升级迁移 |

少任何一项都算没完成。**没有"v0.1 只有单层"这种东西。**

### 10.2 实现顺序（技术依赖，不是产品阶段）

| 步 | 内容 | 为什么必须在这 |
|---|---|---|
| 0 | 仓库骨架：`Cargo.toml`、`rust-toolchain.toml`、`Makefile`（`web` → `build` 依赖链）、`Runner` trait + `testdata/` 回放 | 后面每一步都要能 `cargo test`，骨架不定就会各自长歪 |
| 1 | `src/pve/` 封装 + `doctor` | 连不通、能力不知道，后面全是猜 |
| 2 | `src/core/schema` + `planner` | 纯函数，能在 fixture 上完整单测，不需要集群 |
| 3 | `src/core/runner` + `src/resources/`（bridge / ct / bootstrap / mirror / observed） | 引擎成型；此时 `apply` 已能在真机建出东西 |
| 4 | `src/api/` + SSE + 认证 + 反代 | 网页能登、能看、能点 |
| 5 | 前端：拓扑与详情 → 向导 → 终端与文件 | 前端要有真数据才画得对 |
| 6 | netns 回归 + 真机验收 | 见 10.3 |

每完成一步都是**可运行**的（能起服务、能登录、能看状态），但中间状态不发布、不打 tag。你可以随时 `git pull` 跑起来看一眼，只是功能不全。

### 10.3 验收（这一步绕不过去）

- **第二遍 0 变更**：同一份 spec 连跑两次，第二次 Plan 全 `noop`，按钮直接变灰
- **多层同时存在**：`main` + `suba`(接 main) + `subb`(接 suba) + `subc`(接 main) 四种接法并存，各自能出网，镜像流量落同一台 analysis
- **双栈**：v6 开（默认）与显式关两组用例都过，且关掉时 forward 链的 v6 drop 规则确实在
- **外来成员**：手动建一台 VM 挂到 `main-inner`，30 秒内出现在拓扑上，且它的流量进了 analysis 的抓包文件
- **终端与文件**：网页开两个终端窗口同时用不互踢；`rz`/`sz` 通；面板上传下载通

一个必须说的现实约束：**我没有 PVE 9 环境**。这台 Mac 上只有 OrbStack（普通 VM，不是 PVE）。所以 1–5 步在"假 Runner"上开发——把真实 `pvesh`/`pct` 的输出录成 fixture，回放给引擎，第 6 步必须你在真机上跑。**回放全绿不等于真机通过**——README 的"已验证 / 未验证"两栏就是为这个留的，不会拿假数据冒充。

---

## 11. 已定与待定

### 11.1 已按你的回答定下

| 问题 | 结论 |
|---|---|
| 镜像网职责 | **只做流量镜像**。全局一张 `mirr` 纯二层桥，无 IP、不挂网关，只挂 analysis。gateway 只有 WAN + LAN 两块口 |
| analysis 外网 | **三选一**：`egress: wan`（默认，拿管理网地址，方便 SSH/传报告）/ `<zone>`（走该 zone 网关，受策略约束）/ `none`（完全离线） |
| 靶机 | **只做 LXC**。VM 由你在 PVE 自建；建好后平台**能看见**（§5.9 observed）、镜像能覆盖、能封禁，但不建、不配、不纳管 |
| 凭据 | **创建时选**：`key` / `password` / `key-password`，可在 defaults / zone / host 三层逐级覆盖 |
| 结构 | 不是"单层/双层"，是**同构 zone 的自由组合**。`uplink` 指向 `wan` 或任意 zone，串几层都行 |
| 平台边界 | 只管网络环境与基础设施；业务软件、VM、具体服务一律不管，责任分开 |

### 11.2 本轮新增的决定

| 问题 | 结论 |
|---|---|
| CPU 架构影响吗 | 不影响设计。模板在创建时从 `pveam available` 选，平台不硬编码名字。真影响的是 PVE 版本、单/多节点、存储快照能力——全部交给 `doctor` 探测（§6.6） |
| `wan` 模式 | 用户选。全局 `wan.mode` 取 `dhcp` 或 `static`，逐 zone 可覆盖；DHCP 时回读地址入库 |
| 删除语义 | 三级：`stop` / `retire`（保留磁盘快照与分配）/ `destroy`（全清，含快照，需输 zone 名）。桥只在无引用时删（§4.6） |
| IPv6 | **默认开**。只发 ULA + NAT66，不发 GUA；`target` 角色在 v6 上同样不配 default route。关掉时必须在 forward 链显式 drop（§5.8） |
| 名称与许可 | 项目名 `labyard`；许可证 Apache-2.0（专利授权；只通过 subprocess 调 PVE 的 CLI，不链接其代码，不传染 AGPL） |
| PVE 基线 | **9.x / 单节点**。低于 9.0 拒绝启动，不做 8.x 兼容层；集群只警告不阻止（§6.7） |
| PVE 交互方式 | **命令直连**：`pct` / `qm` / `pvesh`，不用 HTTP API 客户端、不用 token、不配 ACL。读走 `pvesh --output-format json`，写走原生命令（§6.3）。原因：exec 与 conf 直写 API 本来就做不到，用 API 只会多一条通道 |
| 后端语言 | **Rust 1.100**（Python → Go → Rust，见 §7.5.1）。直接收益：峰值 6.5 MB、2.6 MB 单二进制、无 GC 抖动、长期内存曲线平坦。代价约 +1.5～1.8 倍工期，集中在三层配置合并、异步子进程生命周期、utoipa 注解 |
| 交付形态 | **单二进制**：`rust-embed` 打进前端与 `assets/`，宿主只需 `scp` + 一个 systemd 单元，不装运行时、不建 venv（§7.3） |
| 界面形态 | **Web 唯一**。不做操作型 CLI，只留 `serve` / `doctor` / `apply`（给测试）三条（§0） |
| 账号 | **单管理员** `admin` + 可选 TOTP。不做多用户、不做角色（§8.4） |
| Web 终端 | **ttyd** 1.7.7 静态二进制（Debian 13 仓库没有）+ 反代；终端内 `rz`/`sz`（lrzsz），大文件走 `pct push/pull`；**独立标签页、可多开**（§7.4） |
| 外来成员 | 桥上看得见的全部列出（observed）：拓扑显示、镜像自动覆盖、可封禁，**不可改不可删**（§5.9） |
| 交付范围 | **不设阶段**。多层嵌套、全局拓扑、完整页面、终端、文件都属于"做完"的定义（§10） |

### 11.3 仍按默认执行，你不反对就这么定

- `vmid_range: 1000-1099`；inner 从 `.10` 起分配，`.1` 归网关
- apt 源默认 `tuna`；模板列表从 `pveam available` 实时拉，不硬编码版本
- 容器内应急面板（旧的 `labpanel`）**不移植**——Web 是唯一入口，再塞一个容器内面板就是两套认证、两套审计
- 单节点上限：12 张桥、单 zone 32 台主机——超了先报错，别等 PVE 自己变慢
- 所有写操作进审计表；面板监听 `0.0.0.0:8443` + 自签 TLS + 初始随机密码（§8.4）
- 存储、模板、网卡、apt 源、凭据全部在页面上选，不在配置里猜

### 11.4 这一轮我替你做的三个判断（有异议就说）

| 判断 | 我的选择 | 反方理由 | 为什么还是这么定 |
|---|---|---|---|
| 要不要 CLI | 只留 3 条命令 | "脚本化/CICD 需要" | 单机单用户场景，CI 跑不到你的 PVE；真需要批量，Web 的一次 apply 就是批量 |
| IPv6 默认 | 开 | "实验室不需要 v6，默认关更简单" | 默认关 = 这段代码永远不被测；等要用时才开，必然炸 |
| 存储默认 | 不设默认，**第一次必须手选** | "给个默认值体验更顺" | 存储选错（选了不支持快照的 `lvm`）会在两个月后才报应，不如建 zone 时就撞一次墙 |

### 11.5 仍然开着的问题

按"是否需要你表态"分三类，都不阻塞开工。

**A. 原来想问你，后来发现不用问（`doctor` 能自己扫）**

| 原问题 | 现在怎么做 |
|---|---|
| 宿主上哪些 CIDR / VMID 已被占用 | `doctor` 读 `ip -o addr`、`pct list`、`qm list`，算出已用网段与 vmid，建 zone 时在页面上标红冲突项。不靠你手填 |
| 宿主磁盘是 SATA SSD 还是 NVMe | `doctor` 做一次 `dd` 顺序写测带宽（10 秒、只写 256 MB 临时文件），据此给并发建议。**默认 3 是保守值，实测更快就自动上调** |
| CPU 架构 / 模板名 | 从 `pveam available` 实时拉，不猜 |

**B. 真需要你表态的三条（偏好，不是技术）**

| 问题 | 我的默认 | 另一种选择 |
|---|---|---|
| apply 时资源不够（内存/磁盘/load 超阈值） | **直接拒绝**并报差额 | 硬着头皮跑，跑到哪算哪——半成品要你手工收拾，不推荐 |
| 日志与 `retired` 机器的保留期 | 日志 30 天或 5 GB 先到者；`retired` **无限期保留**（继续占 IP 与 VMID） | 日志 7 天；`retired` 30 天后自动清理并释放分配 |
| `observed`（你手工建的机器）要不要支持"纳管" | **不做**，永远只读 | 做的话要处理"纳管前的历史 IP 记录"归属，这是一整套新语义 |

**C. 我按默认执行，随时可改（纯参数）**

- 并发：全局 run 2、重步骤 3、轻步骤 8（§7.6.4）
- 终端并发 8；单文件上传 2 GB；磁盘水位 5 GB
- `systemd`：`MemoryMax=64M`、`CPUQuota=50%`、`IOWeight=50`、`TasksMax=256`
- 拓扑轮询 10 秒；配置缓存 10 秒

---

## 12. 仓库形态

```
labyard/                          Apache-2.0
├── LICENSE  NOTICE  README.md
├── Cargo.toml  Cargo.lock         依赖定义与锁定（Cargo.lock 必须提交）
├── rust-toolchain.toml            锁编译器版本（与 Cargo.lock 一起保证可复现）
├── Makefile                       web（vite build）→ build（cargo build）依赖链
├── .cargo/config.toml             rsproxy 源 + 交叉编译 target 配置
├── src/
│   ├── main.rs                    入口：clap 分发 serve / doctor / apply
│   ├── core/                      schema / planner / runner / events（不 import axum）
│   ├── pve/                       pct/qm/pvesh 命令拼装 + JSON 解析 + capability 探测
│   ├── resources/                 bridge / template / container / net / mirror / observed
│   ├── db/                        rusqlite 封装 + include_str! 编号 SQL 迁移器（不用 ORM）
│   ├── api/                       axum handler + SSE + auth + /tty 反代
│   ├── web/                       rust-embed：#[folder = "web/dist"]
│   └── assets/                    rust-embed：随二进制分发的执行体
│       ├── gateway/               重写的 nftables.conf / labgw / labvpn / restore unit
│       ├── base/bootstrap.sh      主机接入（IP·v6 / SSH / apt / hostname / 时区 / lrzsz）
│       └── tty/attach.sh          ttyd 的启动命令：校验 vmid → pct exec login
├── web/                           React + TS + AntD 源码（vite 构建到 web/dist）
├── tests/
│   ├── unit/                      假 Runner 回放 pct/pvesh 输出（testdata/），不碰真机
│   └── netns/                     从旧 44 项断言移植
├── bench/
│   ├── py_app.py  go_app/  rs_app/   三语言同负载对照（§7.5 的数据来源）
│   └── alloc/                     分配器对照：glibc / jemalloc / mimalloc（§7.5.2）
├── installer/install.sh           宿主一键安装 / --update（含 ttyd 静态二进制下载）
├── examples/                      main.yaml / nested.yaml / multi.yaml
└── docs/
    ├── design.md                  本文
    └── operations.md              安装、升级、排障、降级
```

三条纪律：

- 旧目录（`lab-net/` 等）**不进这个仓库、不做依赖**；`assets/gateway/` 是重写版，行为以 §1 的教训清单为准
- `docs/design.md` 是唯一权威设计文档，旧的 `部署.md` 不再维护
- CI 跑 `cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo test` + `cross build --target x86_64-unknown-linux-gnu`；netns 那组需要 root 和 `CAP_NET_ADMIN`，单独一个 job，不要塞进普通单测

**关于 `web/dist/` 要不要提交**：提交。理由：`install.sh` 在 PVE 宿主上放一个二进制就能跑，用户不需要装 Node 20 再 `npm ci && npm run build`（在 Debian 13 上装 Node 20 还得先加 nodesource 源，正好撞上"不要多一层依赖"的原则）。代价是仓库里多几 MB 构建产物、diff 里噪音大——用 `.gitattributes` 标 `linguist-generated` 和 `-diff` 就够了。**注意 `rust-embed` 要求目录在编译时存在，所以 `web/dist` 必须在 `cargo build` 之前生成，`Makefile` 里把 `web` target 设为 `build` 的前置，并在 `web/dist/` 里提交一个占位的 `index.html`，否则 `cargo check` 在干净 clone 上直接失败。**

**关于 `ttyd` 二进制要不要进仓库**：不进。它 1.3 MB、有独立的版本节奏和 CVE 响应，塞进仓库等于我们替上游发版。`install.sh` 从 GitHub Release 按 `uname -m` 下载并校验 SHA256；离线环境提供 `--ttyd /path/to/binary` 跳过下载。

---

## 13. 已经想清楚、不再问你的三件事

**镜像开关的粒度**：数据模型按"每条 `from → to` 一个 systemd 实例"（这样才能多源投同一台 analysis）；界面上默认显示**一个总开关**，展开后是每条链路的独立开关。两个需求同时满足，不用二选一。

**单节点 vs 集群**：只支持单节点，但 `doctor` 在集群环境下**只警告不阻止**——集群里的单节点用法（桥和 CT 都在同一台）完全成立，强行拒绝反而挡路。真要跨节点编排是另一个项目。

**终端多开怎么办**：不共享 PTY（ttyd 的 `-Q` 不开），每个浏览器连接独立 fork 一个 shell。所以开五个标签页就是五个互不干扰的终端，和本地开五个终端窗口一样。代价是不能"两个人看同一个终端"——那需要 tmux，而 tmux 装不装是业务范畴，平台不管。
