/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。前端做四件事：
 *   1. **一列一网段 + 区域框**（见 `layoutPositions`）：深度=列，每个 zone
 *      一块带标题的框，框内"网段卡在上、主机横排在下"——同一网段的全部
 *      节点永远在同一层、同一框里。
 *   2. **网口分侧**：外网口贴左缘、内网口贴右缘、抓包口贴下缘，略突出
 *      节点框。路径由自定义边 `LabEdge` 按 handle 朝向现算：跨列走列间
 *      走廊、长镜像线走画布底部走廊，全图零穿节点（内置 smoothstep 不
 *      避障，实测会穿 13 处）。
 *   3. 拖动不闪：数据轮询重建节点时保留 RF 内部当前位置，只换数据；
 *      拖动时边端点跟着 handle 走。
 *   4. 接住点击、多选、右键，交给页面开抽屉 / 弹菜单。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
  BaseEdge,
  Controls,
  EdgeLabelRenderer,
  Handle,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  ReactFlowProvider,
  useEdgesState,
  useNodesState,
  useReactFlow,
  type Edge,
  type EdgeProps,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { TopoEdge, TopoIface, TopoNode } from "./api";

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致，否则连线会悬空。 */
const DIM: Record<TopoNode["kind"], { w: number; h: number }> = {
  network: { w: 176, h: 60 },
  host: { w: 172, h: 92 },
};

/** 区域框（一个网段一个）与列间距。框把"同网段的全部节点"圈在一起，
 * 一眼就能看出"这个网对应的层"。 */
const ZONE = {
  pad: 12, // 框内左右边距
  // 框底留白要容下**抓包口 chip**：chip 在卡片下缘再突出 9px、自身高 14px，
  // 一共占卡片下方约 23px。留 26 才不至于让 chip 压住区域框的虚线边。
  padBottom: 26,
  head: 24, // 框标题条高
  gap: 10, // 框标题 / 网段卡 / 主机行之间的纵向间距
  colGap: 44, // 相邻两列（框）之间的走廊宽度——所有跨列线都走这里
  rowGap: 18, // 同列上下两个框的间距
  hostGap: 16, // 框内主机横排间距（网口 chip 比卡边还突出 ~17px，不能小于 16）
};

export const ROLE_LABEL: Record<string, string> = {
  gateway: "网关",
  monitor: "流量监控",
  host: "主机",
  // 老数据兜底：后端已把 server/client 归并为 host，这两个只是防旧缓存
  server: "主机",
  client: "主机",
};

type Data = {
  kind: TopoNode["kind"];
  label: string;
  role: string | null;
  ipv4: string | null;
  status: string | null;
  retired: boolean;
  vmidLabel: string;
  meta: Record<string, unknown>;
  ifaces: TopoIface[];
  /** 被选中 / 被 hover 高亮时的淡化 */
  selected: boolean;
  dimmed: boolean;
};

type GNode = Node<Data, "lab">;

type BoxData = {
  label: string;
  cidr: string;
  hosts: number;
  external: boolean;
};
type GBox = Node<BoxData, "zonebox">;

/** 网口在哪一侧。约定：**左为入、右为出、下为镜像**。
 *
 * - 网关：`eth1` 是上联口（外网 / 父网络，流量从这一侧进来）→ 左；
 *   `eth0` 是本 zone 的内网口（由它派生出去）→ 右。
 *   编号规则与后端 topology.rs 一致：eth0 恒为本网络，eth1 恒为上联。
 * - 监控机：抓包口 → 下；管理口（eth0，接它被部署到的那个网络）→ 左。
 * - 其他主机：只有一个入向口，全在左边。
 *
 * 之前只按 `net === "wan"` 判断，结果网关的 eth1（上联到父 zone 时
 * net 是父 zone 名）被放到右边，普通主机的内网口也被放到右边，与
 * 连线方向相反。
 */
function sideOf(f: TopoIface, role: string | null): "l" | "r" | "b" {
  if (f.capture) return "b";
  if (role === "gateway") return f.name === "eth1" ? "l" : "r";
  return "l";
}

/** handle id 约定：`{t|s}{l|r|b}:{iface}`。assemble 与节点渲染共用这条规则。 */
function handleId(type: "s" | "t", side: string, iface: string) {
  return `${type}${side}:${iface}`;
}

const SIDE_POS: Record<string, Position> = {
  l: Position.Left,
  r: Position.Right,
  b: Position.Bottom,
};

/** 网口 chip：按 side 吸在节点边缘（略突出 8px），handle 就在突出点上。 */
function IfaceChip({ f, role }: { f: TopoIface; role: string | null }) {
  const side = sideOf(f, role);
  const tip = f.capture
    ? `${f.name} · 抓包（${f.net} 的流量）`
    : `${f.name} · ${f.net}${f.ip ? ` ${f.ip.replace(/\/\d+$/, "")}` : ""}`;
  const isB = side === "b";
  const style: React.CSSProperties = isB
    ? { bottom: -9 }
    : side === "l"
      ? { left: -9 }
      : { right: -9 };
  const handles = (["s", "t"] as const).map((t) => (
    <Handle
      key={t}
      type={t === "s" ? "source" : "target"}
      id={handleId(t, side, f.name)}
      position={SIDE_POS[side]}
      style={{ width: 6, height: 6, opacity: 0, ...style }}
    />
  ));
  return (
    <div className={`iface${f.capture ? " iface-cap" : ""}`} title={tip} style={{ position: "relative" }}>
      {handles}
      <span className="iface-name">{f.name}</span>
      {f.capture && <span className="iface-net">{f.net}</span>}
    </div>
  );
}

