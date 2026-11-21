/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。前端做三件事：
 *
 *   1. **一列一类，交替排开**（`layoutPositions`）：网段占一列、它的设备占
 *      下一列，再下一个网段一列……即
 *      `外网 | 网关·监控机·主机 | 内网 | 网关·监控机·主机 | …`。
 *      网段只要看列就知道在第几层，不再画区域框（框里再套框，越看越乱）。
 *   2. **连线全部横着走**：网段卡的出入口在**左右两侧**（左=上联进来 /
 *      右=下挂设备），主机的网口 chip 朝着自己所在的网段。跨列线走列间
 *      走廊（每条线一条车道，互不重合），所以线下不穿节点、上不掉头。
 *   3. 拖动不闪、hover 只点亮相关的一小片，见文件后半。
 *
 * 镜像线的两端：**监控机**那端是它卡片下方的抓包口；**网关/主机**那端就
 * 在它那块真实网口的 chip 上（和网络连线同一个锚点）——镜像本来就是从
 * 这块网卡复制出去的，锚在别处就成了"凭空多出来一个口"。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
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

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致：网口 chip 是 absolute
 * 吸在卡片边缘上的，尺寸对不上连线就会悬空。 */
const DIM: Record<TopoNode["kind"], { w: number; h: number }> = {
  network: { w: 176, h: 60 },
  host: { w: 176, h: 92 },
};

