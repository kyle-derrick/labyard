#!/usr/bin/env python3
"""真机逐接口回归：把路由表里每个端点都打一遍，校验状态码与关键字段。

和 `smoke.sh` 的分工：
  - `smoke.sh` 在无 PVE 的容器里跑，验证 HTTP 层 / planner / 规格持久化，
    不碰真资源，跑得快。
  - 本脚本对着**已部署好的真机面板**跑，验证每条路由都接得上 PVE 层、
    返回体字段齐全、写操作能落地并能复原。

用法：
    ./scripts/api-regress.py --base https://127.0.0.1:8443 --password 'xxx'
    ./scripts/api-regress.py --base https://127.0.0.1:8443 --password 'xxx' --destructive
    ./scripts/api-regress.py --help

密码也可以用环境变量 LABYARD_PW 传，避免进 shell history。

默认**不做**破坏性操作（destroy 真删资源、VPN 启停会重启网关容器）：
  - destroy：只验证 `confirm` 与 zone 名不一致时被 400 拒绝
  - VPN：只验证关闭幂等与 client 端点，不真的开
加 `--destructive` 才会跑完整链路。

退出码：全部通过 0，有失败 1。
"""

import argparse
import http.cookiejar
import json
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ap = argparse.ArgumentParser(description="labyard 面板逐接口回归")
ap.add_argument("--base", default="https://127.0.0.1:8443", help="面板地址")
ap.add_argument("--password", default=None, help="登录密码（或设 LABYARD_PW）")
ap.add_argument("--zone", default="lab", help="用来跑写操作的主 zone（默认 lab）")
ap.add_argument("--host", default=None, help="该 zone 里一台非网关主机（默认自动挑）")
ap.add_argument("--destructive", action="store_true", help="跑 destroy 与 VPN 启停")
args = ap.parse_args()

BASE = args.base.rstrip("/")
PW = args.password or __import__("os").environ.get("LABYARD_PW", "")
ZONE = args.zone

# 面板用的是自签证书，回归脚本无所谓中间人——这里不做证书校验
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE
jar = http.cookiejar.CookieJar()
op = urllib.request.build_opener(
    urllib.request.HTTPSHandler(context=ctx),
    urllib.request.HTTPCookieProcessor(jar),
)

rows = []


def q(seg: str) -> str:
    """路径段必须编码：镜像连线的 id 里含 `->`，原始 `>` 会被 HTTP/1 解析器
    以 400 拒绝（RFC 3986 里它就不是合法 URI 字符），前端用的是
    encodeURIComponent，这里保持一致。"""
    return urllib.parse.quote(seg, safe="")


def call(method, path, body=None, raw=False):
    data = None
    hdr = {}
    if body is not None:
        data = json.dumps(body).encode()
        hdr["content-type"] = "application/json"
    req = urllib.request.Request(BASE + path, data=data, method=method, headers=hdr)
    try:
        r = op.open(req, timeout=300)
        code, txt = r.status, r.read()
    except urllib.error.HTTPError as e:
        code, txt = e.code, e.read()
    except Exception as e:  # 连接层失败也要计入，而不是让脚本崩掉
        return 0, str(e)
    if raw:
        return code, txt
    try:
        return code, json.loads(txt)
    except Exception:
        return code, txt[:120]


def record(ok, label, what, code, detail=""):
    rows.append((ok, label, what, code, detail))
    print(f"{'OK  ' if ok else 'FAIL'} {label:<34} {code:<4} {what}", flush=True)
    if not ok and detail:
        print(f"     -> {detail}", flush=True)


def check(label, method, path, body=None, want=200, keys=None, forbid=None):
    code, p = call(method, path, body)
    ok = (code in want) if isinstance(want, (list, tuple, set)) else (code == want)
    detail = ""
    if ok and isinstance(p, dict):
        miss = [k for k in (keys or []) if k not in p]
        if miss:
            ok, detail = False, "缺字段 " + ",".join(miss)
        bad = [k for k in (forbid or []) if k in p]
        if bad:
            ok, detail = False, "不该有字段 " + ",".join(bad)
    if not ok:
        detail = detail or str(p)[:110]
    record(ok, label, f"{method} {path}", code, detail)
    return p