function LabNode({ data }: NodeProps<GNode>) {
  const dim = DIM[data.kind];
  const meta = data.meta as Record<string, unknown>;
  const m = (k: string) => (typeof meta[k] === "string" ? (meta[k] as string) : null);
  const flag = (k: string) => meta[k] === true;

  const roleCls = data.kind === "host" && data.role ? `tn-role-${data.role}` : "";
  const cls = [
    "tn",
    data.kind === "network" ? "tn-net" : "",
    data.kind === "network" && flag("external") ? "tn-net-wan" : "",
    roleCls,
    data.retired ? "tn-retired" : "",
    data.kind === "host" && data.status !== "running" && !data.retired ? "tn-idle" : "",
    data.selected ? "tn-sel" : "",
    data.dimmed ? "tn-dim" : "",
  ]
    .filter(Boolean)
    .join(" ");

  if (data.kind !== "host") {
    // 网络节点（以及兜底）：左收右发；**底部**是"下挂主机"的出点
    // （网段卡在上、主机横排在下，线垂直向下接入每台主机，不掉头）
    return (
      <>
        <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
        {/* 底部两个出/入点：左侧给"下挂主机"（源），右侧给本 zone 网关的
            内网口（目标）。分开摆，扇形线各走各的，不挤在一个点上。 */}
        <Handle
          type="source"
          position={Position.Bottom}
          id={handleId("s", "b", "inner")}
          style={{ left: "34%", transform: "translate(-50%, 0)", opacity: 0 }}
        />
        <Handle
          type="target"
          position={Position.Bottom}
          id={handleId("t", "b", "net")}
          style={{ left: "66%", transform: "translate(-50%, 0)", opacity: 0 }}
        />
        <div className={cls} style={{ width: dim.w, height: dim.h }}>
          <div className="tn-row">
            <span className="tn-neticon" data-external={flag("external") ? "1" : "0"} />
            <span className="tn-title">{data.label}</span>
            <span className="tn-chips">
              {flag("egress") && <span className="tn-chip chip-egress">出网</span>}
              {(meta.pubs as number) > 0 && (
                <span className="tn-chip chip-pub">映射 {String(meta.pubs)}</span>
              )}
              {Array.isArray(meta.vpn) && meta.vpn.length > 0 && (
                <span className="tn-chip chip-vpn">
                  {((meta.vpn as string[]) ?? []).join("/").toUpperCase()}
                </span>
              )}
            </span>
          </div>
          <div className="tn-sub mono">{m("cidr") ?? "—"}</div>
          <div className="tn-sub tn-faint mono">
            {m("bridge")}
            {m("gateway_host") ? ` · 网关 ${m("gateway_host")}` : ""}
          </div>
        </div>
        <Handle
          type="source"
          position={Position.Right}
          id={handleId("s", "r", "net")}
          style={{ opacity: 0 }}
        />
      </>
    );
  }

  // 主机节点：网口分侧——按"左入右出下镜像"，具体规则见 sideOf
  const left = data.ifaces.filter((f) => sideOf(f, data.role) === "l");
  const right = data.ifaces.filter((f) => sideOf(f, data.role) === "r");
  const bottom = data.ifaces.filter((f) => sideOf(f, data.role) === "b");
  return (
    <>
      <div className={cls} style={{ width: dim.w, height: dim.h }}>
        <div className="tn-row">
          <span className="tn-dot" data-st={data.retired ? "retired" : data.status} />
          <span className="tn-title">{data.label}</span>
          <span className="tn-chip">{ROLE_LABEL[data.role ?? ""] ?? data.role}</span>
        </div>
        <div className="tn-sub mono">
          {(data.ipv4 ?? "").replace(/\/\d+$/, "")}
          {typeof meta.memory === "number" && (
            <span className="tn-faint">
              {" "}
              · {String(meta.cores)}C/{String(meta.memory)}M
            </span>
          )}
        </div>
        <div className="tn-sub tn-faint mono">
          {data.vmidLabel}
          {data.vmidLabel && data.retired ? " · " : ""}
          {data.retired ? "已废弃" : ""}
          {/* 监控机声明在别的网络里（如 lab:monitor、管理口在 wan）时，
              把声明名标出来，否则用户会以为凭空多了一台机器 */}
          {typeof meta.declared === "string" && (
            <span> · 声明 {String(meta.declared)}</span>
          )}
        </div>
        {/* 分侧网口：绝对定位在框边缘，chip 突出 9px 方便连线 */}
        <div
          style={{
            position: "absolute",
            left: -8,
            top: 14,
            display: "flex",
            flexDirection: "column",
            gap: 6,
          }}
        >
          {left.map((f) => (
            <IfaceChip key={f.name} f={f} role={data.role} />
          ))}
        </div>
        <div
          style={{
            position: "absolute",
            right: -8,
            top: 14,
            display: "flex",
            flexDirection: "column",
            gap: 6,
          }}
        >
          {right.map((f) => (
            <IfaceChip key={f.name} f={f} role={data.role} />
          ))}
        </div>
        <div
          style={{
            position: "absolute",
            bottom: -9,
            left: 0,
            right: 0,
            display: "flex",
            justifyContent: "center",
            gap: 6,
          }}
        >
          {bottom.map((f) => (
            <IfaceChip key={f.name} f={f} role={data.role} />
          ))}
        </div>
      </div>
    </>
  );
}

