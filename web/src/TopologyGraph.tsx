/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给的是两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。这里做四件事：
 *   1. 用 **elkjs**（Eclipse Layout Kernel）算层级与位置，只吃 attach 边——
 *      镜像线参与布局会把监控机推到所有主机后面一档。
 *      方向 LR：外网 → 网关 → 内网 → 主机。
 *   2. **网口分侧**：外网口贴左缘、内网口贴右缘、抓包口贴下缘，均略突出
 *      节点框——线从哪个口进出一眼可见。attach 边交给 ELK 的正交路由
 *      （`elk.edgeRouting: ORTHOGONAL`）输出折线，绕开节点，不穿模。
 *   3. 拖动不闪：数据轮询重建节点时**保留 RF 内部当前位置**，只换数据；
 *      拖动位置由 React Flow 自己管理，不经过重建路径。
 *   4. 接住点击、多选、右键，交给页面去开抽屉 / 弹菜单。
 */
import { useEffect, useMemo, useRef, useState } from "react";
import {
  Background,
  BackgroundVariant,
  BaseEdge,
  Controls,
  EdgeProps,
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
import ELK from "elkjs/lib/elk.bundled.js";
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

/** 网口在哪一侧：wan 口在左（外面从左边来），普通口在右（内网在右边），
 * 抓包口在下（镜像线从下面进监控机）。 */
function sideOf(f: TopoIface): "l" | "r" | "b" {
  if (f.capture) return "b";
  if (f.net === "wan") return "l";
  return "r";
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
function IfaceChip({ f }: { f: TopoIface }) {
  const side = sideOf(f);
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

  // 主机节点：网口分侧——wan 口贴左缘竖排、内网口贴右缘竖排、抓包口贴下缘横排
  const left = data.ifaces.filter((f) => sideOf(f) === "l");
  const right = data.ifaces.filter((f) => sideOf(f) === "r");
  const bottom = data.ifaces.filter((f) => sideOf(f) === "b");
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
            <IfaceChip key={f.name} f={f} />
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
            <IfaceChip key={f.name} f={f} />
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
            <IfaceChip key={f.name} f={f} />
          ))}
        </div>
      </div>
    </>
  );
}

const nodeTypes = { lab: LabNode };

/** ELK 正交路由的 attach 边：直接按 ELK 给的折点画 polyline——它绕开了节点。 */
function OrthEdge({
  data,
  markerEnd,
}: EdgeProps) {
  const pts: [number, number][] = (data?.pts as [number, number][] | undefined) ?? [];
  const d =
    pts.length >= 2
      ? pts.map((p, i) => `${i ? "L" : "M"}${p[0]},${p[1]}`).join(" ")
      : "";
  return (
    <BaseEdge
      path={d}
      markerEnd={markerEnd}
      style={{
        stroke: (data?.stroke as string) ?? "#b9c4d4",
        strokeWidth: (data?.strokeWidth as number) ?? 1.2,
      }}
    />
  );
}

const edgeTypes = { ortho: OrthEdge };

/** 边的画法参数：attach 走 ELK 正交路由，mirror 半透明虚线（smoothstep）。 */
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
  /** attach 边的正交折点（ELK ORTHOGONAL 路由输出，root 全局坐标） */
  paths: Map<string, [number, number][]>;
};

/** ELK 分层布局 + 正交路由，只吃 attach 边。 */
async function layoutPositions(nodes: TopoNode[], edges: TopoEdge[]): Promise<Layout> {
  const uniq = uniqNodes(nodes);
  const elk = new ELK();
  const children = [...uniq.values()].map((n) => {
    const d = DIM[n.kind] ?? DIM.host;
    return { id: n.id, width: d.w, height: d.h };
  });
  const elEdges = edges
    .filter((e) => e.kind === "attach" && e.from !== e.to && uniq.has(e.from) && uniq.has(e.to))
    .map((e) => ({ id: `${e.from}->${e.to}`, sources: [e.from], targets: [e.to] }));

  const res = await elk.layout({
    id: "root",
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": "RIGHT",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.spacing.nodeNode": "46",
      "elk.layered.spacing.nodeNodeBetweenLayers": "140",
      "elk.layered.nodePlacement.strategy": "BRANDES_KOEPF",
      "elk.layered.mergeEdges": "true",
      "elk.padding": "[top=40,left=60,bottom=40,right=60]",
    },
    children,
    edges: elEdges,
  });

  const pos = new Map<string, { x: number; y: number }>();
  for (const c of res.children ?? []) {
    if (typeof c.x === "number" && typeof c.y === "number") {
      pos.set(c.id, { x: c.x, y: c.y });
    }
  }
  // 收集每条 attach 边的折线：ELK 的 edge 输出顺序与输入一致，
  // id 是我们传的 `{from}->{to}`，sections 是 root 全局坐标
  const paths = new Map<string, [number, number][]>();
  for (const ee of res.edges ?? []) {
    const pts: [number, number][] = [];
    for (const s of (ee as { sections?: { startPoint: { x: number; y: number }; endPoint: { x: number; y: number }; bendPoints?: { x: number; y: number }[] }[] }).sections ?? []) {
      pts.push([s.startPoint.x, s.startPoint.y]);
      for (const b of s.bendPoints ?? []) pts.push([b.x, b.y]);
      pts.push([s.endPoint.x, s.endPoint.y]);
    }
    if (pts.length >= 2) paths.set(ee.id, pts);
  }
  return { pos, paths };
}