def head(label):
    print(f"\n── {label} ──", flush=True)


# ─────────────────────────── 只读 ───────────────────────────
head("健康与认证")
check("健康检查", "GET", "/api/health", keys=["ok"])

# allow_anonymous 是配置项：关着的时候匿名访问 me 应当 401，开着才 200。
code, me = call("GET", "/api/auth/me")
if code == 200:
    record(isinstance(me, dict) and "user" in me, "匿名可访问 me", "GET /api/auth/me", code,
           "" if isinstance(me, dict) else str(me)[:80])
elif code == 401:
    record(True, "匿名被拒（allow_anonymous=false）", "GET /api/auth/me", code)
else:
    record(False, "匿名可访问 me", "GET /api/auth/me", code, str(me)[:80])

check("错误密码被拒", "POST", "/api/auth/login", {"password": "wrong"}, want=401)
if PW:
    check("登录", "POST", "/api/auth/login", {"password": PW}, keys=["user"])
    check("会话生效", "GET", "/api/auth/me", keys=["user"])
elif code == 401:
    # 面板关着匿名访问，后面每条都会 401：直接说清楚，别让 30 条假失败刷屏
    print("\n这个实例 allow_anonymous=false，必须给密码：--password 或 LABYARD_PW",
          flush=True)
    sys.exit(2)
else:
    print("（匿名模式，跳过写操作）", flush=True)

head("规格与计划")
check("步骤状态", "GET", "/api/state", keys=["steps"])
check("默认规格", "GET", "/api/spec", keys=["yaml", "exists"])
check("规格列表", "GET", "/api/specs", keys=["specs"])
check("单份规格", "GET", f"/api/specs/{q(ZONE)}", keys=["yaml", "exists", "name"])
# 可视化编排的数据源：YAML 的 JSON 投影，不是补全过默认值的那份
check("结构化视图", "GET", f"/api/specs/{q(ZONE)}/model",
      keys=["model", "exists", "name"])
check("资源目录", "GET", "/api/catalog",
      keys=["template_storages", "disk_storages", "templates"])
check("zone 片段", "GET", f"/api/specs/{q(ZONE)}/zones/{q(ZONE)}",
      keys=["zone", "yaml", "exists"])
gone = check("zone 片段-不存在也返回 200", "GET",
             f"/api/specs/{q(ZONE)}/zones/nope__", keys=["zone", "yaml", "exists"])
record(isinstance(gone, dict) and gone.get("exists") is False and gone.get("yaml") is None,
       "不存在的 zone 是 exists:false", "yaml 为 null", 200, str(gone)[:110])
pv = check("预览", "POST", "/api/preview", {"spec": ZONE}, keys=["steps"])
# preview 现在带逐步动作判定：有字段就行（值取决于环境实际状态，不硬断言）
if isinstance(pv, dict) and pv.get("steps"):
    first = pv["steps"][0]
    record("action" in first, "预览带 create/update/skip 判定", "steps[0]",
           200, str(first)[:110])

head("运行态")
check("自检", "GET", "/api/doctor", keys=["items"])
check("run 历史", "GET", "/api/runs", keys=["runs"])
check("主机清单", "GET", "/api/hosts", keys=["hosts"])
check("拓扑", "GET", "/api/topology", keys=["nodes", "edges", "specs"])
check("网关状态", "GET", f"/api/zones/{q(ZONE)}/gateway/status",
      keys=["zone", "spec", "gateway"])
check("镜像列表", "GET", "/api/mirrors", keys=["mirrors", "reconcile"])
check("OpenAPI", "GET", "/api/openapi.json", keys=["openapi", "paths"])