/** 区域框：纯背景层，把同一网段的网段卡 + 主机圈在一起。
 * 不可选、不可连、不接收鼠标——只是"这个网段占这一层"的视觉边界。 */
function ZoneBox({ data }: NodeProps<GBox>) {
  return (
    <div className={`zonebox${data.external ? " zonebox-wan" : ""}`}>
      <div className="zonebox-head">
        <span className="zonebox-name">{data.label}</span>
        {data.cidr && <span className="zonebox-cidr mono">{data.cidr}</span>}
        <span className="zonebox-count">{data.hosts} 台</span>
      </div>
    </div>
  );
}

/** 正交走廊边：按 handle 朝向拼折线，长距离线（镜像主干）走底部走廊。
 *
 * 为什么不用内置 smoothstep：它**不避障**，实测全图 13 条边穿过节点
 * （`gw→net` 掉头穿自己、`net:l2→l3:gw` 穿 l2:w1、镜像主干横穿主机）。
 * 这里的三段式路由在所有场景下都只经过"列间空隙"或"画布下缘"：
 *   出向 18px → 走廊 → 进向 18px。
 */
function LabEdge({
  sourceX,
  sourceY,
  targetX,
  targetY,
  sourcePosition,
  targetPosition,
  style,
  markerEnd,
  data,
}: EdgeProps) {
  const d0 = (data ?? {}) as { corridorY?: number; exitX?: number };
  const corridorY = typeof d0.corridorY === "number" ? d0.corridorY : undefined;
  const stem = 18;
  const pts: [number, number][] = [[sourceX, sourceY]];
  const push = (x: number, y: number) => {
    const last = pts[pts.length - 1];
    if (Math.abs(last[0] - x) > 0.5 || Math.abs(last[1] - y) > 0.5) pts.push([x, y]);
  };

  if (targetPosition === Position.Bottom && corridorY !== undefined) {
    // 长线（镜像主干 / 展开的逐主机线）：出向 → 画布底部走廊 → 从下方进抓包口
    if (sourcePosition === Position.Right) push(sourceX + 8, sourceY);
    else if (sourcePosition === Position.Left) {
      // 主机横排时间隙只有 16px：左侧 18px 会落进左邻主机，纵向也必须
      // 越过本机卡片（eth0 chip 中心约在卡顶 +22）到"网段卡下方空隙"，
      // 再横向出框。
      push(sourceX, sourceY - 28);
    } else if (sourcePosition === Position.Top) push(sourceX, sourceY - stem);
    else push(sourceX, sourceY + stem);
    // 先出本框右缘（列间走廊）再下沉，避免竖直段穿过本框内的主机行
    const sideX = pts[pts.length - 1][0];
    const sideY = pts[pts.length - 1][1];
    const exitX = typeof d0.exitX === "number" ? d0.exitX : sideX;
    if (Math.abs(exitX - sideX) > 1) push(exitX, sideY);
    push(pts[pts.length - 1][0], corridorY);
    push(targetX, corridorY);
    push(targetX, targetY + stem);
    push(targetX, targetY);
  } else if (sourcePosition === Position.Right && targetPosition === Position.Left) {
    // 跨列上联：走列间走廊（目标左缘再往左 28px 处必是空隙）
    const mid = targetX > sourceX + 40 ? targetX - 28 : sourceX + stem;
    push(mid, sourceY);
    push(mid, targetY);
    push(targetX, targetY);
  } else if (sourcePosition === Position.Bottom && targetPosition === Position.Top) {
    // 本网段下挂主机：垂直下行，端点错开时中段水平
    const midY = sourceY + Math.max(14, (targetY - sourceY) * 0.45);
    push(sourceX, midY);
    push(targetX, midY);
    push(targetX, targetY);
  } else if (targetPosition === Position.Bottom) {
    // 右/左 → 底（网关内网口 → 本网段卡）：**先竖直**沿主机间隙上到
    // 网段卡下方的空隙（targetY + 6），再横向进底。若先横移 18px 会
    // 擦进右边相邻主机的左上角（实测 22 个采样点穿 cache）。
    const gapY = targetY + 6;
    push(sourceX, gapY);
    push(targetX, gapY);
    push(targetX, targetY);
  } else if (sourcePosition === Position.Bottom && targetPosition === Position.Left) {
    // 网段卡下挂主机：水平段贴着主机行**上方**的空隙走（sourceY + 6），
    // 否则 +18 会横穿中间那台主机的顶部。
    const midY = sourceY + 6;
    push(sourceX, midY);
    // targetX 是**网口 chip 上**的 handle 位置，比主机卡左缘还靠左 19px；
    // 从 handle 往左让 8px 会进入左邻主机。所以竖线走在 chip 的**右边**
    // （主机卡左缘与 chip 之间的间隙），最后再向左拐 8px 进 handle。
    push(targetX + 8, midY);
    push(targetX + 8, targetY);
    push(targetX, targetY);
  } else {
    const midX = (sourceX + targetX) / 2;
    push(midX, sourceY);
    push(midX, targetY);
    push(targetX, targetY);
  }

  const d = pts.map((q, i) => `${i ? "L" : "M"} ${q[0]},${q[1]}`).join(" ");
  const lbl = typeof (data ?? {}).label === "string" ? (data as { label: string }).label : "";
  // 标签放在路径中点（长线就放在走廊上）——只在 hover 展开时由 assemble 传入
  const mid = pts[Math.floor(pts.length / 2)] ?? [sourceX, sourceY];
  return (
    <>
      <BaseEdge path={d} style={style} markerEnd={markerEnd} />
      {lbl && (
        <EdgeLabelRenderer>
          <div
            className="edge-lbl"
            style={{ transform: `translate(-50%, -50%) translate(${mid[0]}px, ${mid[1]}px)` }}
          >
            {lbl}
          </div>
        </EdgeLabelRenderer>
      )}
    </>
  );
}