/** 画布几何。所有间距只在这里改，别散落到各处。 */
const GEO = {
  pad: 40, // 画布左上留白
  colGap: 100, // 列间走廊：跨列线全走这里（同时要容下镜像竖线与车道）
  rowGap: 22, // 同列上下两个节点之间（要容下卡片下缘突出的抓包口 chip）
  lane0: 46, // 第一条车道离目标口的距离
  laneStep: 9, // 车道间距（同一个网段扇出多条线时用）
  mir: 13, // 镜像线从网口 chip 外沿再往外走多少再下沉
  mirStep: 10, // 同列同侧多台被镜像的主机，竖线依次错开（6 太小，看着像双线）
  corridor: 44, // 画布底部走廊：在最低节点下方多少
  busLane: 30, // 每台监控机一条横向采集通道
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
  /** 主机所属网段：标题下第一行显示，三个 zone 的 gateway 才不会混 */
  zone: string | null;
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

/** 网口在哪一侧。约定：**朝自己的网段**。
 *
 * - 网关：`eth1` 是上联口（接上一列那个网段）→ 左；`eth0` 是本 zone 内网口
 *   （接下一列那个网段）→ 右。编号与后端 topology.rs 一致。
 * - 监控机：抓包口（`capture`）→ 下；管理口 → 左。
 * - 其他主机：只有一个入向口 → 左。
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

/** handle 的位移要按侧单独给（见 IfaceChip 里的说明）：
 *  左右口只居中 y、不横向平移；下口只居中 x、不纵向平移。 */
const SIDE_TF: Record<string, string> = {
  l: "translate(0, -50%)",
  r: "translate(0, -50%)",
  b: "translate(-50%, 0)",
};

/** 网口 chip。handle 用 react-flow 的默认定位（左口 left:0、右口 right:0、
 *  下口 bottom:0 + 自身 -50% 偏移）——**正好落在 chip 的边缘上**。
 *
 *  以前的写法给 handle 额外加了 ±9px，线头就停在 chip 外面一格，看着像
 *  "接口位置和线头对不上"。要让线离 chip 远一点，应该挪 chip 容器
 *  （下面 `left: -8`），不是挪 handle。 */
function IfaceChip({ f, role }: { f: TopoIface; role: string | null }) {
  const side = sideOf(f, role);
  const tip = f.capture
    ? `${f.name} · 抓包（${f.net} 的流量）`
    : `${f.name} · ${f.net}${f.ip ? ` ${f.ip.replace(/\/\d+$/, "")}` : ""}`;
  // 两个方向各挂一个 handle：同一条 chip 既可能是边的起点（主机→网段、
  // 主机→监控机），也可能是终点（网段→主机）。锚点同一个位置。
  //
  // `transform` 必须按侧覆盖：react-flow 默认 `translate(-50%,-50%)`，而它算
  // 锚点时取 handle 的**外沿**（left 取 x、right 取 x+width、bottom 取
  // y+height）。默认位移下 6px 的 handle 会让锚点偏出 chip 边缘 3px——缩放
  // 70% 时就是"线头没贴住网口"。改成只居中对侧后，锚点正好落在 chip 边上。
  const handles = (["s", "t"] as const).map((t) => (
    <Handle
      key={t}
      type={t === "s" ? "source" : "target"}
      id={handleId(t, side, f.name)}
      position={SIDE_POS[side]}
      style={{ width: 6, height: 6, opacity: 0, border: "none", transform: SIDE_TF[side] }}
    />
  ));
  return (
    <div className={`iface${f.capture ? " iface-cap" : ""}`} title={tip}>
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
    // 网段卡：**左进右出**。左边收上联（本 zone 网关的内网口 / 父网段挂过来
    // 的线），右边扇出到下挂设备。没有底部出点——网络连线不从下方走。
    return (
      <>
        <Handle
          type="target"
          position={Position.Left}
          id={handleId("t", "l", "net")}
          style={{ opacity: 0, border: "none", transform: "translate(0, -50%)" }}
        />
        <Handle
          type="source"
          position={Position.Right}
          id={handleId("s", "r", "net")}
          style={{ opacity: 0, border: "none", transform: "translate(0, -50%)" }}
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
            {m("gateway_host")
              ? ` · 网关 ${m("gateway_host")}`
              : m("uplink_gw")
                ? ` · 网关 ${m("uplink_gw")}`
                : ""}
          </div>
        </div>
      </>
    );
  }

  // 主机：网口按"朝自己网段"分侧——左入右出（网关 eth0）、下抓包
  const left = data.ifaces.filter((f) => sideOf(f, data.role) === "l");
  const right = data.ifaces.filter((f) => sideOf(f, data.role) === "r");
  const bottom = data.ifaces.filter((f) => sideOf(f, data.role) === "b");
  return (
    // 外层只做定位容器：卡片自己有 overflow:hidden（截断长标题），网口 chip
    // 又必须探出卡片边缘——放进卡片里就会被裁掉，所以兄弟节点摆放。
    <div className="tn-wrap" style={{ width: dim.w, height: dim.h }}>
      <div className={cls} style={{ width: dim.w, height: dim.h }}>
        <div className="tn-row">
          <span className="tn-dot" data-st={data.retired ? "retired" : data.status} />
          <span className="tn-title">{data.label}</span>
          <span className="tn-chip">{ROLE_LABEL[data.role ?? ""] ?? data.role}</span>
        </div>
        <div className="tn-sub mono">
          {data.zone && <span className="tn-zone">{data.zone} · </span>}
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
          {typeof meta.declared === "string" && <span> · 声明 {String(meta.declared)}</span>}
        </div>
      </div>
      {/* 分侧网口：chip 吸在卡片边缘、向外突出 8px，handle 在 chip 边沿 */}
      <div className="iface-col" style={{ left: -8, top: 14 }}>
        {left.map((f) => (
          <IfaceChip key={f.name} f={f} role={data.role} />
        ))}
      </div>
      <div className="iface-col" style={{ right: -8, top: 14, alignItems: "flex-end" }}>
        {right.map((f) => (
          <IfaceChip key={f.name} f={f} role={data.role} />
        ))}
      </div>
      <div className="iface-row" style={{ bottom: -9 }}>
        {bottom.map((f) => (
          <IfaceChip key={f.name} f={f} role={data.role} />
        ))}
      </div>
    </div>
  );
}

/** 正交边：全部横着走。
 *
 * 为什么要自己算路径：内置 smoothstep 不避障（实测穿节点十几次），而且
 * 折点是"两端中点"，同一个网段扇出 5 条线时会在走廊里完全重合——hover
 * 只能压在最上面那条上，看着像"线分叉"。
 *
 * 两种走法：
 *   - 跨列（右→左）：出 10px → 走廊里的本车道竖线 → 横进目标口
 *   - 镜像（口→底部抓包口）：出 13px → 下到画布底部走廊 → 横移 → 上顶进抓包口
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
  const d0 = (data ?? {}) as {
    corridorY?: number;
    lane?: number;
    laneStep?: number;
    mirSlot?: number;
    dim?: boolean;
  };
  const lbl = typeof (data ?? {}).label === "string" ? (data as { label: string }).label : "";

  const pts: [number, number][] = [[sourceX, sourceY]];
  const push = (x: number, y: number) => {
    const last = pts[pts.length - 1];
    if (Math.abs(last[0] - x) > 0.5 || Math.abs(last[1] - y) > 0.5) pts.push([x, y]);
  };

  if (targetPosition === Position.Bottom) {
    // 镜像线：从网口外沿出来 → 沉到底部走廊 → 横移 → 从下方顶进抓包口。
    // 同一列同侧的口 x 完全一样，靠 mirSlot 依次外错几像素，才看得出是几条线。
    const slot = typeof d0.mirSlot === "number" ? d0.mirSlot : 0;
    const dir = sourcePosition === Position.Left ? -1 : sourcePosition === Position.Right ? 1 : 0;
    const off = dir * (GEO.mir + slot * GEO.mirStep);
    const cy = typeof d0.corridorY === "number" ? d0.corridorY : targetY + GEO.corridor;
    push(sourceX + off, sourceY);
    push(sourceX + off, cy);
    push(targetX, cy);
    push(targetX, targetY);
  } else if (sourcePosition === Position.Right && targetPosition === Position.Left) {
    // 跨列：走廊里按车道竖着走。车道号由 assemble 按目标 y 排序给出，
    // 越靠上的目标越贴近目标列——扇出的线因此是"嵌套"的，互不交叉。
    // laneStep 由 assemble 按该源口扇出的条数摊算：条数多时缩小间距，否则
    // 会挤到卡片边上被 clamp，几条线重合、hover 只能选中最上面那条。
    const lane = typeof d0.lane === "number" ? d0.lane : 0;
    const step = typeof d0.laneStep === "number" ? d0.laneStep : GEO.laneStep;
    if (Math.abs(targetY - sourceY) < 1) {
      push(targetX, targetY);
    } else {
      const laneX = Math.max(targetX - GEO.lane0 - lane * step, sourceX + 10);
      push(laneX, sourceY);
      push(laneX, targetY);
      push(targetX, targetY);
    }
  } else if (sourcePosition === Position.Left) {
    // 兜底（布局异常时）：往左让一格再横走
    const laneX = Math.min(sourceX - GEO.lane0, targetX + 10);
    push(laneX, sourceY);
    push(laneX, targetY);
    push(targetX, targetY);
  } else {
    const midX = (sourceX + targetX) / 2;
    push(midX, sourceY);
    push(midX, targetY);
    push(targetX, targetY);
  }

  const d = pts.map((q, i) => `${i ? "L" : "M"} ${q[0]},${q[1]}`).join(" ");

  // 标签挂在最长一段的中点上（折线中点常常落在拐角上，看起来像飘着）
  let best: [[number, number], [number, number]] = [pts[0], pts[pts.length - 1]];
  let bestLen = -1;
  for (let i = 1; i < pts.length; i++) {
    const len = Math.abs(pts[i][0] - pts[i - 1][0]) + Math.abs(pts[i][1] - pts[i - 1][1]);
    if (len > bestLen) {
      bestLen = len;
      best = [pts[i - 1], pts[i]];
    }
  }
  const lp: [number, number] = [(best[0][0] + best[1][0]) / 2, (best[0][1] + best[1][1]) / 2];

  return (
    <>
      <path d={d} className="lab-path" fill="none" style={style} markerEnd={markerEnd} />
      {/* 加粗的透明命中区：细线也好点中。被淡化（正聚焦别的线）时不挂
          命中区——否则鼠标一移开就被旁边那条几乎看不见的线接住，表现为
          "凸显之后移出不会恢复"。 */}
      {!d0.dim && (
        <path d={d} fill="none" stroke="transparent" strokeWidth={11} style={{ pointerEvents: "stroke" }} />
      )}
      {lbl && (
        <EdgeLabelRenderer>
          <div
            className="edge-lbl"
            style={{ transform: `translate(-50%, -50%) translate(${lp[0]}px, ${lp[1]}px)` }}
          >
            {lbl}
          </div>
        </EdgeLabelRenderer>
      )}
    </>
  );
}