# 主机：从清单里挑一台非网关的，后面生命周期/电源/快照都用它。
# 注意用 `host`（规格里的主机名）而不是 `name`（PVE 里的容器名）：后者是
# `lab-cache` 这种带 zone 前缀的显示名，拿去当路径参数会 404。
hosts = call("GET", "/api/hosts")[1]
HOST = args.host
if not HOST and isinstance(hosts, dict):
    cands = [h for h in hosts.get("hosts", [])
             if h.get("zone") == ZONE and h.get("vmid") and h.get("host")]
    # 优先普通主机：网关不能当镜像源，分析机当源会变成"自己镜像自己"
    for want_roles in (("host", "server", "client"), ()):
        for h in cands:
            role = h.get("role") or ""
            if want_roles and role not in want_roles:
                continue
            if not want_roles and role in ("gateway", "analysis", "monitor"):
                continue
            HOST = h.get("host")
            break
        if HOST:
            break
if not HOST:
    print("（清单里没挑到可用主机，跳过主机级检查）", flush=True)

if HOST:
    check("快照列表", "GET", f"/api/snapshots/{q(ZONE)}/{q(HOST)}", keys=["snapshots", "vmid"])
    check("快照 action 非法被拒", "POST", f"/api/snapshots/{q(ZONE)}/{q(HOST)}",
          {"action": "nope", "name": "x"}, want=400)
    check("快照名非法被拒", "POST", f"/api/snapshots/{q(ZONE)}/{q(HOST)}",
          {"action": "create", "name": "../etc"}, want=400)

head("参数校验（负路径）")
check("电源 action 非法被拒", "POST", f"/api/hosts/{q(ZONE)}/{q(HOST or 'x')}/power",
      {"action": "explode"}, want=[400, 404])
check("不存在的主机 power 404", "POST", f"/api/hosts/{q(ZONE)}/no-such-host/power",
      {"action": "reboot"}, want=404)
check("不存在的主机 retire 404", "POST", f"/api/hosts/{q(ZONE)}/no-such-host/retire",
      want=404)
check("不存在的主机 revive 404", "POST", f"/api/hosts/{q(ZONE)}/no-such-host/revive",
      want=404)
check("destroy 二次确认不一致被拒", "POST", "/api/destroy",
      {"zone": ZONE, "confirm": "im-not-sure"}, want=[400, 404])
check("VPN action 非法被拒", "POST", f"/api/zones/{q(ZONE)}/gateway/vpn",
      {"proto": "wireguard", "action": "maybe"}, want=[400, 404])

