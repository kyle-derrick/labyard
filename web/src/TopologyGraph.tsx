/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给的是两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。这里做四件事：
 *   1. 用 **elkjs**（Eclipse Layout Kernel，成熟布局引擎）算层级与位置，
 *      只吃 attach 边——镜像线参与布局会把监控机推到所有主机后面一档。
 *      方向 LR（左→右）：外网 → 网关 → 内网 → 主机。
 *   2. 主机节点底部画出**每个网口**（chip），每枚 chip 左右各一对隐藏
 *      handle；每条边按两端节点的相对方位选"朝向对端的那一侧"——
 *      线永远从 chip 面向对端的一侧出来，不再绕圈、不再悬空。
 *   3. 把 kind / role / 状态映射成视觉
 *   4. 接住点击、多选、右键，交给页面去开抽屉 / 弹菜单
 *
 * **布局只在结构变化时重算**：轮询每 15 秒来一次数据，如果每次都重排，
 * 用户刚拖好的位置会被抹掉、图会跳一下。按"节点 id + 边集合"算结构签名，
 * 签名没变就只更新 data / 样式，位置沿用。
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
import ELK from "elkjs/lib/elk.bundled.js";
import type { TopoEdge, TopoIface, TopoNode } from "./api";

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致，否则连线会悬空。
 * host 高度含底部的网口 chip 行。 */
const DIM: Record<TopoNode["kind"], { w: number; h: number }> = {
  network: { w: 190, h: 70 },
  host: { w: 196, h: 94 },
};

export const ROLE_LABEL: Record<string, string> = {
  gateway: "网关",
  monitor: "流量监控",
  server: "服务",
  client: "终端",
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

/** 网口 chip。左右各一对不可见 handle：`sl`/`sr` 供出边、`tl`/`tr` 供入边，
 * 边按"对端在哪侧"选 handle，线贴着 chip 面向对端的那一侧进出。 */
function IfaceChip({ f }: { f: TopoIface }) {
  const tip = f.capture
    ? `${f.name} · 抓包（${f.net} 的流量）`
    : `${f.name} · ${f.net}${f.ip ? ` ${f.ip.replace(/\/\d+$/, "")}` : ""}`;
  const hs = (type: "source" | "target", side: "l" | "r") => (
    <Handle
      type={type}
      id={`${type[0]}${side}:${f.name}`}
      position={side === "l" ? Position.Left : Position.Right}
      style={{
        [side === "l" ? "left" : "right"]: -4,
        top: "50%",
        width: 6,
        height: 6,
        opacity: 0,
      }}
    />
  );
  return (
    <div className={`iface${f.capture ? " iface-cap" : ""}`} title={tip}>
      {hs("source", "l")}
      {hs("source", "r")}
      {hs("target", "l")}
      {hs("target", "r")}
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

  return (
    <>
      {/* 网络节点（以及兜底）：左侧收、右侧发 */}
      <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
      <div className={cls} style={{ width: dim.w, height: dim.h }}>
        {data.kind === "network" && (
          <>
            <div className="tn-row">
              <span className="tn-neticon" data-external={flag("external") ? "1" : "0"} />
              <span className="tn-title">{data.label}</span>
              <span className="tn-chips">
                {flag("nat") && <span className="tn-chip chip-nat">NAT</span>}
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
          </>
        )}

        {data.kind === "host" && (
          <>
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
            </div>
            <div className="ifaces">
              {data.ifaces.map((f) => (
                <IfaceChip key={f.name} f={f} />
              ))}
            </div>
          </>
        )}
      </div>
      <Handle type="source" position={Position.Right} style={{ opacity: 0 }} />
    </>
  );
}

const nodeTypes = { lab: LabNode };

/** 边的画法：attach 细灰实线（网关的上联口加粗成蓝），mirror 半透明虚线。 */
function edgeSpec(e: TopoEdge, hot: boolean): Partial<Edge> {
  if (e.kind === "mirror") {
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
  // attach：上联口（数据从外网进来那一段）画粗一点，其余细灰
  const uplink = e.port === "uplink";
  const color = uplink ? "#2f6feb" : "#b9c4d4";
  return {
    style: { stroke: color, strokeWidth: uplink ? 1.8 : 1.2 },
    markerEnd: uplink
      ? { type: MarkerType.ArrowClosed, color, width: 14, height: 14 }
      : undefined,
  };
}

/** 跨规格同名的 zone 会撞 id（后端一套 key 空间），这里按先出现的留下 */
function uniqNodes(nodes: TopoNode[]): Map<string, TopoNode> {
  const uniq = new Map<string, TopoNode>();
  for (const n of nodes) if (!uniq.has(n.id)) uniq.set(n.id, n);
  return uniq;
}

/** ELK 分层布局，只吃 attach 边。返回节点 id → 左上角坐标。 */
async function layoutPositions(
  nodes: TopoNode[],
  edges: TopoEdge[],
): Promise<Map<string, { x: number; y: number }>> {
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
      "elk.spacing.nodeNode": "40",
      "elk.layered.spacing.nodeNodeBetweenLayers": "130",
      "elk.layered.nodePlacement.strategy": "BRANDES_KOEPF",
      "elk.layered.mergeEdges": "true",
      "elk.padding": "[top=24,left=24,bottom=24,right=24]",
    },
    children,
    edges: elEdges,
  });

  const out = new Map<string, { x: number; y: number }>();
  for (const c of res.children ?? []) {
    if (typeof c.x === "number" && typeof c.y === "number") {
      out.set(c.id, { x: c.x, y: c.y });
    }
  }
  return out;
}