const edgeTypes = { lab: LabEdge };
const nodeTypes = { lab: LabNode, zonebox: ZoneBox };

/** 边的画法参数：uplink 用蓝色描边 + 箭头，其余弱化成灰线。 */
function attachSpec(e: TopoEdge): { stroke: string; strokeWidth: number; marker?: boolean } {
  const uplink = e.port === "uplink";
  return {
    stroke: uplink ? "#2f6feb" : "#b9c4d4",
    strokeWidth: uplink ? 1.8 : 1.2,
    marker: uplink,
  };
}

function mirrorSpec(hot: boolean): Partial<Edge> {
  return {
    style: {
      stroke: "#f59e0b",
      strokeWidth: hot ? 2.2 : 1.4,
      strokeDasharray: "5 4",
      opacity: hot ? 1 : 0.45,
    },
    // 不带 label：谁抓谁看两端就够，多条线的标签挤在画布中部
    // 互相重叠（"镜像通道 x - eth0"铺一片），信息还重复。
    markerEnd: {
      type: MarkerType.ArrowClosed,
      color: "#f59e0b",
      width: 14,
      height: 14,
    },
    zIndex: hot ? 10 : 0,
  };
}

/** 跨规格同名的 zone 会撞 id（后端一套 key 空间），这里按先出现的留下 */
function uniqNodes(nodes: TopoNode[]): Map<string, TopoNode> {
  const uniq = new Map<string, TopoNode>();
  for (const n of nodes) if (!uniq.has(n.id)) uniq.set(n.id, n);
  return uniq;
}

type Box = {
  id: string;
  zone: string;
  x: number;
  y: number;
  w: number;
  h: number;
  label: string;
  cidr: string;
  hosts: number;
  external: boolean;
};

type Layout = {
  pos: Map<string, { x: number; y: number }>;
  /** 每个 zone 一块区域框：框住同网段的网段卡 + 全部主机 */
  boxes: Box[];
  /** 画布底部走廊：所有长距离镜像主干沿这条水平线走，永远在节点下方 */
  corridorY: number;
};

/** 布局：**一列一网段**，深度 = 列号。
 *
 * 每列一个带标题的区域框，框内：网段卡在上（居中）、主机横排在下。
 * 同一网段的全部节点永远落在同一个框、同一层，一眼可见归属；
 * 跨列只有 attach 的 uplink 一条，走列间走廊（`ZONE.colGap`）。
 *
 * 之前是"网段卡一列 + 主机一列"的两小列：`gw.eth0(右) → net(左)` 成了反向
 * 掉头线、穿过自己；`net:l2 → l3:gw` 穿过 l2:w1。现在网段卡和主机竖的垂直
 * 关系改成"上→下"，网段出来的线全部向下，不再掉头。
 *
 * 深度从 attach 边的 `port: uplink` 推：`net:<父> → host:<子>:gateway`。
 */
function layoutPositions(nodes: TopoNode[], edges: TopoEdge[]): Layout {
  const uniq = uniqNodes(nodes);
  const zoneOf = (n: TopoNode) => (n.kind === "network" ? n.id.slice(4) : (n.zone ?? ""));

  const parent = new Map<string, string>();
  for (const e of edges) {
    if (e.kind !== "attach" || e.port !== "uplink") continue;
    if (!e.from.startsWith("net:") || !e.to.startsWith("host:")) continue;
    const child = e.to.split(":")[1];
    parent.set(child, e.from.slice(4));
  }
  const depthOf = (zone: string) => {
    let d = 0;
    let cur = zone;
    for (let guard = 0; guard < 16 && parent.has(cur); guard++) {
      cur = parent.get(cur)!;
      d++;
    }
    return d;
  };

  const groups = new Map<string, { net: TopoNode | null; hosts: TopoNode[] }>();
  for (const n of uniq.values()) {
    const z = zoneOf(n);
    const g = groups.get(z) ?? { net: null, hosts: [] };
    if (n.kind === "network") g.net = n;
    else g.hosts.push(n);
    groups.set(z, g);
  }

  // 框内主机顺序：网关 → 监控机 → 其他主机（固定，不随轮询跳动）
  const rank = (n: TopoNode) => (n.role === "gateway" ? 0 : n.role === "monitor" ? 1 : 2);
  for (const g of groups.values()) {
    g.hosts.sort((a, b) => rank(a) - rank(b) || a.label.localeCompare(b.label));
  }

  const boxSize = (z: string) => {
    const g = groups.get(z)!;
    const n = g.hosts.length;
    const rowW = n > 0 ? n * DIM.host.w + (n - 1) * ZONE.hostGap : 0;
    const w = Math.max(DIM.network.w, rowW) + ZONE.pad * 2;
    const h =
      ZONE.head +
      ZONE.gap +
      DIM.network.h +
      (n > 0 ? ZONE.gap + DIM.host.h : 0) +
      ZONE.padBottom;
    return { w, h };
  };

  // 深度 → 该列的 zone（同列的兄弟网段按名字排，顺序稳定）
  const cols = new Map<number, string[]>();
  for (const z of groups.keys()) {
    const d = depthOf(z);
    const arr = cols.get(d) ?? [];
    arr.push(z);
    cols.set(d, arr);
  }
  for (const arr of cols.values()) arr.sort();

  const maxD = Math.max(0, ...cols.keys());
  const colW = new Map<number, number>();
  for (const [d, zs] of cols) colW.set(d, Math.max(...zs.map((z) => boxSize(z).w)));

  const colX = new Map<number, number>();
  let cx = 36;
  for (let d = 0; d <= maxD; d++) {
    colX.set(d, cx);
    cx += (colW.get(d) ?? 0) + ZONE.colGap;
  }

  const pos = new Map<string, { x: number; y: number }>();
  const boxes: Box[] = [];
  let bottom = 0;

  for (const [d, zs] of cols) {
    let y = 36;
    for (const z of zs) {
      const g = groups.get(z)!;
      const { w, h } = boxSize(z);
      const bx = colX.get(d)!;
      const meta = (g.net?.meta ?? {}) as Record<string, unknown>;

      // 网段卡：框内水平居中、贴标题条下方
      if (g.net) {
        pos.set(g.net.id, { x: bx + (w - DIM.network.w) / 2, y: y + ZONE.head + ZONE.gap });
      }
      // 主机：网段卡下方横排（超过框宽的极端情况由 fitView 兜住）
      const rowW = g.hosts.length * DIM.host.w + Math.max(0, g.hosts.length - 1) * ZONE.hostGap;
      const rowX = bx + (w - rowW) / 2;
      const rowY = y + ZONE.head + ZONE.gap + DIM.network.h + ZONE.gap;
      g.hosts.forEach((hn, i) =>
        pos.set(hn.id, { x: rowX + i * (DIM.host.w + ZONE.hostGap), y: rowY }),
      );

      boxes.push({
        id: `zone:${z}`,
        zone: z,
        x: bx,
        y,
        w,
        h,
        label: g.net?.label ?? z,
        cidr: typeof meta.cidr === "string" ? meta.cidr : "",
        hosts: g.hosts.length,
        external: meta.external === true,
      });
      bottom = Math.max(bottom, y + h);
      y += h + ZONE.rowGap;
    }
  }

  return { pos, boxes, corridorY: bottom + 44 };
}

