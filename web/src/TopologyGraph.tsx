/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端的 `/api/topology` 给的已经是"节点 + 边"（`zone:main`、`host:main:web`），
 * 这里不再解析 `uplink: main` 之类的东西，只做两件事：
 *   1. 用 dagre 算一遍层级布局（wan → zone → host，mirror 边单独拉一条虚线）
 *   2. 把 kind / role / retired / status 映射成颜色与线型
 *
 * 布局是**一次性**算的：节点拖动只改位置，不改结构，不需要重排。
 */
import { useEffect, useMemo } from "react";
import {
  Background,
  BackgroundVariant,
  Controls,
  Handle,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  useEdgesState,
  useNodesState,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import dagre from "dagre";
import type { TopoEdge, TopoNode } from "./api";

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致，否则连线会悬空。 */
const DIM: Record<TopoNode["kind"], { w: number; h: number }> = {
  wan: { w: 168, h: 52 },
  zone: { w: 184, h: 66 },
  host: { w: 208, h: 62 },
  mirr: { w: 132, h: 48 },
};

const ROLE_LABEL: Record<string, string> = {
  gateway: "网关",
  analysis: "分析机",
  server: "靶机·服务",
  client: "靶机·终端",
};

type Data = {
  kind: TopoNode["kind"];
  label: string;
  role: string | null;
  ipv4: string | null;
  status: string | null;
  retired: boolean;
  /** CT 号已经拼好（"CT 101"），节点里直接显示，不用再判空 */
  vmidLabel: string;
  meta: Record<string, unknown>;
};

type GNode = Node<Data, "lab">;

/** 单个节点的卡片。用自定义节点而不是默认样式，因为要区分的维度太多。 */
function LabNode({ data }: NodeProps<GNode>) {
  const dim = DIM[data.kind];
  const cls = [
    "tn",
    `tn-${data.kind}`,
    data.kind === "host" && data.role ? `tn-role-${data.role}` : "",
    data.retired ? "tn-retired" : "",
    data.kind === "host" && data.status !== "running" && !data.retired ? "tn-idle" : "",
  ]
    .filter(Boolean)
    .join(" ");

  const meta = data.meta as Record<string, unknown>;
  const m = (k: string) => (typeof meta[k] === "string" ? (meta[k] as string) : null);
  const flag = (k: string) => meta[k] === true;

  return (
    <>
      <Handle type="target" position={Position.Left} style={{ opacity: 0 }} />
      <div className={cls} style={{ width: dim.w, height: dim.h }}>
        {data.kind === "wan" && (
          <>
            <div className="tn-title">WAN</div>
            <div className="tn-sub mono">{m("bridge")}</div>
          </>
        )}

        {data.kind === "zone" && (
          <>
            <div className="tn-row">
              <span className="tn-title">{data.label}</span>
              <span className="tn-chips">
                {flag("nat") && <span className="tn-chip chip-nat">NAT</span>}
                {flag("egress") && <span className="tn-chip chip-egress">出网</span>}
                {(meta.pubs as number) > 0 && (
                  <span className="tn-chip chip-pub">映射 {String(meta.pubs)}</span>
                )}
                {Array.isArray(meta.vpn) && meta.vpn.length > 0 && (
                  <span className="tn-chip chip-vpn">VPN</span>
                )}
              </span>
            </div>
            <div className="tn-sub mono">{m("cidr")}</div>
            <div className="tn-sub mono tn-faint">{m("bridge")}</div>
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
              {data.retired ? "已废弃" : m("provisioner") ?? ""}
            </div>
          </>
        )}

        {data.kind === "mirr" && (
          <>
            <div className="tn-title mono">mirr</div>
            <div className="tn-sub tn-faint">二层镜像桥</div>
          </>
        )}
      </div>
      <Handle type="source" position={Position.Right} style={{ opacity: 0 }} />
    </>
  );
}

const nodeTypes = { lab: LabNode };

/** 边的画法：uplink 实线蓝、member 细灰、mirror 虚线琥珀、mirror-port 短虚线。 */
function edgeSpec(e: TopoEdge): Partial<Edge> {
  const label = e.label ?? undefined;
  switch (e.kind) {
    case "uplink":
      return {
        style: { stroke: "#2f6feb", strokeWidth: 1.8 },
        label,
        labelStyle: { fontSize: 11, fill: "#2f6feb" },
        labelBgStyle: { fill: "#ffffff", fillOpacity: 0.85 },
        markerEnd: { type: MarkerType.ArrowClosed, color: "#2f6feb", width: 16, height: 16 },
      };
    case "member":
      return {
        style: { stroke: "#c8d2e0", strokeWidth: 1.2 },
        markerEnd: { type: MarkerType.Arrow, color: "#c8d2e0", width: 12, height: 12 },
      };
    case "mirror-port":
      return { style: { stroke: "#d97706", strokeWidth: 1.2, strokeDasharray: "3 3" } };
    case "mirror":
    default:
      return {
        style: { stroke: "#d97706", strokeWidth: 2, strokeDasharray: "6 4" },
        label,
        labelStyle: { fontSize: 11, fill: "#b45309" },
        labelBgStyle: { fill: "#fffbeb", fillOpacity: 0.9 },
        markerEnd: { type: MarkerType.ArrowClosed, color: "#d97706", width: 16, height: 16 },
        animated: false,
      };
  }
}

function build(nodes: TopoNode[], edges: TopoEdge[]): { nodes: GNode[]; edges: Edge[] } {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "LR", nodesep: 26, ranksep: 104, marginx: 24, marginy: 24 });
  g.setDefaultEdgeLabel(() => ({}));

  // 跨规格同名的 zone 会撞 id（后端一套 key 空间），这里按先出现的留下
  const uniq = new Map<string, TopoNode>();
  for (const n of nodes) if (!uniq.has(n.id)) uniq.set(n.id, n);

  for (const n of uniq.values()) {
    const d = DIM[n.kind] ?? DIM.host;
    g.setNode(n.id, { width: d.w, height: d.h });
  }
  for (const e of edges) {
    if (e.from === e.to || !uniq.has(e.from) || !uniq.has(e.to)) continue;
    g.setEdge(e.from, e.to);
  }
  dagre.layout(g);

  const rfNodes: GNode[] = [];
  for (const n of uniq.values()) {
    const d = DIM[n.kind] ?? DIM.host;
    const p = g.node(n.id) as { x: number; y: number } | undefined;
    rfNodes.push({
      id: n.id,
      type: "lab",
      position: { x: (p?.x ?? 0) - d.w / 2, y: (p?.y ?? 0) - d.h / 2 },
      data: {
        kind: n.kind,
        label: n.label,
        role: n.role,
        ipv4: n.ipv4,
        status: n.status,
        retired: n.retired,
        vmidLabel: n.vmid ? `CT ${n.vmid}` : "",
        meta: n.meta,
      },
      // 尺寸交给自定义节点自己定，这里给 dagre 用的值即可
      width: d.w,
      height: d.h,
    });
  }

  const rfEdges: Edge[] = edges
    .filter((e) => uniq.has(e.from) && uniq.has(e.to) && e.from !== e.to)
    .map((e, i) => ({
      id: `${e.kind}:${e.from}->${e.to}:${i}`,
      source: e.from,
      target: e.to,
      type: "smoothstep",
      ...edgeSpec(e),
    }));

  return { nodes: rfNodes, edges: rfEdges };
}