# ─────────────────────────── 规格 CRUD ───────────────────────────
if PW:
    head("规格 CRUD（临时规格 zz-regress）")
    TMP = "zz-regress"
    call("DELETE", f"/api/specs/{q(TMP)}")  # 清掉上一次跑挂的残留
    check("复制出一份临时规格", "POST", "/api/specs", {"name": TMP, "from": ZONE},
          want=[200, 201])
    check("重名被拒", "POST", "/api/specs", {"name": TMP, "from": ZONE}, want=[400, 409])
    check("非法名被拒", "POST", "/api/specs", {"name": "Bad Name!", "from": ZONE}, want=400)
    tmp = check("读回临时规格", "GET", f"/api/specs/{q(TMP)}", keys=["yaml", "exists"])
    if isinstance(tmp, dict) and tmp.get("yaml"):
        check("原样保存", "PUT", f"/api/specs/{q(TMP)}", {"yaml": tmp["yaml"]}, keys=["ok"])
    mv = check("读回结构化视图", "GET", f"/api/specs/{q(TMP)}/model",
               keys=["model", "exists"])
    if isinstance(mv, dict) and mv.get("model"):
        model = mv["model"]
        model.setdefault("defaults", {})["cores"] = 3
        check("按结构化视图保存", "PUT", f"/api/specs/{q(TMP)}/model",
              {"model": model}, keys=["ok", "yaml"])
    # ── 整网监控开关 ──
    # 开（on=true）会真的建抓包桥/装采集单元，回归里不跑——只验证：
    # ① use 与 name 同写被 400 挡下（且不动规格）；② 关闭是幂等清理。
    check("监控开关-use+name 冲突被拒", "POST", f"/api/zones/{q(ZONE)}/monitor",
          {"on": True, "name": "probe", "use": f"{ZONE}:probe"}, want=400)
    before = call("GET", f"/api/specs/{q(ZONE)}")[1]
    # 关监控有两种合法结果：
    # ① 200 —— 没有别的 zone 在复用，声明被移除；
    # ② 400 —— 有别的 zone 还在 monitor.use 指向它。这时**必须**拒绝并说明
    #    依赖方，否则删掉声明会让那些 use 变成悬空引用（以"监控机不存在"
    #    的面目报出来，把用户引向完全错误的方向）。
    code, p = call("POST", f"/api/zones/{q(ZONE)}/monitor", {"on": False})
    ok = code == 200 and isinstance(p, dict) and "ok" in p
    if code == 400 and isinstance(p, dict) and "正在复用" in str(p.get("error", "")):
        ok = True
    record(ok, "监控开关-关（幂等 / 有依赖时拒绝）",
           f"POST /api/zones/{q(ZONE)}/monitor", code,
           "" if ok else str(p)[:110])
    check("加 zone", "POST", f"/api/specs/{q(TMP)}/zones",
          {"zone": "tmp1",
           "yaml": "uplink: wan\nv4: 10.99.1.0/24\nhosts:\n  gw: { role: gateway }\n"},
          keys=["ok"])
    check("读 zone 片段", "GET", f"/api/specs/{q(TMP)}/zones/tmp1", keys=["yaml", "exists"])
    check("改 zone 片段", "PUT", f"/api/specs/{q(TMP)}/zones/tmp1",
          {"yaml": "uplink: wan\nv4: 10.99.2.0/24\nhosts:\n  gw: { role: gateway }\n"},
          keys=["ok"])
    check("删 zone", "DELETE", f"/api/specs/{q(TMP)}/zones/tmp1", keys=["ok"])

    # ── 镜像连线的增删往返，在临时 zone 上做 ──
    # 不去动真规格里的连线：那条同时由顶层 `mirrors:` 和主机上的 `mirror:` 字段
    # 生成，删了会让环境变样，而且删不干净（见下面那条负向断言）。
    check("加镜像用的 zone", "POST", f"/api/specs/{q(TMP)}/zones",
          {"zone": "zt",
           "yaml": "uplink: wan\nv4: 10.99.3.0/24\n"
                   "hosts:\n  gw: { role: gateway }\n  s1: { role: server }\n"
                   "  a1: { role: analysis }\n"},
          keys=["ok"])
    link = "zt:s1->zt:a1"
    check("加镜像连线", "POST", "/api/mirrors", {"from": "zt:s1", "to": "zt:a1"},
          keys=["ok", "spec"])
    lst = call("GET", "/api/mirrors")[1]
    ids = [m["from"] + "->" + m["to"] for m in lst.get("mirrors", [])] \
        if isinstance(lst, dict) else []
    record(link in ids, "连线 id 格式为 from->to", "GET /api/mirrors", 200, str(ids)[:110])
    check("重复加被拒", "POST", "/api/mirrors", {"from": "zt:s1", "to": "zt:a1"}, want=400)
    check("不存在的源主机被拒", "POST", "/api/mirrors",
          {"from": "zt:no-such-host", "to": "zt:a1"}, want=400)
    # 监控机（analysis）抓自己 = 自己的内网口 → 自己的抓包口，没有回环，合法；
    # 普通 server 抓自己没有意义，解析阶段就拦。
    check("监控机抓自己的内网口（合法）", "POST", "/api/mirrors",
          {"from": "zt:a1", "to": "zt:a1"}, keys=["ok", "spec"])
    self_link = "zt:a1->zt:a1"
    check("删监控机自连线", "DELETE", f"/api/mirrors/{q(self_link)}", keys=["ok"])
    check("普通主机抓自己被拒", "POST", "/api/mirrors",
          {"from": "zt:s1", "to": "zt:s1"}, want=400)
    check("删镜像连线", "DELETE", f"/api/mirrors/{q(link)}", keys=["ok"])
    lst = call("GET", "/api/mirrors")[1]
    left = [m["from"] + "->" + m["to"] for m in lst.get("mirrors", [])] \
        if isinstance(lst, dict) else []
    record(link not in left, "删后列表里不再有", "GET /api/mirrors", 200, str(left)[:110])
    check("再删一次失败", "DELETE", f"/api/mirrors/{q(link)}", want=400)
    check("删镜像用的 zone", "DELETE", f"/api/specs/{q(TMP)}/zones/zt", keys=["ok"])

    # ── monitor.use 引用其它 zone 的新建声明 ──
    # 回归：监控机声明 mgmt: wan 时实际建在 wan 里，`za:monitor` 这种按
    # 声明所在 zone 的写法曾误报"不存在"，用户白配半天。
    check("监控声明+跨网引用-保存", "PUT", f"/api/specs/{q(TMP)}", {"yaml":
        "defaults:\n  cores: 1\n  memory: 512\n  disk: 4\n"
        "  template: debian-13-standard\n  storage: local-lvm\n"
        "zones:\n"
        "  za:\n    uplink: wan\n    v4: 10.99.4.0/24\n"
        "    monitor: { name: monitor, mgmt: wan }\n"
        "    hosts:\n      gw: { role: gateway }\n"
        "  zb:\n    uplink: za\n    v4: 10.99.5.0/24\n"
        "    monitor: { use: \"za:monitor\" }\n"
        "    hosts:\n      gw: { role: gateway }\n"},
        keys=["ok"])
    check("引用不存在的声明-被拒", "PUT", f"/api/specs/{q(TMP)}", {"yaml":
        "defaults:\n  cores: 1\n  memory: 512\n  disk: 4\n"
        "  template: debian-13-standard\n  storage: local-lvm\n"
        "zones:\n"
        "  za:\n    uplink: wan\n    v4: 10.99.4.0/24\n"
        "    monitor: { name: monitor, mgmt: wan }\n"
        "    hosts:\n      gw: { role: gateway }\n"
        "  zb:\n    uplink: za\n    v4: 10.99.5.0/24\n"
        "    monitor: { use: \"za:probe\" }\n"
        "    hosts:\n      gw: { role: gateway }\n"},
        want=400)

    check("删临时规格", "DELETE", f"/api/specs/{q(TMP)}", keys=["ok"])
    # 删除后的读不是 404，而是 exists:false——前端靠这个字段决定显示"新建"还是"编辑"
    after_del = check("删后再读返回 exists:false", "GET", f"/api/specs/{q(TMP)}",
                      keys=["yaml", "exists", "name"])
    record(isinstance(after_del, dict) and after_del.get("exists") is False,
           "删除后 exists 为 false", "GET /api/specs/{name}", 200, str(after_del)[:110])