const edgeTypes = { lab: LabEdge };
const nodeTypes = { lab: LabNode };

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
      strokeWidth: hot ? 2.2 : 1.3,
      strokeDasharray: "5 4",
      opacity: hot ? 1 : 0.5,
    },
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

/** 镜像边的 id。带抓包口名：同一台源主机可能被复制到同一台监控机的
 *  多个抓包口上，只用 `from->to` 会撞 id，React Flow 会丢掉一条。 */
function mirrorEdgeId(e: TopoEdge): string {
  return `mirror:${e.from}->${e.to}:${e.target_iface ?? ""}`;
}

type Layout = {
  pos: Map<string, { x: number; y: number }>;
  /** 节点在第几列（0 起）。跨列线的车道、镜像线的错位都按它算。 */
  colOf: Map<string, number>;
  /** 画布底部走廊：镜像竖线到这里汇成一条总干 */
  corridorY: number;
  /** 每台监控机一条横向通道（多台监控机时上下错开） */
  laneOf: Map<string, number>;
};

/** 布局：**一列一网段、一列设备，交替**。
 *
 *   col0 外网 │ col1 [网关 监控机 主机] │ col2 内网 │ col3 [网关 监控机 主机] │ …
 *
 * 列号：网段 = 父网段 + 2（中间那列留给它的设备）；网关在本网段左边一列
 * （它的 eth1 朝左接上层网段、eth0 朝右接自己的网段），其他主机在右边一列。
 * 因此所有 attach 边都是"右→左"的横向线，网络连线既不会从卡下方出来，
 * 也不会掉头。
 *
 * 纵向：每列按"左邻列连接对象的高度"排——同一网段的设备挨在一起、
 * 网关对着它的网段。监控机排在本列最底：它的抓包口在卡片下方，采集线要
 * 从下面顶上来，底下不能压着别的卡。
 */