function assemble(
  nodes: TopoNode[],
  edges: TopoEdge[],
  layout: Layout,
  selected: Set<string>,
  hotMirror: Set<string>,
  dimmed: Set<string>,
  /** 展开逐主机镜像线的网络（`zone->监控机`）；空 = 只画聚合主干 */
  expand: Set<string>,
): { nodes: (GNode | GBox)[]; edges: Edge[] } {
  const uniq = uniqNodes(nodes);
  const zoneOf = (id: string) => (id.startsWith("net:") ? id.slice(4) : (id.split(":")[1] ?? ""));

  // 区域框垫在最底层：不可拖、不可选、不参与连线，只是"这一层是这个网段"
  const rfNodes: (GNode | GBox)[] = layout.boxes.map((b) => ({
    id: b.id,
    type: "zonebox" as const,
    position: { x: b.x, y: b.y },
    width: b.w,
    height: b.h,
    draggable: false,
    selectable: false,
    connectable: false,
    deletable: false,
    focusable: false,
    zIndex: -1,
    data: { label: b.label, cidr: b.cidr, hosts: b.hosts, external: b.external },
  }));

  for (const n of uniq.values()) {
    const d = DIM[n.kind] ?? DIM.host;
    const p = layout.pos.get(n.id) ?? { x: 0, y: 0 };
    rfNodes.push({
      id: n.id,
      type: "lab",
      position: p,
      data: {
        kind: n.kind,
        label: n.label,
        role: n.role,
        ipv4: n.ipv4,
        status: n.status,
        retired: n.retired,
        vmidLabel: n.vmid ? `CT ${n.vmid}` : "",
        meta: n.meta as Record<string, unknown>,
        ifaces: n.ifaces ?? [],
        selected: selected.has(n.id),
        dimmed: dimmed.has(n.id),
      },
      width: d.w,
      height: d.h,
    });
  }

  /** 某个节点上某块网口的 handle side（不在清单里就兜底左侧） */
  const sideOfOn = (nodeId: string, iface: string): string => {
    const n = uniq.get(nodeId);
    const f = n?.ifaces?.find((x) => x.name === iface);
    return f ? sideOf(f, n?.role ?? null) : "l";
  };

  const inSameBox = (e: TopoEdge) => zoneOf(e.from) === zoneOf(e.to);

  // ── handle 解析：网段卡没有物理网口，按语义选固定的接入点 ──
  //   net 作为源、目标在本框内（下挂主机 / 管理口）：走底部，线垂直向下
  //   net 作为源、目标在别的框（跨列 uplink）：走右缘，线走列间走廊
  //   net 作为目标：走底部右侧（本 zone 网关的内网口接进来）
  const srcHandle = (e: TopoEdge): string | undefined => {
    if (e.source_iface) return handleId("s", sideOfOn(e.from, e.source_iface), e.source_iface);
    if (e.from.startsWith("net:")) {
      return inSameBox(e) ? handleId("s", "b", "inner") : handleId("s", "r", "net");
    }
    return undefined;
  };
  const tgtHandle = (e: TopoEdge): string | undefined => {
    if (e.target_iface) return handleId("t", sideOfOn(e.to, e.target_iface), e.target_iface);
    if (e.to.startsWith("net:")) return handleId("t", "b", "net");
    return undefined;
  };
  const boxOfZone = new Map(layout.boxes.map((b) => [b.zone, b]));

  const rfEdges: Edge[] = [];

  /** 单条逐主机镜像线（源口 → 监控机抓包口）。聚合主干展开时才画。
   * forceHot：从展开的主干来的线直接高亮。 */
  const perHostMirror = (e: TopoEdge, i: number, forceHot = false): Edge => {
    const id = `${e.kind}:${e.from}->${e.to}:${i}`;
    const ss = e.source_iface ? sideOfOn(e.from, e.source_iface) : "l";
    const ts = e.target_iface ? sideOfOn(e.to, e.target_iface) : "b";
    return {
      id,
      source: e.from,
      target: e.to,
      sourceHandle: handleId("s", ss, e.source_iface ?? "eth0"),
      targetHandle: handleId("t", ts, e.target_iface ?? "eth1"),
      type: "lab",
      data: {
        corridorY: layout.corridorY,
        exitX:
          (boxOfZone.get(zoneOf(e.from))?.x ?? 0) +
          (boxOfZone.get(zoneOf(e.from))?.w ?? 0) +
          20,
        sameBox: inSameBox(e),
        label: "",
      },
      ...mirrorSpec(forceHot || hotMirror.has(id)),
    } as Edge;
  };

  // ── 整网采集：同网络的多条逐主机线合成一条主干 ──
  // 语义上它是网络级开关（开在网络节点上），所以主干画 网络 → 监控机抓包口，
  // 悬停监控机或主干时再把逐主机细线展开。
  const groups = new Map<string, TopoEdge[]>();
  const singles: TopoEdge[] = [];
  for (const e of edges) {
    if (e.kind !== "mirror") continue;
    const zone = e.from.startsWith("host:") ? e.from.split(":")[1] : "";
    const netId = `net:${zone}`;
    if (zone && uniq.has(netId) && e.from !== e.to) {
      const k = `${zone}->${e.to}`;
      const arr = groups.get(k) ?? [];
      arr.push(e);
      groups.set(k, arr);
    } else {
      singles.push(e);
    }
  }

  let seq = 0;
  for (const [k, list] of groups) {
    if (list.length < 2) {
      singles.push(...list);
      continue;
    }
    const zone = k.split("->")[0];
    const head = list[0];
    const iface = head.target_iface ?? "eth1";
    const open = expand.has(k);
    const id = `mirror-agg:${k}`;
    // 主干：网段卡右缘 → 画布底部走廊 → 监控机底部抓包口。
    // 走廊在所有区域框下方（layout.corridorY），无论如何都不穿节点。
    rfEdges.push({
      id,
      source: `net:${zone}`,
      sourceHandle: handleId("s", "r", "net"),
      target: head.to,
      targetHandle: handleId("t", sideOfOn(head.to, iface), iface),
      type: "lab",
      data: {
        corridorY: layout.corridorY,
        // 先水平出到本框右缘外再下沉：直接从网段卡右缘往下会竖穿
        // 本框内的主机行（实测穿 l2:w1 12 个采样点）
        exitX: (boxOfZone.get(zone)?.x ?? 0) + (boxOfZone.get(zone)?.w ?? 0) + 20,
        sameBox: false,
        label: open ? `整网采集 · ${list.length} 台` : "",
      },
      style: {
        stroke: "#f59e0b",
        strokeWidth: open ? 2.2 : 1.3,
        strokeDasharray: "6 4",
        opacity: open ? 1 : 0.5,
      },
      markerEnd: {
        type: MarkerType.ArrowClosed,
        color: "#f59e0b",
        width: 15,
        height: 15,
      },
      zIndex: open ? 6 : 3,
    } as Edge);
    if (open) {
      list.forEach((e, i) => rfEdges.push(perHostMirror(e, i, true)));
    }
  }

  singles.forEach((e, i) => rfEdges.push(perHostMirror(e, i)));

  // attach 边：端点锚在具体网口的 handle 上。
  for (const e of edges) {
    if (e.kind !== "attach") continue;
    if (!uniq.has(e.from) || !uniq.has(e.to) || e.from === e.to) continue;
    const id = `attach:${e.from}->${e.to}:${seq++}`;
    const spec = attachSpec(e);
    rfEdges.push({
      id,
      source: e.from,
      target: e.to,
      sourceHandle: srcHandle(e),
      targetHandle: tgtHandle(e),
      type: "lab",
      data: { sameBox: inSameBox(e), label: "" },
      style: { stroke: spec.stroke, strokeWidth: spec.strokeWidth },
      markerEnd: spec.marker
        ? { type: MarkerType.ArrowClosed, color: spec.stroke, width: 14, height: 14 }
        : undefined,
      zIndex: e.port === "uplink" ? 2 : 1,
    } as Edge);
  }

  return { nodes: rfNodes, edges: rfEdges };
}