# ─────────────────────────── 网关 ───────────────────────────
if PW:
    head("网关：端口映射 / egress")
    check("加端口映射", "POST", f"/api/zones/{q(ZONE)}/gateway/pub",
          {"proto": "tcp", "port": 17681, "to": "10.77.0.20:7681"}, keys=["ok"])
    time.sleep(5)
    st = check("网关状态里能看到映射", "GET", f"/api/zones/{q(ZONE)}/gateway/status",
               keys=["zone", "spec", "gateway", "egress", "pubs", "vpn"])
    pubs = (st or {}).get("pubs") or []
    record(any(p.get("port") == 17681 for p in pubs), "映射已落进规格",
           "gateway.pubs", 200, f"pubs={pubs}"[:110])
    check("删端口映射", "DELETE", f"/api/zones/{q(ZONE)}/gateway/pub/0", keys=["ok"])
    time.sleep(4)
    check("关 egress", "POST", f"/api/zones/{q(ZONE)}/gateway/egress", {"on": False},
          keys=["ok"])
    time.sleep(4)
    off = check("egress 关后读状态", "GET", f"/api/zones/{q(ZONE)}/gateway/status",
                keys=["egress"])
    record(isinstance(off, dict) and off.get("egress") is False,
           "egress 真的关了", "egress=false", 200, str((off or {}).get("egress")))
    check("开 egress", "POST", f"/api/zones/{q(ZONE)}/gateway/egress", {"on": True},
          keys=["ok"])
    time.sleep(3)
    on = check("egress 开回来", "GET", f"/api/zones/{q(ZONE)}/gateway/status",
               keys=["egress"])
    record(isinstance(on, dict) and on.get("egress") is True,
           "egress 恢复为开", "egress=true", 200, str((on or {}).get("egress")))

    # VPN：关一次验证幂等，再把它恢复成原来那样，免得回归把机器改了个样
    was_on = bool(((on or {}).get("vpn") or {}).get("wireguard")) \
        if isinstance(on, dict) else False
    check("VPN 关闭幂等", "POST", f"/api/zones/{q(ZONE)}/gateway/vpn",
          {"proto": "wireguard", "action": "off"}, keys=["ok"])
    check("VPN client 端点", "GET", f"/api/zones/{q(ZONE)}/gateway/vpn/wireguard/client",
          want=[200, 404])
    if was_on:
        time.sleep(2)
        check("VPN 恢复为开启", "POST", f"/api/zones/{q(ZONE)}/gateway/vpn",
              {"proto": "wireguard", "action": "on"}, keys=["ok"])
        time.sleep(8)

