/** 全局拓扑图：react-flow 只负责画，语义全在后端。
 *
 * 后端给的是两类节点（`net:*` / `host:*`）和两类边（`attach` / `mirror`），
 * 方向都是数据流方向。这里做四件事：
 *   1. 用 dagre 算层级（外网 → 网关 → 内网 → 主机），**只用 attach 边**——
 *      镜像线从主机指向监控机，参与布局的话监控机会被推到所有主机后面
 *      一档，看起来像"挂在别的机器下面"。镜像线只负责画，不负责布局。
 *   2. 主机节点底部画出**每个网口**（chip），所有边都锚到具体网口上：
 *      attach 边接主机的 eth0/eth1，镜像线从源主机的内网口出发、接到
 *      监控机上对应的抓包口。"线从哪块口出发"和"图上哪块口"是同一个东西。
 *   3. 把 kind / role / 状态映射成视觉
 *   4. 接住点击、多选、右键，交给页面去开抽屉 / 弹菜单
 *
 * **布局只在结构变化时重算**：轮询每 15 秒来一次数据，如果每次都重排，
 * 用户刚拖好的位置会被抹掉、图会跳一下——这也是"用起来卡"的观感来源之一。
 * 所以这里按"节点 id + 边集合"算一个结构签名，签名没变就只更新 data。
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
  useEdgesState,
  useNodesState,
  type Edge,
  type Node,
  type NodeProps,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import dagre from "dagre";
import type { TopoEdge, TopoIface, TopoNode } from "./api";

/** 每种节点的画布尺寸。CSS 里的宽高必须和这里一致，否则连线会悬空。
 * host 高度含底部的网口 chip 行（单行 22px，两行也放得下）。 */
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

/** 网口 chip。两枚不可见的 handle 叠在 chip 底部中点：
 * `s:{name}` 供出边（镜像线、网关内网口）用，`t:{name}` 供入边用。 */
function IfaceChip({ f }: { f: TopoIface }) {
  const tip = f.capture
    ? `${f.name} · 抓包（${f.net} 的流量）`
    : `${f.name} · ${f.net}${f.ip ? ` ${f.ip.replace(/\/\d+$/, "")}` : ""}`;
  return (
    <div
      className={`iface${f.capture ? " iface-cap" : ""}`}
      title={tip}
    >
      <Handle
        type="source"
        id={`s:${f.name}`}
        position={Position.Bottom}
        style={{ left: "50%", bottom: -4, width: 6, height: 6, opacity: 0 }}
      />
      <Handle
        type="target"
        id={`t:${f.name}`}
        position={Position.Bottom}
        style={{ left: "50%", bottom: -4, width: 6, height: 6, opacity: 0 }}
      />
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
      {/* 网络节点的默认接入点：左边收、右边发 */}
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
  // 布局只吃 attach 边：镜像线把主机连向监控机，参与布局会把监控机推到
  // 所有主机后面一档——图上看就成了"监控机挂在别的机器后面"。镜像线在
  // 一个网络里的语义是"平行存在于每台主机与监控机之间"，不该影响层级。
  for (const e of edges) {
    if (e.kind !== "attach") continue;
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
        ifaces: n.ifaces ?? [],
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
        // 端点锚到具体网口：主机侧有网口名的边都走 chip 上的 handle
        sourceHandle: e.source_iface ? `s:${e.source_iface}` : undefined,
        targetHandle: e.target_iface ? `t:${e.target_iface}` : undefined,
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
