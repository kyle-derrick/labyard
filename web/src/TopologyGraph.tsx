/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给的是两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。这里做三件事：
 *   1. 用 dagre 算层级（外网 → 网关 → 内网 → 主机），只算一次
 *   2. 把 kind / role / 状态映射成视觉
 *   3. 接住点击、多选、右键，交给页面去开抽屉 / 弹菜单
 *
 * **布局只在结构变化时重算**：轮询每 15 秒来一次数据，如果每次都重排，
 * 用户刚拖好的位置会被抹掉、图会跳一下——这也是"用起来卡"的观感来源之一。
 * 所以这里按"节点 id + 边集合"算一个结构签名，签名没变就只更新 data。
 */
import { useEffect, useMemo, useRef } from "react";
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
  network: { w: 190, h: 70 },
  host: { w: 196, h: 62 },
};

export const ROLE_LABEL: Record<string, string> = {
  gateway: "网关",
  analysis: "分析机",
  server: "靶机",
  client: "靶机",
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
  /** 被选中 / 被"分析机高亮"淡化 */
  selected: boolean;
  dimmed: boolean;
};

type GNode = Node<Data, "lab">;

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
                  <span className="tn-chip chip-vpn">VPN</span>
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

function build(
  nodes: TopoNode[],
  edges: TopoEdge[],
  selected: Set<string>,
  hotMirror: Set<string>,
  dimmed: Set<string>,
): { nodes: GNode[]; edges: Edge[] } {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "LR", nodesep: 30, ranksep: 112, marginx: 24, marginy: 24 });
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
        meta: n.meta as Record<string, unknown>,
        selected: selected.has(n.id),
        dimmed: dimmed.has(n.id),
      },
      width: d.w,
      height: d.h,
    });
  }

  const rfEdges: Edge[] = edges
    .filter((e) => uniq.has(e.from) && uniq.has(e.to) && e.from !== e.to)
    .map((e, i) => {
      const id = `${e.kind}:${e.from}->${e.to}:${i}`;
      return {
        id,
        source: e.from,
        target: e.to,
        type: "smoothstep",
        ...edgeSpec(e, hotMirror.has(id)),
      };
    });

  return { nodes: rfNodes, edges: rfEdges };
}

export default function TopologyGraph({
  nodes,
  edges,
  selected = [],
  focus,
  onPick,
  onSelect,
  onContextMenu,
}: {
  nodes: TopoNode[];
  edges: TopoEdge[];
  selected?: string[];
  /** 当前"正在看"的分析机 id：它的镜像虚线高亮，其余淡化 */
  focus?: string | null;
  onPick?: (n: TopoNode | null) => void;
  onSelect?: (ids: string[]) => void;
  onContextMenu?: (e: React.MouseEvent, n: TopoNode | null) => void;
}) {
  const sel = useMemo(() => new Set(selected), [selected]);

  // focus（分析机）→ 它接的镜像边 + 相关网络，其余淡化
  const hotMirror = useMemo(() => {
    const s = new Set<string>();
    if (!focus) return s;
    edges.forEach((e, i) => {
      if (e.kind === "mirror" && e.to === focus) s.add(`${e.kind}:${e.from}->${e.to}:${i}`);
    });
    return s;
  }, [edges, focus]);

  const dimmed = useMemo(() => {
    if (!focus) return new Set<string>();
    const keep = new Set<string>([focus]);
    edges
      .filter((e) => e.kind === "mirror" && e.to === focus)
      .forEach((e) => keep.add(e.from));
    const out = new Set<string>();
    nodes.forEach((n) => {
      if (!keep.has(n.id)) out.add(n.id);
    });
    return out;
  }, [edges, nodes, focus]);

  const built = useMemo(
    () => build(nodes, edges, sel, hotMirror, dimmed),
    [nodes, edges, sel, hotMirror, dimmed],
  );

  // 结构签名：id 与边集合不变时，只换 data，保留用户拖出来的位置
  const structKey = useMemo(() => {
    const nk = nodes.map((n) => n.id).sort().join(",");
    const ek = edges.map((e) => `${e.kind}:${e.from}>${e.to}`).sort().join(",");
    return `${nk}#${ek}`;
  }, [nodes, edges]);

  const [rfNodes, setRfNodes, onNodesChange] = useNodesState<GNode>(built.nodes);
  const [rfEdges, setRfEdges, onEdgesChange] = useEdgesState<Edge>(built.edges);
  const lastKey = useRef("");

  useEffect(() => {
    if (lastKey.current === structKey) {
      // 同结构：只更新节点 data 与边样式，位置沿用当前值
      setRfNodes((prev) =>
        prev.map((p) => {
          const b = built.nodes.find((x) => x.id === p.id);
          return b ? { ...p, data: b.data } : p;
        }),
      );
      setRfEdges(built.edges);
      return;
    }
    lastKey.current = structKey;
    setRfNodes(built.nodes);
    setRfEdges(built.edges);
  }, [built, structKey, setRfNodes, setRfEdges]);

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
        fitView
        fitViewOptions={{ padding: 0.16 }}
        minZoom={0.15}
        maxZoom={1.6}
        nodesConnectable={false}
        edgesFocusable={false}
        onNodeClick={(_, n) => pick(n.id)}
        onPaneClick={() => pick(undefined)}
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
            if (d?.role === "analysis") return "#bfdbfe";
            return "#e2e8f0";
          }}
          maskColor="rgba(245,246,248,0.72)"
          style={{ background: "#ffffff", border: "1px solid var(--border)" }}
        />
      </ReactFlow>
    </div>
  );
}