function layoutPositions(nodes: TopoNode[], edges: TopoEdge[]): Layout {
  const uniq = uniqNodes(nodes);
  const zoneOf = (n: TopoNode) => (n.kind === "network" ? n.id.slice(4) : (n.zone ?? ""));

  const parent = new Map<string, string>();
  for (const e of edges) {
    if (e.kind !== "attach" || e.port !== "uplink") continue;
    if (!e.from.startsWith("net:") || !e.to.startsWith("host:")) continue;
    parent.set(e.to.split(":")[1], e.from.slice(4));
  }
  const netCol = (zone: string) => {
    let c = 0;
    let cur = zone;
    for (let g = 0; g < 16 && parent.has(cur); g++) {
      cur = parent.get(cur)!;
      c += 2;
    }
    return c;
  };

  const colOf = new Map<string, number>();
  for (const n of uniq.values()) {
    if (n.kind === "network") colOf.set(n.id, netCol(zoneOf(n)));
    else {
      const c = netCol(zoneOf(n));
      colOf.set(n.id, n.role === "gateway" ? Math.max(0, c - 1) : c + 1);
    }
  }

  // 纵向对齐的参照：右邻列的节点看着左邻列里与它相连的那个
  const refOf = new Map<string, string>();
  for (const e of edges) {
    if (e.kind !== "attach") continue;
    const ca = colOf.get(e.from);
    const cb = colOf.get(e.to);
    if (ca === undefined || cb === undefined) continue;
    if (cb > ca) refOf.set(e.to, e.from);
    else if (ca > cb) refOf.set(e.from, e.to);
  }

  const byCol = new Map<number, TopoNode[]>();
  for (const n of uniq.values()) {
    const c = colOf.get(n.id) ?? 0;
    const arr = byCol.get(c) ?? [];
    arr.push(n);
    byCol.set(c, arr);
  }
  const maxCol = Math.max(0, ...byCol.keys());

  const pos = new Map<string, { x: number; y: number }>();
  const rank = (n: TopoNode) => (n.role === "gateway" ? 0 : n.role === "monitor" ? 2 : 1);
  let bottom = GEO.pad;

  for (let c = 0; c <= maxCol; c++) {
    const list = byCol.get(c) ?? [];
    if (!list.length) continue;

    const wantY = (n: TopoNode): number | null => {
      const r = refOf.get(n.id);
      const rp = r ? pos.get(r) : undefined;
      const rn = r ? uniq.get(r) : undefined;
      if (!rp || !rn) return null;
      return rp.y + DIM[rn.kind].h / 2 - DIM[n.kind].h / 2;
    };

    list.sort((a, b) => {
      // 监控机沉底（采集竖线从下方顶上来，下面不能有卡）
      const ma = a.role === "monitor" ? 1 : 0;
      const mb = b.role === "monitor" ? 1 : 0;
      if (ma !== mb) return ma - mb;
      const wa = wantY(a);
      const wb = wantY(b);
      if (wa !== null && wb !== null && Math.abs(wa - wb) > 1) return wa - wb;
      return rank(a) - rank(b) || a.label.localeCompare(b.label);
    });

    const x = GEO.pad + c * (DIM.network.w + GEO.colGap);
    let y = GEO.pad;
    for (const n of list) {
      const w = wantY(n);
      if (w !== null) y = Math.max(y, w);
      pos.set(n.id, { x: x + (DIM.network.w - DIM[n.kind].w) / 2, y });
      y += DIM[n.kind].h + GEO.rowGap;
    }
    bottom = Math.max(bottom, y - GEO.rowGap);
  }

  // 每台监控机一条横向采集通道（按列、再按上下顺序）
  const monitors = [...uniq.values()]
    .filter((n) => n.role === "monitor")
    .sort((a, b) => (pos.get(a.id)?.y ?? 0) - (pos.get(b.id)?.y ?? 0))
    .map((n) => n.id);
  const laneOf = new Map<string, number>();
  monitors.forEach((id, i) => laneOf.set(id, i));

  return { pos, colOf, corridorY: bottom + GEO.corridor, laneOf };
}