export default function TopologyGraph({
  nodes,
  edges,
  onPick,
}: {
  nodes: TopoNode[];
  edges: TopoEdge[];
  onPick?: (n: TopoNode | null) => void;
}) {
  const built = useMemo(() => build(nodes, edges), [nodes, edges]);
  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode>(built.nodes);
  const [rfEdges, setRfEdges, onEdgesChange] = useEdgesState<Edge>(built.edges);

  // 数据变了就整体换掉（id 稳定，用户拖过的位置会丢——可以接受，
  // 反正结构变了的图本来就该重排）
  useEffect(() => {
    setRfNodes(built.nodes);
    setRfEdges(built.edges);
  }, [built, setRfNodes, setRfEdges]);

  return (
    <div className="topo-canvas">
      <ReactFlow
        nodes={rfNodes}
        edges={rfEdges}
        nodeTypes={nodeTypes}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        fitView
        fitViewOptions={{ padding: 0.18 }}
        minZoom={0.2}
        maxZoom={1.6}
        nodesConnectable={false}
        edgesFocusable={false}
        onNodeClick={(_, n) => {
          const orig = nodes.find((x) => x.id === n.id) ?? null;
          onPick?.(orig);
        }}
        onPaneClick={() => onPick?.(null)}
        proOptions={{ hideAttribution: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={18} size={1} color="#dbe1ea" />
        <Controls showInteractive={false} />
        <MiniMap
          pannable
          zoomable
          nodeColor={(n) => {
            const k = (n.data as Data)?.kind;
            return k === "wan"
              ? "#a5b4fc"
              : k === "zone"
                ? "#93b8f7"
                : k === "mirr"
                  ? "#fbbf24"
                  : "#e2e8f0";
          }}
          maskColor="rgba(245,246,248,0.72)"
          style={{ background: "#ffffff", border: "1px solid var(--border)" }}
        />
      </ReactFlow>
    </div>
  );
}