function GraphInner({
  nodes,
  edges,
  selected = [],
  onPick,
  onSelect,
  onContextMenu,
}: {
  nodes: TopoNode[];
  edges: TopoEdge[];
  selected?: string[];
  onPick?: (n: TopoNode | null) => void;
  onSelect?: (ids: string[]) => void;
  onContextMenu?: (e: React.MouseEvent, n: TopoNode | null) => void;
}) {
  // sel 只按"选中了哪些 id"失效，不按数组身份：父组件每次渲染给一个新的
  // 数组（哪怕内容相同），如果按身份比较，built 会跟着变 → setRfNodes 整体
  // 换掉节点对象 → React Flow 重新触发 onSelectionChange → 死循环，节点
  // 永不稳定，边永远量不出 handle 位置（表现为"连线全没了"）。
  const selKey = selected.join("\n");
  const sel = useMemo(() => new Set(selected), [selKey]); // eslint-disable-line react-hooks/exhaustive-deps
  const rf = useReactFlow();

  // 高亮靠 **hover** 而不是点击：点击节点要开抽屉，两个事件会撞在一起。
  // 悬停监控机节点 = 它的所有镜像线高亮；悬停某条镜像线 = 只高亮那条。
  //
  // 退出用"延迟清除"：鼠标从节点滑到贴在它边缘的抓包口/连线上时，浏览器会
  // 先发 mouseleave 再发 mouseenter，立刻置空就表现为"高亮闪一下就没"——
  // 也就是"hover 时灵时不灵"。给 160ms 的容差，进入新目标时撤销。
  const [hoverNode, setHoverNode] = useState<string | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);
  const leaveTimer = useRef<number | null>(null);
  const cancelLeave = () => {
    if (leaveTimer.current !== null) {
      window.clearTimeout(leaveTimer.current);
      leaveTimer.current = null;
    }
  };
  const scheduleLeave = () => {
    cancelLeave();
    leaveTimer.current = window.setTimeout(() => {
      leaveTimer.current = null;
      setHoverNode(null);
      setHoverEdge(null);
    }, 160);
  };
  useEffect(() => cancelLeave, []);

  const mirrorIds = useMemo(() => {
    const m = new Map<string, string[]>();
    edges.forEach((e, i) => {
      if (e.kind !== "mirror") return;
      const id = `${e.kind}:${e.from}->${e.to}:${i}`;
      (m.get(e.to) ?? m.set(e.to, []).get(e.to)!).push(id);
    });
    return m;
  }, [edges]);

  const hotMirror = useMemo(() => {
    if (hoverEdge) return new Set([hoverEdge]);
    if (!hoverNode) return new Set<string>();
    // 悬停监控机：点亮它吃进来的所有镜像线
    const inbound = mirrorIds.get(hoverNode);
    if (inbound) return new Set(inbound);
    // 悬停普通主机：点亮它发出去的镜像线——"我到底被谁在抓"一眼可见
    const out = new Set<string>();
    edges.forEach((e, i) => {
      if (e.kind === "mirror" && e.from === hoverNode) {
        out.add(`${e.kind}:${e.from}->${e.to}:${i}`);
      }
    });
    return out;
  }, [hoverEdge, hoverNode, mirrorIds, edges]);

  const dimmed = useMemo(() => {
    if (!hoverNode || !mirrorIds.has(hoverNode) || hoverEdge) return new Set<string>();
    // 悬停监控机时淡化无关节点，聚焦它的采集链路
    const keep = new Set<string>([hoverNode]);
    edges
      .filter((e) => e.kind === "mirror" && e.to === hoverNode)
      .forEach((e) => keep.add(e.from));
    const out = new Set<string>();
    nodes.forEach((n) => {
      if (!keep.has(n.id)) out.add(n.id);
    });
    return out;
  }, [edges, nodes, hoverNode, hoverEdge, mirrorIds]);

  // ── 手工布局：只在结构变化时重算（纯函数，同步出结果）──
  const structKey = useMemo(() => {
    const nk = nodes.map((n) => n.id).sort().join(",");
    const ek = edges
      .map((e) => `${e.kind}:${e.from}>${e.to}`)
      .sort()
      .join(",");
    return `${nk}#${ek}`;
  }, [nodes, edges]);

  const [layout, setLayout] = useState<Layout>({ pos: new Map(), boxes: [], corridorY: 0 });
  const fitted = useRef(false);

  useEffect(() => {
    try {
      setLayout(layoutPositions(nodes, edges));
    } catch {
      // 布局失败就保持旧位置：图乱一点总比消失好
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [structKey]);

  // 展开哪些"整网采集"主干：悬停监控机 = 全展开，悬停某条主干 = 只展开它。
  const monitorIds = useMemo(
    () => new Set(nodes.filter((n) => n.role === "monitor").map((n) => n.id)),
    [nodes],
  );
  const expand = useMemo(() => {
    const s = new Set<string>();
    if (hoverEdge?.startsWith("mirror-agg:")) {
      s.add(hoverEdge.slice("mirror-agg:".length));
    }
    if (hoverNode && monitorIds.has(hoverNode)) {
      edges.forEach((e) => {
        if (e.kind !== "mirror" || e.to !== hoverNode) return;
        if (!e.from.startsWith("host:")) return;
        s.add(`${e.from.split(":")[1]}->${e.to}`);
      });
    }
    return s;
  }, [hoverEdge, hoverNode, monitorIds, edges]);

  const built = useMemo(
    () => assemble(nodes, edges, layout, sel, hotMirror, dimmed, expand),
    [nodes, edges, layout, sel, hotMirror, dimmed, expand],
  );

  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode | GBox>([]);
  const [rfEdges, setRfEdges, onEdgesChange] = useEdgesState<Edge>([]);

  // 布局引用记一笔：ELK 异步算完返回**新 Layout 对象** → 全量采用它的
  // 位置；引用没变（同结构下的轮询刷新 / hover 高亮重建）→ 保留 RF 内部
  // 当前位置（用户拖过的）。初始 layout 是空地图，第一次 built 的节点全
  // 在 (0,0)，若不区分，ELK 结果会被这批 (0,0) 盖住——所有节点叠成一坨，
  // 就是"取景只有一台机器"的根因。
  const layoutRef = useRef<Layout | null>(null);

  useEffect(() => {
    const fresh = layoutRef.current !== layout;
    layoutRef.current = layout;
    setRfNodes((prev) => {
      if (fresh || prev.length === 0) return built.nodes;
      const have = new Map(prev.map((n) => [n.id, n.position]));
      return built.nodes.map((n) => ({ ...n, position: have.get(n.id) ?? n.position }));
    });
    setRfEdges(built.edges);
    // 首次拿到布局后取景：此时 setState 已在本轮 flush，rAF 回调能看到
    // 带坐标的节点。
    // 取景前必须确认**节点已经拿到真实坐标**：首帧那次 built 用的是空布局，
    // 所有节点都在 (0,0)，此时 fitView 会把视口放大到单点（表现是"图里只剩
    // 一个节点、还贴在角落"）。判断方式是"不全在原点"，而不是看布局 map 大小
    // ——布局已经换了、built 还是旧的那一轮就是这么漏过去的。
    const placed = built.nodes.some((n) => n.position.x !== 0 || n.position.y !== 0);
    if (!fitted.current && built.nodes.length > 0 && placed) {
      fitted.current = true;
      // rAF 回调可能跑在 React 把新节点提交进 React Flow store **之前**
      // ——量到的还是上一轮全在 (0,0) 的节点堆，视口顶到 maxZoom、中心对准
      // 原点，整张图平移出画布（表现：只剩一个节点挂在角落）。所以取景前
      // 先问 store 要一次节点：没就绪就下一帧再试，最多等半秒。
      const tryFit = (tries: number) => {
        const ns = rf.getNodes();
        const ready =
          ns.length > 0 && ns.some((n) => n.position.x !== 0 || n.position.y !== 0);
        if (!ready) {
          if (tries > 0) requestAnimationFrame(() => tryFit(tries - 1));
          return;
        }
        // minZoom 别低于 0.55：再小下去节点里的字就成了灰块，
        // "看清楚整张图"不如"看得清一台机器"，超出的部分让人自己拖。
        rf.fitView({ padding: 0.12, duration: 200, minZoom: 0.55 });
      };
      requestAnimationFrame(() => tryFit(30));
    }
  }, [built, setRfNodes, setRfEdges, layout, rf]);

  const pick = (id: string | undefined) => {
    const orig = id ? (nodes.find((x) => x.id === id) ?? null) : null;
    onPick?.(orig);
  };

  return (
    <div className="topo-canvas">
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        nodeTypes={nodeTypes}
        edgeTypes={edgeTypes}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onSelectionChange={({ nodes: ns }) =>
          onSelect?.(ns.filter((n) => n.type !== "zonebox").map((n) => n.id))
        }
        // 普通拖动 = 平移画布，Shift + 拖动 = 框选。反过来的话（拖拽即框选）
        // 会让人以为图卡住了：想挪一下视野，结果拉出一个选框。
        selectionOnDrag={false}
        selectionKeyCode="Shift"
        multiSelectionKeyCode={["Meta", "Control"]}
        deleteKeyCode={null}
        minZoom={0.15}
        maxZoom={1.6}
        nodesConnectable={false}
        edgesFocusable={false}
        onNodeClick={(_, n) => {
          if (n.type === "zonebox") return;
          pick(n.id);
        }}
        onPaneClick={() => pick(undefined)}
        // hover 高亮：监控机节点与镜像线都接住鼠标
        onNodeMouseEnter={(_, n) => {
          if (n.type === "zonebox") return;
          cancelLeave();
          setHoverNode(n.id);
          setHoverEdge(null);
        }}
        onNodeMouseLeave={() => scheduleLeave()}
        onEdgeMouseEnter={(_, e) => {
          cancelLeave();
          setHoverEdge(e.id);
        }}
        onEdgeMouseLeave={() => scheduleLeave()}
        onNodeContextMenu={(ev, n) => {
          ev.preventDefault();
          const orig = nodes.find((x) => x.id === n.id) ?? null;
          onContextMenu?.(ev, orig);
        }}
        onPaneContextMenu={(ev) => {
          ev.preventDefault();
          onContextMenu?.(ev as React.MouseEvent, null);
        }}
        proOptions={{ hideAttribution: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={18} size={1} color="#dbe1ea" />
        <Controls showInteractive={false} />
        <MiniMap
          pannable
          zoomable
          nodeBorderRadius={3}
          nodeStrokeWidth={2}
          nodeColor={(n) => {
            const d = n.data as Data;
            if (d?.kind === "network") return d.meta?.external ? "#a5b4fc" : "#ddd6fe";
            if (d?.role === "gateway") return "#bbf7d0";
            if (d?.role === "monitor") return "#a5f3fc";
            return "#e2e8f0";
          }}
          maskColor="rgba(245,246,248,0.72)"
          // 之前用的是 var(--border)，变量没定义就等于没有边框：缩略图贴在
          // 画布右下角像一块脏色斑。这里写死颜色，并加圆角与投影。
          style={{
            background: "#ffffff",
            border: "1px solid #e2e8f0",
            borderRadius: 8,
            boxShadow: "0 2px 8px rgba(15,23,42,.08)",
            width: 168,
            height: 104,
            margin: 12,
          }}
        />
      </ReactFlow>
    </div>
  );
}

export default function TopologyGraph(props: Parameters<typeof GraphInner>[0]) {
  return (
    <ReactFlowProvider>
      <GraphInner {...props} />
    </ReactFlowProvider>
  );
}