function assemble(
  nodes: TopoNode[],
  edges: TopoEdge[],
  layout: Layout,
  selected: Set<string>,
  hotMirror: Set<string>,
  dimmed: Set<string>,
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

  /** 某个节点上某块网口的 handle side（不在清单里就兜底右侧） */
  const sideOfOn = (nodeId: string, iface: string): string => {
    const f = uniq.get(nodeId)?.ifaces?.find((x) => x.name === iface);
    return f ? sideOf(f) : "r";
  };

  const rfEdges: Edge[] = edges
    .filter(
      (e) =>
        uniq.has(e.from) &&
        uniq.has(e.to) &&
        // 监控机自己的内网口 → 自己的抓包口：同节点自环线画出来只会是一团
        // 乱麻，语义在 chip 的"抓包"标注与抽屉清单里已经完整，图上省略。
        e.from !== e.to,
    )
    .map((e, i) => {
      const id = `${e.kind}:${e.from}->${e.to}:${i}`;
      if (e.kind === "attach") {
        // attach：ELK 正交路由的折点直接画，handle 只作锚（正交边忽略它）
        const spec = attachSpec(e);
        return {
          id,
          source: e.from,
          target: e.to,
          type: "ortho",
          data: {
            pts: layout.paths.get(`${e.from}->${e.to}`),
            stroke: spec.stroke,
            strokeWidth: spec.strokeWidth,
          },
          markerEnd: spec.marker
            ? { type: MarkerType.ArrowClosed, color: spec.stroke, width: 14, height: 14 }
            : undefined,
          zIndex: e.port === "uplink" ? 2 : 1,
        } as Edge;
      }
      // mirror：从源口所在侧出、进目标口所在侧（抓包口在监控机下缘）
      const ss = e.source_iface ? sideOfOn(e.from, e.source_iface) : "r";
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
    });

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
  const [hoverNode, setHoverNode] = useState<string | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);

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
    if (hoverNode && mirrorIds.has(hoverNode)) return new Set(mirrorIds.get(hoverNode));
    return new Set<string>();
  }, [hoverEdge, hoverNode, mirrorIds]);

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

  // ── ELK 布局：只在结构变化时异步重算 ──
  const structKey = useMemo(() => {
    const nk = nodes.map((n) => n.id).sort().join(",");
    const ek = edges
      .map((e) => `${e.kind}:${e.from}>${e.to}`)
      .sort()
      .join(",");
    return `${nk}#${ek}`;
  }, [nodes, edges]);

  const [layout, setLayout] = useState<Layout>({ pos: new Map(), paths: new Map() });
  const fitted = useRef(false);

  useEffect(() => {
    let alive = true;
    (async () => {
      try {
        const l = await layoutPositions(nodes, edges);
        if (!alive) return;
        setLayout(l);
      } catch {
        // 布局失败就保持旧位置：图乱一点总比消失好
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [structKey]);

  const built = useMemo(
    () => assemble(nodes, edges, layout, sel, hotMirror, dimmed),
    [nodes, edges, layout, sel, hotMirror, dimmed],
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
    if (!fitted.current && built.nodes.length > 0 && layout.pos.size > 0) {
      fitted.current = true;
      requestAnimationFrame(() => rf.fitView({ padding: 0.16, duration: 200 }));
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
        onNodeMouseEnter={(_, n) => setHoverNode(n.id)}
        onNodeMouseLeave={() => setHoverNode(null)}
        onEdgeMouseEnter={(_, e) => setHoverEdge(e.id)}
        onEdgeMouseLeave={() => setHoverEdge(null)}
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
          nodeColor={(n) => {
            const d = n.data as Data;
            if (d?.kind === "network") return d.meta?.external ? "#a5b4fc" : "#ddd6fe";
            if (d?.role === "gateway") return "#bbf7d0";
            if (d?.role === "monitor") return "#bfdbfe";
            return "#e2e8f0";
          }}
          maskColor="rgba(245,246,248,0.72)"
          style={{ background: "#ffffff", border: "1px solid var(--border)" }}
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