function assemble(
  nodes: TopoNode[],
  edges: TopoEdge[],
  layout: Layout,
  selected: Set<string>,
  hotMirror: Set<string>,
  dimmed: Set<string>,
  /** 当前悬停的边：只点亮它、淡化其余线与无关节点 */
  hoverEdge: string | null,
): { nodes: GNode[]; edges: Edge[] } {
  const uniq = uniqNodes(nodes);

  /** 某个节点上某块网口在哪个侧（不在清单里就兜底左侧） */
  const sideOfOn = (nodeId: string, iface: string): string => {
    const n = uniq.get(nodeId);
    const f = n?.ifaces?.find((x) => x.name === iface);
    return f ? sideOf(f, n?.role ?? null) : "l";
  };

  /** 某节点上第一块（非）抓包网口——边没写 iface 时兜底，别写死 eth1：
   *  多抓包口的监控机、改过编号的老规格都会因此把线连到不存在的 handle。 */
  const ifaceOn = (nodeId: string, capture: boolean): string | null => {
    const list = uniq.get(nodeId)?.ifaces ?? [];
    return (capture ? list.find((f) => f.capture) : list.find((f) => !f.capture))?.name ?? null;
  };

  // ── handle 解析：网段卡没有物理网口，左进右出 ──
  const srcHandle = (e: TopoEdge): string | undefined => {
    if (e.source_iface) return handleId("s", sideOfOn(e.from, e.source_iface), e.source_iface);
    if (e.from.startsWith("net:")) return handleId("s", "r", "net");
    const f = ifaceOn(e.from, false);
    return f ? handleId("s", sideOfOn(e.from, f), f) : undefined;
  };
  const tgtHandle = (e: TopoEdge): string | undefined => {
    if (e.target_iface) return handleId("t", sideOfOn(e.to, e.target_iface), e.target_iface);
    if (e.to.startsWith("net:")) return handleId("t", "l", "net");
    const f = ifaceOn(e.to, false);
    return f ? handleId("t", sideOfOn(e.to, f), f) : undefined;
  };

  const yOf = (id: string) => layout.pos.get(id)?.y ?? 0;

  // 车道：同一个源口出去的多条线，按目标高低编号；越靠上的目标越贴目标列。
  // 不编号的话它们会在走廊里完全重合，hover 只能选中压在上面那条。
  // 间距按扇出条数摊算，条数多时自动收窄，避免最后几条挤到卡片边上重合。
  const laneOf = new Map<string, number>();
  const laneStepOf = new Map<string, number>();
  {
    const bySrc = new Map<string, TopoEdge[]>();
    for (const e of edges) {
      if (e.kind !== "attach") continue;
      const arr = bySrc.get(e.from) ?? [];
      arr.push(e);
      bySrc.set(e.from, arr);
    }
    for (const [src, arr] of bySrc) {
      arr.sort((a, b) => yOf(a.to) - yOf(b.to));
      arr.forEach((e, i) => laneOf.set(`${e.from}|${e.to}`, i));
      // 走廊可用宽度 = 列间距 - 第一条车道占位 - 两侧留白
      const room = Math.max(0, GEO.colGap - GEO.lane0 - 8);
      const step = arr.length > 1 ? Math.min(GEO.laneStep, Math.max(5, room / (arr.length - 1))) : GEO.laneStep;
      laneStepOf.set(src, step);
    }
  }

  // 同一列、同一侧的被镜像主机：竖线依次外错，不然它们完全重合。
  // 按 **每条边** 分侧编号（不是每台主机）：网关的 eth0 在右、普通主机在左，
  // 一台机器两侧都被镜像时，如果只按主机编号，两侧会互相覆盖槽位。
  const mirSlot = new Map<string, number>();
  {
    const bySide = new Map<string, string[]>();
    for (const e of edges) {
      if (e.kind !== "mirror") continue;
      const sIf = e.source_iface ?? ifaceOn(e.from, false);
      const c = layout.colOf.get(e.from);
      if (c === undefined || !sIf) continue;
      const k = `${c}:${sideOfOn(e.from, sIf)}`;
      const arr = bySide.get(k) ?? [];
      if (!arr.includes(e.from)) arr.push(e.from);
      bySide.set(k, arr);
    }
    for (const [k, arr] of bySide) {
      arr.sort((a, b) => yOf(a) - yOf(b)).forEach((id, i) => mirSlot.set(`${k}|${id}`, i));
    }
  }

  const rfEdges: Edge[] = [];

  // ── 镜像线：源主机的网口 → 监控机的抓包口 ──
  // 一艘主机一条线，同一台监控机的线共用一条底部横向通道，看着是一根总线，
  // 但每条都能单独 hover（"这台机器到底被谁在抓"要能问得出来）。
  for (const e of edges) {
    if (e.kind !== "mirror") continue;
    if (!uniq.has(e.from) || !uniq.has(e.to) || e.from === e.to) continue;
    const id = mirrorEdgeId(e);
    const lane = layout.laneOf.get(e.to) ?? 0;
    // 两端都按名称解析出**真实存在**的网口：规格里缺 iface 时回落到该机第一块
    // （非）抓包口。写死 eth1 的话，多抓包口/改过编号的机器会连到不存在的
    // handle，React Flow 悄悄把线拉到节点正中，看着像"线穿卡"。
    const sIf = e.source_iface ?? ifaceOn(e.from, false);
    const side = sIf ? sideOfOn(e.from, sIf) : "l";
    const slot = mirSlot.get(`${layout.colOf.get(e.from) ?? 0}:${side}|${e.from}`) ?? 0;
    const tIf = e.target_iface ?? ifaceOn(e.to, true);
    const ts = tIf ? sideOfOn(e.to, tIf) : "b";
    rfEdges.push({
      id,
      source: e.from,
      target: e.to,
      sourceHandle: sIf ? handleId("s", side, sIf) : undefined,
      targetHandle: tIf ? handleId("t", ts, tIf) : undefined,
      type: "lab",
      data: {
        // 错位靠"起点外移"实现：同列同侧的口 x 一样，错开才看得清是几条线
        corridorY: layout.corridorY + lane * GEO.busLane,
        mirSlot: slot,
        tip: `镜像 ${e.from.split(":").slice(1).join(":")}:${sIf ?? "?"} → ${e.to
          .split(":")
          .slice(1)
          .join(":")}:${tIf ?? "?"}`,
        from: e.from,
        to: e.to,
      },
      ...mirrorSpec(hotMirror.has(id)),
    } as Edge);
  }

  // ── attach 边：端点锚在具体网口的 chip 上 ──
  let seq = 0;
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
      data: {
        lane: laneOf.get(`${e.from}|${e.to}`) ?? 0,
        laneStep: laneStepOf.get(e.from) ?? GEO.laneStep,
        tip: `${e.from.split(":").slice(1).join(":")}${e.source_iface ? ":" + e.source_iface : ""} → ${e.to
          .split(":")
          .slice(1)
          .join(":")}${e.target_iface ? ":" + e.target_iface : ""}${
          e.port === "uplink" ? "（上联）" : ""
        }`,
        from: e.from,
        to: e.to,
      },
      style: { stroke: spec.stroke, strokeWidth: spec.strokeWidth },
      markerEnd: spec.marker
        ? { type: MarkerType.ArrowClosed, color: spec.stroke, width: 14, height: 14 }
        : undefined,
      zIndex: e.port === "uplink" ? 2 : 1,
    } as Edge);
  }

  // ── 悬停聚焦：点亮这一条、压暗其余，并显示它的接口标签 ──
  // 悬停某个节点走另一条分支：点亮与它相关的镜像线，其余线**也**压暗——
  // 只淡节点不淡线的话，线还亮着，看着像"淡了一半"，对比很突兀。
  const dimByNodeFocus = !hoverEdge && hotMirror.size > 0;
  const focused = hoverEdge
    ? rfEdges.map((e) => {
        const foc = e.id === hoverEdge;
        const d = (e.data ?? {}) as { tip?: string };
        return {
          ...e,
          style: {
            ...(e.style ?? {}),
            opacity: foc ? 1 : 0.08,
          },
          zIndex: foc ? 999 : e.zIndex,
          data: {
            ...e.data,
            dim: !foc,
            label: foc ? (d.tip ?? "") : "",
          },
        } as Edge;
      })
    : dimByNodeFocus
      ? rfEdges.map((e) =>
          hotMirror.has(e.id)
            ? e
            : ({
                ...e,
                style: { ...(e.style ?? {}), opacity: 0.22 },
              } as Edge),
        )
      : rfEdges;

  // 节点：悬停某条线时只保留它的两端（其余淡化），否则沿用 hover 监控机的淡化
  const keep = new Set<string>();
  if (hoverEdge) {
    const e = focused.find((x) => x.id === hoverEdge);
    const d = (e?.data ?? {}) as { from?: string; to?: string };
    if (d.from) keep.add(d.from);
    if (d.to) keep.add(d.to);
  }

  const rfNodes: GNode[] = [];
  for (const n of uniq.values()) {
    const d = DIM[n.kind];
    const p = layout.pos.get(n.id) ?? { x: 0, y: 0 };
    rfNodes.push({
      id: n.id,
      type: "lab",
      position: p,
      data: {
        kind: n.kind,
        label: n.label,
        zone: n.zone ?? null,
        role: n.role,
        ipv4: n.ipv4,
        status: n.status,
        retired: n.retired,
        vmidLabel: n.vmid ? `CT ${n.vmid}` : "",
        meta: n.meta as Record<string, unknown>,
        ifaces: n.ifaces ?? [],
        selected: selected.has(n.id),
        dimmed: hoverEdge ? !keep.has(n.id) : dimmed.has(n.id),
      },
      width: d.w,
      height: d.h,
    });
  }

  return { nodes: rfNodes, edges: focused };
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
  // 悬停监控机 = 它吃进来的镜像线全亮；悬停某台主机 = 它被谁抓；悬停某条
  // 线 = 只亮那条。
  //
  // 退出用"延迟清除"：鼠标从节点滑到贴在它边缘的 chip/连线上时，浏览器会
  // 先发 mouseleave 再发 mouseenter，立刻置空就表现为"高亮闪一下就没"。
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
    }, 140);
  };
  const clearNow = () => {
    cancelLeave();
    setHoverNode(null);
    setHoverEdge(null);
  };
  useEffect(() => cancelLeave, []);

  /** 悬停某个节点时"相关的一小片"：它自己 + 镜像线的另一端。
   *  普通主机也走这条路——以前只有监控机才淡化无关节点，悬停普通主机会出现
   *  "线全暗了、卡片还亮着"的半淡状态，看着很突兀。 */
  const nodeFocus = useMemo(() => {
    const hot = new Set<string>();
    const keep = new Set<string>();
    if (!hoverNode) return { hot, keep };
    keep.add(hoverNode);
    edges.forEach((e) => {
      if (e.kind !== "mirror" || (e.from !== hoverNode && e.to !== hoverNode)) return;
      hot.add(mirrorEdgeId(e));
      keep.add(e.from);
      keep.add(e.to);
    });
    return { hot, keep };
  }, [edges, hoverNode]);

  const hotMirror = useMemo(
    () => (hoverEdge ? new Set([hoverEdge]) : nodeFocus.hot),
    [hoverEdge, nodeFocus],
  );

  const dimmed = useMemo(() => {
    // 只悬停一条线时由 assemble 处理节点淡化；悬停节点但没有任何镜像关系
    // （比如一台没被抓包的普通主机）就什么都不淡——整图突然变灰更吓人。
    if (hoverEdge || !hoverNode || nodeFocus.hot.size === 0) return new Set<string>();
    const out = new Set<string>();
    nodes.forEach((n) => {
      if (!nodeFocus.keep.has(n.id)) out.add(n.id);
    });
    return out;
  }, [nodes, hoverNode, hoverEdge, nodeFocus]);

  // ── 手工布局：只在结构变化时重算（纯函数，同步出结果）──
  const structKey = useMemo(() => {
    const nk = nodes.map((n) => n.id).sort().join(",");
    const ek = edges
      .map((e) => `${e.kind}:${e.from}>${e.to}`)
      .sort()
      .join(",");
    return `${nk}#${ek}`;
  }, [nodes, edges]);

  const [layout, setLayout] = useState<Layout>({
    pos: new Map(),
    colOf: new Map(),
    corridorY: 0,
    laneOf: new Map(),
  });
  const fitted = useRef(false);

  useEffect(() => {
    try {
      setLayout(layoutPositions(nodes, edges));
    } catch {
      // 布局失败就保持旧位置：图乱一点总比消失好
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [structKey]);

  const built = useMemo(
    () => assemble(nodes, edges, layout, sel, hotMirror, dimmed, hoverEdge),
    [nodes, edges, layout, sel, hotMirror, dimmed, hoverEdge],
  );

  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode>([]);
  const [rfEdges, setRfEdges, onEdgesChange] = useEdgesState<Edge>([]);

  // 布局引用记一笔：换了新 Layout 对象 → 全量采用它的位置（结构变了）；
  // 引用没变（轮询刷新 / hover 高亮重建）→ 保留 RF 内部当前位置（用户拖过
  // 的）。初始 layout 是空地图，第一次 built 的节点全在 (0,0)，若不区分，
  // 真实位置会被这批 (0,0) 盖住——所有节点叠成一坨。
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
    // 首次拿到布局后取景：取景前必须确认**节点已经拿到真实坐标**，否则
    // fitView 会把视口放大到一个点（表现是"图里只剩一个节点、贴在角落"）。
    const placed = built.nodes.some((n) => n.position.x !== 0 || n.position.y !== 0);
    if (!fitted.current && built.nodes.length > 0 && placed) {
      fitted.current = true;
      // 取景要把**采集走廊**一起框进去：fitView 只量节点，而镜像线要落到
      // 节点下方 44~100px 才汇成总线——图一高，底部总线就被画布下沿裁掉。
      const laneMax = Math.max(0, ...layout.laneOf.values());
      const x0 = Math.min(...built.nodes.map((n) => n.position.x));
      const y0 = Math.min(...built.nodes.map((n) => n.position.y));
      const x1 = Math.max(...built.nodes.map((n) => n.position.x + (n.width ?? 0)));
      const y1 = Math.max(
        ...built.nodes.map((n) => n.position.y + (n.height ?? 0)),
        layout.corridorY + laneMax * GEO.busLane + 8,
      );
      const bounds = { x: x0, y: y0, width: x1 - x0, height: y1 - y0 };
      // rAF 回调可能跑在 React 把新节点提交进 React Flow store **之前**
      // ——量到的还是上一轮全在 (0,0) 的节点堆。所以先问 store 要一次节点：
      // 没就绪就下一帧再试，最多等半秒。
      const tryFit = (tries: number) => {
        const ns = rf.getNodes();
        const ready = ns.length > 0 && ns.some((n) => n.position.x !== 0 || n.position.y !== 0);
        if (!ready) {
          if (tries > 0) requestAnimationFrame(() => tryFit(tries - 1));
          return;
        }
        // padding 传数字是**像素**（不是比例）：0.05 只有 0.05px，节点会贴边。
        rf.fitBounds(bounds, { padding: 10, duration: 200 });
      };
      requestAnimationFrame(() => tryFit(30));
    }
  }, [built, setRfNodes, setRfEdges, layout, rf]);

  const pick = (id: string | undefined) => {
    const orig = id ? (nodes.find((x) => x.id === id) ?? null) : null;
    onPick?.(orig);
  };

  return (
    // 鼠标离开整块画布时立刻收掉高亮（只靠边的 mouseleave 有边缘情况）
    <div className="topo-canvas" onMouseLeave={clearNow}>
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        nodeTypes={nodeTypes}
        edgeTypes={edgeTypes}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onSelectionChange={({ nodes: ns }) => onSelect?.(ns.map((n) => n.id))}
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
        onNodeClick={(_, n) => pick(n.id)}
        onPaneClick={() => {
          clearNow();
          pick(undefined);
        }}
        onNodeMouseEnter={(_, n) => {
          cancelLeave();
          setHoverNode(n.id);
          setHoverEdge(null);
        }}
        onNodeMouseLeave={() => scheduleLeave()}
        onEdgeMouseEnter={(_, e) => {
          cancelLeave();
          setHoverNode(null);
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
