/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给的是两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。这里做四件事：
 *   1. **一列一个 zone** 的手工分层布局（见 `layoutPositions`）：左列上游、
 *      右列下游，列内从上到下是"网段 → 网关 → 监控机 → 主机"。
 *      不靠自动布局：层级算法把网段节点当普通节点排，一条链路能撑开三四列。
 *   2. **网口分侧**：外网口贴左缘、内网口贴右缘、抓包口贴下缘，均略突出
 *      节点框——线从哪个口进出一眼可见。路径交给 React Flow 按 handle 现算
 *      （smoothstep），拖节点时线跟着走。
 *   3. 拖动不闪：数据轮询重建节点时**保留 RF 内部当前位置**，只换数据；
 *      拖动位置由 React Flow 自己管理，不经过重建路径。
 *   4. 接住点击、多选、右键，交给页面去开抽屉 / 弹菜单。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
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
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import type { TopoEdge, TopoIface, TopoNode } from "./api";

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致，否则连线会悬空。
 * host 高度含底部的抓包口 chip 行；左右缘的网口 chip 是突出的，不算宽。 */
const DIM: Record<TopoNode["kind"], { w: number; h: number }> = {
  network: { w: 190, h: 70 },
  host: { w: 200, h: 118 },
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
    // 网络节点（以及兜底）：左侧收、右侧发
    return (
      <>
        <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
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
        <Handle type="source" position={Position.Right} style={{ opacity: 0 }} />
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

function mirrorSpec(e: TopoEdge, hot: boolean): Partial<Edge> {
  return {
    style: {
      stroke: "#f59e0b",
      strokeWidth: hot ? 2.2 : 1.4,
      strokeDasharray: "5 4",
      opacity: hot ? 1 : 0.45,
    },
    label: e.label ?? undefined,
    labelStyle: { fontSize: 10.5, fill: "#b45309", opacity: hot ? 1 : 0.7 },
    labelBgStyle: { fill: "#fffbeb", fillOpacity: 0.92 },
    labelBgPadding: [4, 2] as [number, number],
    labelBgBorderRadius: 4,
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

type Layout = {
  pos: Map<string, { x: number; y: number }>;
};

/** 手工分层布局：**一列一个 zone**，列内从上到下是"网段 → 网关 → 监控机 → 主机"。
 *
 * 为什么不用 ELK：层级布局把网段节点当成普通节点参与排序，结果 gw 在网段
 * 左边、主机在右边，一条链路被拉成三四列，5 个 zone 就撑到近 2000px 宽——
 * fitView 只能缩到 0.4，"看清楚整张图"和"看清一台机器"两头都做不到。
 * 可读性靠**列语义**而不是自动优化：左列是上游，右列是下游，同列同网段。
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

  // zone → 该 zone 的节点（网段节点 + 主机）
  const zones = new Map<string, TopoNode[]>();
  for (const n of uniq.values()) {
    const z = zoneOf(n);
    const arr = zones.get(z) ?? [];
    arr.push(n);
    zones.set(z, arr);
  }

  const rank = (n: TopoNode) => {
    if (n.kind === "network") return 0;
    if (n.role === "gateway") return 1;
    if (n.role === "monitor") return 2;
    return 3;
  };

  // 列 → 该列的 zone 组。同列多个 zone（兄弟）按名字排，顺序稳定。
  const cols = new Map<number, string[]>();
  for (const z of zones.keys()) {
    const d = depthOf(z);
    const arr = cols.get(d) ?? [];
    arr.push(z);
    cols.set(d, arr);
  }
  for (const arr of cols.values()) arr.sort();

  const COL_W = DIM.host.w + 96;
  const GAP = 16;
  const GROUP_GAP = 34;

  // 先算每列高度，再整体垂直居中——否则矮列全挤在顶上，图看着"歪"
  const colHeights = new Map<number, number>();
  for (const [d, zs] of cols) {
    let h = 0;
    for (const z of zs) {
      const list = (zones.get(z) ?? []).slice().sort((a, b) => rank(a) - rank(b) || a.label.localeCompare(b.label));
      if (list.length === 0) continue;
      for (const n of list) h += DIM[n.kind].h + GAP;
      h += GROUP_GAP;
    }
    colHeights.set(d, h);
  }
  const maxH = Math.max(0, ...colHeights.values());

  const pos = new Map<string, { x: number; y: number }>();
  for (const [d, zs] of cols) {
    const x = 40 + d * COL_W;
    let y = 40 + Math.max(0, (maxH - (colHeights.get(d) ?? 0)) / 2);
    for (const z of zs) {
      const list = (zones.get(z) ?? []).slice().sort((a, b) => rank(a) - rank(b) || a.label.localeCompare(b.label));
      for (const n of list) {
        pos.set(n.id, { x, y });
        y += DIM[n.kind].h + GAP;
      }
      y += GROUP_GAP;
    }
  }
  return { pos };
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
): { nodes: GNode[]; edges: Edge[] } {
  const uniq = uniqNodes(nodes);

  const rfNodes: GNode[] = [];
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

  const rfEdges: Edge[] = [];

  /** 单条逐主机镜像线（源口 → 监控机抓包口）。聚合主干展开时才画。 */
  const perHostMirror = (e: TopoEdge, i: number): Edge => {
    const id = `${e.kind}:${e.from}->${e.to}:${i}`;
    const ss = e.source_iface ? sideOfOn(e.from, e.source_iface) : "l";
    const ts = e.target_iface ? sideOfOn(e.to, e.target_iface) : "b";
    return {
      id,
      source: e.from,
      target: e.to,
      sourceHandle: handleId("s", ss, e.source_iface ?? "eth0"),
      targetHandle: handleId("t", ts, e.target_iface ?? "eth1"),
      type: "smoothstep",
      ...mirrorSpec(e, hotMirror.has(id)),
    } as Edge;
  };

  // ── 整网采集：同网络的多条逐主机线合成一条主干 ──
  // 一个网络开监控时，图上会出现"每台主机 → 同一块抓包口"的 N 条虚线，
  // 全部横穿画布、互相压标签，一眼看不出这个网段到底归谁抓。语义上它本来
  // 就是网络级开关（开在网络节点上），所以主干画 网络 → 监控机抓包口，
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
      // 只有一台被采（精确连线）：保持逐主机的画法，语义更强
      singles.push(...list);
      continue;
    }
    const zone = k.split("->")[0];
    const head = list[0];
    const iface = head.target_iface ?? "eth1";
    const open = expand.has(k);
    const id = `mirror-agg:${k}`;
    rfEdges.push({
      id,
      source: `net:${zone}`,
      target: head.to,
      targetHandle: handleId("t", sideOfOn(head.to, iface), iface),
      type: "smoothstep",
      pathOptions: { borderRadius: 12, offset: 22 },
      style: {
        stroke: "#f59e0b",
        strokeWidth: open ? 2.4 : 1.6,
        strokeDasharray: "6 4",
        opacity: open ? 1 : 0.75,
      },
      label: `整网采集 · ${list.length} 台`,
      labelStyle: { fontSize: 10.5, fill: "#b45309" },
      labelBgStyle: { fill: "#fffbeb", fillOpacity: 0.94 },
      labelBgPadding: [5, 2] as [number, number],
      labelBgBorderRadius: 4,
      markerEnd: {
        type: MarkerType.ArrowClosed,
        color: "#f59e0b",
        width: 15,
        height: 15,
      },
      zIndex: open ? 6 : 3,
    } as Edge);
    if (open) {
      list.forEach((e, i) => rfEdges.push(perHostMirror(e, i)));
    }
  }

  singles.forEach((e, i) => rfEdges.push(perHostMirror(e, i)));

  // attach 边：端点锚在具体网口的 handle 上，让 React Flow 算路径。
  // 以前用 ELK 的绝对折点画 polyline，节点一拖线就留在原地，
  // 节点内容一变高端点还会悬空——看着像"线摆在那，没接上"。
  for (const e of edges) {
    if (e.kind !== "attach") continue;
    if (!uniq.has(e.from) || !uniq.has(e.to) || e.from === e.to) continue;
    const id = `attach:${e.from}->${e.to}:${seq++}`;
    const spec = attachSpec(e);
    const ss = e.source_iface
      ? handleId("s", sideOfOn(e.from, e.source_iface), e.source_iface)
      : undefined;
    const ts = e.target_iface
      ? handleId("t", sideOfOn(e.to, e.target_iface), e.target_iface)
      : undefined;
    rfEdges.push({
      id,
      source: e.from,
      target: e.to,
      sourceHandle: ss,
      targetHandle: ts,
      type: "smoothstep",
      pathOptions: { borderRadius: 10, offset: 16 },
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

  const [layout, setLayout] = useState<Layout>({ pos: new Map() });
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

  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode>([]);
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
        onPaneClick={() => pick(undefined)}
        // hover 高亮：监控机节点与镜像线都接住鼠标
        onNodeMouseEnter={(_, n) => {
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