# ─────────────────────────── 镜像：主机字段生成的连线 ───────────────────────────
ANALYSIS = None
if isinstance(hosts, dict):
    for h in hosts.get("hosts", []):
        if h.get("zone") == ZONE and h.get("role") == "analysis":
            ANALYSIS = h.get("host")
            break


def mirror_ids():
    r = call("GET", "/api/mirrors")[1]
    if not isinstance(r, dict):
        return []
    return [m["from"] + "->" + m["to"] for m in r.get("mirrors", [])]


if PW and HOST and ANALYSIS:
    head("镜像：主机字段生成的连线")
    link = f"{ZONE}:{HOST}->{ZONE}:{ANALYSIS}"
    if link in mirror_ids():
        # 这条连线来自主机上的 `mirror: source/target`（可能同时也有顶层条目）。
        # 删顶层条目不会让它消失，接口必须明确报错而不是回 200 —— 这是
        # "点了删除、刷新又出现"那类问题的根源。
        code, p = call("DELETE", f"/api/mirrors/{q(link)}")
        detail = p.get("error", "") if isinstance(p, dict) else str(p)[:110]
        record(code == 400 and link in mirror_ids(),
               "主机字段生成的连线删不掉且明确报错", f"DELETE /api/mirrors/{link}", code,
               f"{detail}")
    else:
        print(f"（{link} 不存在，跳过这条负向检查）", flush=True)

# ─────────────────────────── 生命周期 ───────────────────────────
if PW and HOST:
    head("生命周期")

    def retired_flag():
        r = call("GET", "/api/hosts")[1]
        if not isinstance(r, dict):
            return None
        for h in r.get("hosts", []):
            if h.get("zone") == ZONE and h.get("host") == HOST:
                return h.get("retired")
        return None

    check("retire 主机", "POST", f"/api/hosts/{q(ZONE)}/{q(HOST)}/retire",
          keys=["ok", "stopped", "mirror_units_stopped"])
    time.sleep(2)
    record(retired_flag() is True, "主机被标记为 retired", "hosts[].retired", 200,
           f"retired={retired_flag()}")
    check("revive 主机", "POST", f"/api/hosts/{q(ZONE)}/{q(HOST)}/revive",
          keys=["ok", "started"])
    time.sleep(6)
    record(retired_flag() is not True, "revive 后标记被清掉", "hosts[].retired", 200,
           f"retired={retired_flag()}")

    check("retire 整 zone", "POST", f"/api/zones/{q(ZONE)}/retire", keys=["ok"])
    time.sleep(2)
    st = check("zone 全部标为 retired", "GET", f"/api/zones/{q(ZONE)}/gateway/status",
               keys=["retired_hosts"])
    record(isinstance(st, dict) and len(st.get("retired_hosts") or []) >= 2,
           "整 zone retire 覆盖所有主机", "retired_hosts", 200,
           str((st or {}).get("retired_hosts"))[:110])
    check("revive 整 zone", "POST", f"/api/zones/{q(ZONE)}/revive", keys=["ok"])
    time.sleep(8)

if PW and HOST:
    head("电源")
    check("电源 reboot", "POST", f"/api/hosts/{q(ZONE)}/{q(HOST)}/power",
          {"action": "reboot"}, want=[200, 202])
    time.sleep(6)