function assemble(
  nodes: TopoNode[],
  edges: TopoEdge[],
  pos: Map<string, { x: number; y: number }>,
  selected: Set<string>,
  hotMirror: Set<string>,
  dimmed: Set<string>,
): { nodes: GNode[]; edges: Edge[] } {
  const uniq = uniqNodes(nodes);

  const rfNodes: GNode[] = [];
  for (const n of uniq.values()) {
    const d = DIM[n.kind] ?? DIM.host;
    const p = pos.get(n.id) ?? { x: 0, y: 0 };
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
      // chip handle 按相对方位选：对端在右就用右侧 handle，在左用左侧。
      // 这样线从节点面向对端的那一侧进出，不会横穿节点再绕回来。
      const px = (id2: string) => pos.get(id2)?.x ?? 0;
      const srcRight = px(e.to) >= px(e.from);
      const tgtRight = px(e.from) >= px(e.to);
      const sourceHandle = e.source_iface
        ? `${srcRight ? "sr" : "sl"}:${e.source_iface}`
        : undefined;
      const targetHandle = e.target_iface
        ? `${tgtRight ? "tr" : "tl"}:${e.target_iface}`
        : undefined;
      return {
        id,
        source: e.from,
        target: e.to,
        sourceHandle,
        targetHandle,
        type: "smoothstep",
        ...edgeSpec(e, hotMirror.has(id)),
      };
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
  const sel = useMemo(() => new Set(selected), [selected]);
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

  // 用户拖出来的位置单独记一笔：重算布局时不覆盖它，轮询重建时不丢
  const dragPos = useRef<Map<string, { x: number; y: number }>>(new Map());
  const [pos, setPos] = useState<Map<string, { x: number; y: number }>>(new Map());
  const fitted = useRef(false);

  useEffect(() => {
    // 结构变了：旧的拖动位置作废，等 ELK 给新布局
    dragPos.current.clear();
    let alive = true;
    (async () => {
      try {
        const p = await layoutPositions(nodes, edges);
        if (!alive) return;
        setPos(p);
        if (!fitted.current) {
          fitted.current = true;
          // 等一帧让节点真正落位，再取景
          requestAnimationFrame(() => rf.fitView({ padding: 0.16, duration: 200 }));
        }
      } catch {
        // 布局失败就保持旧位置：图乱一点总比消失好
      }
    })();
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [structKey]);

  const effPos = useMemo(
    () => (dragPos.current.size ? new Map([...pos, ...dragPos.current]) : pos),
    [pos],
  );

  const built = useMemo(
    () => assemble(nodes, edges, effPos, sel, hotMirror, dimmed),
    [nodes, edges, effPos, sel, hotMirror, dimmed],
  );

  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode>([]);
  const [rfEdges, setRfEdges, onEdgesChange] = useEdgesState<Edge>([]);

  useEffect(() => {
    setRfNodes(built.nodes);
    setRfEdges(built.edges);
  }, [built, setRfNodes, setRfEdges]);

  // 拖动结束的位置落一笔，重算 data 时沿用
  const handleNodesChange = (changes: Parameters<typeof onNodesChange>[0]) => {
    for (const c of changes) {
      if (c.type === "position" && c.dragging && c.position) {
        dragPos.current.set(c.id, c.position);
      }
    }
    onNodesChange(changes);
  };

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
        onNodesChange={handleNodesChange}
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