# ─────────────────────────── 执行链路 ───────────────────────────
if PW:
    head("执行链路")
    run = check("apply", "POST", "/api/apply", {"spec": ZONE}, keys=["run_id"])
    rid = run.get("run_id") if isinstance(run, dict) else None
    if rid:
        status = "?"
        for _ in range(100):
            st = call("GET", "/api/runs")[1]
            cur = [r for r in st.get("runs", []) if r.get("id") == rid] \
                if isinstance(st, dict) else []
            if cur and cur[0].get("status") in ("ok", "failed", "error", "cancelled"):
                status = cur[0]["status"]
                break
            time.sleep(3)
        record(status in ("ok", "failed", "error", "cancelled"),
               "apply 跑到终态", f"run {rid}", 200, f"status={status}")
        check("run 步骤", "GET", f"/api/runs/{q(rid)}/steps", keys=["steps"])
        c, body = call("GET", f"/api/runs/{q(rid)}/events", raw=True)
        # 已结束的 run：服务端补一条 finished 就关流，不该把连接挂在那里等
        record(c == 200 and isinstance(body, bytes) and b"data:" in body,
               "已结束 run 的 SSE 立即收尾", f"GET /api/runs/{rid}/events", c,
               f"{len(body) if isinstance(body, bytes) else '?'} bytes")
        check("不存在的 run 的 SSE 404", "GET", "/api/runs/no-such-run/events", want=404)
        check("已结束的 run 再 cancel", "POST", f"/api/runs/{q(rid)}/cancel",
              want=[200, 400, 409])
        check("已结束的 run 再 retry", "POST", f"/api/runs/{q(rid)}/retry",
              want=[200, 400, 409, 412])

# ─────────────────────────── 终端反代 ───────────────────────────
head("终端反代")
GW = None
if isinstance(hosts, dict):
    for h in hosts.get("hosts", []):
        if h.get("zone") == ZONE and h.get("role") == "gateway":
            GW = h.get("host")
            break
if GW:
    gw = f"{ZONE}/{GW}"
    c, body = call("GET", f"/terminal/{gw}", raw=True)
    record(c in (200, 301, 302, 307, 308), "终端裸路径", f"GET /terminal/{gw}", c,
           f"{len(body) if isinstance(body, bytes) else '?'} bytes")
    c, body = call("GET", f"/terminal/{gw}/", raw=True)
    ok = c == 200 and isinstance(body, bytes) and b"ttyd" in body.lower()
    record(ok, "终端页面（ttyd）", f"GET /terminal/{gw}/", c,
           f"{len(body) if isinstance(body, bytes) else '?'} bytes")
    c, body = call("GET", f"/terminal/{gw}/token", raw=True)
    record(c in (200, 404), "终端子路径代理", f"GET /terminal/{gw}/token", c)
    c, _ = call("GET", f"/terminal/{ZONE}/no-such-host/", raw=True)
    record(c in (404, 502, 503), "不存在主机的终端不 500", "GET /terminal/{z}/nope/", c)
else:
    print("（清单里没挑到网关主机，跳过终端检查）", flush=True)

# ─────────────────────────── 可选：破坏性 ───────────────────────────
if args.destructive and PW:
    head("破坏性（--destructive）")
    check("VPN 开启", "POST", f"/api/zones/{q(ZONE)}/gateway/vpn",
          {"proto": "wireguard", "action": "on"}, keys=["ok"])
    time.sleep(10)
    check("VPN client 可下载", "GET", f"/api/zones/{q(ZONE)}/gateway/vpn/wireguard/client",
          want=200)
    check("VPN 关闭", "POST", f"/api/zones/{q(ZONE)}/gateway/vpn",
          {"proto": "wireguard", "action": "off"}, keys=["ok"])

bad = [r for r in rows if not r[0]]
print(f"\n{len(rows) - len(bad)}/{len(rows)} 通过", flush=True)
if bad:
    print("失败项：", flush=True)
    for _, label, what, code, detail in bad:
        print(f"  - {label}  [{code}] {what}\n    {detail}", flush=True)
sys.exit(1 if bad else 0)
