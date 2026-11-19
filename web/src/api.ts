/** 后端 API 的最小封装。类型手写，与 src/api/mod.rs 对应。 */

export type StepState = {
  key: string;
  phase: string;
  state: string;
  message: string;
};

export type DoctorItem = {
  name: string;
  ok: boolean;
  detail: string;
};

export type PreviewStep = {
  key: string;
  phase: string;
  input_hash: string;
  zone: string;
  /** create=会新建 update=会更新 skip=已完成且参数未变，跳过 */
  action?: "create" | "update" | "skip";
  reason?: string;
};

/** 资源目录（/api/catalog）：可视化编排下拉的数据源。 */
export type Catalog = {
  template_storages: { storage: string; kind: string }[];
  disk_storages: { storage: string; kind: string; avail: number | null; total: number | null }[];
  templates: { storage: string; volid: string; label: string }[];
};

export type PreviewResp = {
  zones: unknown[];
  steps: PreviewStep[];
};

export type RunEvent =
  | { kind: "started"; key: string; command: string; pid: number }
  | { kind: "line"; key: string; stream: "stdout" | "stderr"; line: string }
  | { kind: "skipped"; key: string; lines: number }
  | { kind: "upstream_skipped"; key: string; reason: string }
  | { kind: "finished"; key: string; code: number | null; duration_ms: number }
  | { kind: "failed"; key: string; error: string };

class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init?.headers ?? {}),
    },
    credentials: "same-origin",
  });
  if (!res.ok) {
    let msg = res.statusText;
    try {
      const j = await res.json();
      msg = j.error ?? msg;
    } catch {
      /* 非 JSON 响应，保留状态文本 */
    }
    throw new ApiError(res.status, msg);
  }
  return (await res.json()) as T;
}

export const api = {
  health: () => req<{ ok: boolean; version: string }>("/api/health"),

  me: () => req<{ user: string }>("/api/auth/me"),

  login: (password: string) =>
    req<{ user: string }>("/api/auth/login", {
      method: "POST",
      body: JSON.stringify({ password }),
    }),

  logout: () => req<{ ok: boolean }>("/api/auth/logout", { method: "POST" }),
  changePassword: (old_password: string, new_password: string) =>
    req<{ ok: boolean }>("/api/auth/password", {
      method: "POST",
      body: JSON.stringify({ old_password, new_password }),
    }),

  state: () =>
    req<{ steps: StepState[]; retired?: [string, string, string, string][] }>(
      "/api/state",
    ),

  /** 读取服务端已保存的规格；从未保存过时 exists=false、yaml=null。 */
  getSpec: () =>
    req<{ yaml: string | null; exists: boolean }>("/api/spec"),

  /** 保存规格。服务端会先 parse + resolve 校验，非法直接 400。 */
  putSpec: (yaml: string) =>
    req<{ ok: boolean }>("/api/spec", {
      method: "PUT",
      body: JSON.stringify({ yaml }),
    }),

  preview: (yaml: string, snapshot = false) =>
    req<PreviewResp>("/api/preview", {
      method: "POST",
      body: JSON.stringify({ yaml, snapshot }),
    }),

  /** 可用模板与存储：编排页的下拉数据源。 */
  catalog: () => req<Catalog>("/api/catalog"),

  apply: (yaml: string, snapshot = false) =>
    req<{ run_id: string }>("/api/apply", {
      method: "POST",
      body: JSON.stringify({ yaml, snapshot }),
    }),

  /** 直接执行某份已保存的规格（规格列表页用）。 */
  applySpec: (spec: string, snapshot = false) =>
    req<{ run_id: string }>("/api/apply", {
      method: "POST",
      body: JSON.stringify({ spec, snapshot }),
    }),

  cancel: (runId: string) =>
    req<{ ok: boolean }>(`/api/runs/${runId}/cancel`, { method: "POST" }),

  doctor: () => req<{ items: DoctorItem[] }>("/api/doctor"),

  // ── 规格文件（多份） ──
  specs: () => req<{ specs: SpecMeta[] }>("/api/specs"),

  createSpec: (body: { name: string; yaml?: string; copy_of?: string }) =>
    req<{ ok: boolean; name: string }>("/api/specs", {
      method: "POST",
      body: JSON.stringify(body),
    }),

  specDetail: (name: string) =>
    req<{ yaml: string | null; exists: boolean; name: string }>(
      `/api/specs/${encodeURIComponent(name)}`,
    ),

  putSpecNamed: (name: string, yaml: string) =>
    req<{ ok: boolean }>(`/api/specs/${encodeURIComponent(name)}`, {
      method: "PUT",
      body: JSON.stringify({ yaml }),
    }),

  deleteSpec: (name: string) =>
    req<{ ok: boolean }>(`/api/specs/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),

  addZone: (spec: string, zone: string, yaml: string) =>
    req<{ ok: boolean }>(`/api/specs/${encodeURIComponent(spec)}/zones`, {
      method: "POST",
      body: JSON.stringify({ zone, yaml }),
    }),

  /** 取某个 zone 的 YAML 片段，作为编辑框的起点。 */
  zoneDetail: (spec: string, zone: string) =>
    req<{ zone: string; yaml: string | null; exists: boolean }>(
      `/api/specs/${encodeURIComponent(spec)}/zones/${encodeURIComponent(zone)}`,
    ),

  updateZone: (spec: string, zone: string, yaml: string) =>
    req<{ ok: boolean }>(
      `/api/specs/${encodeURIComponent(spec)}/zones/${encodeURIComponent(zone)}`,
      { method: "PUT", body: JSON.stringify({ yaml }) },
    ),

  deleteZone: (spec: string, zone: string) =>
    req<{ ok: boolean }>(
      `/api/specs/${encodeURIComponent(spec)}/zones/${encodeURIComponent(zone)}`,
      { method: "DELETE" },
    ),

  /** 结构化视图：可视化编排读它，保存也走它（后端负责 JSON ↔ YAML）。 */
  specModel: (name: string) =>
    req<{ exists: boolean; model: unknown; yaml: string | null; name: string }>(
      `/api/specs/${encodeURIComponent(name)}/model`,
    ),

  putSpecModel: (name: string, model: unknown) =>
    req<{ ok: boolean; yaml: string }>(
      `/api/specs/${encodeURIComponent(name)}/model`,
      { method: "PUT", body: JSON.stringify({ model }) },
    ),

  // ── 拓扑 ──
  topology: (spec?: string) =>
    req<{ nodes: TopoNode[]; edges: TopoEdge[]; specs: string[] }>(
      `/api/topology${spec ? `?spec=${encodeURIComponent(spec)}` : ""}`,
    ),

  // ── 网关 ──
  gateway: (zone: string) =>
    req<GatewayStatus>(`/api/zones/${encodeURIComponent(zone)}/gateway/status`),

  setEgress: (zone: string, on: boolean) =>
    req<{ ok: boolean; run_id: string }>(
      `/api/zones/${encodeURIComponent(zone)}/gateway/egress`,
      { method: "POST", body: JSON.stringify({ on }) },
    ),

  addPub: (zone: string, body: { proto: string; port: number; to: string }) =>
    req<{ ok: boolean; run_id: string }>(
      `/api/zones/${encodeURIComponent(zone)}/gateway/pub`,
      { method: "POST", body: JSON.stringify(body) },
    ),

  delPub: (zone: string, idx: number) =>
    req<{ ok: boolean; run_id: string }>(
      `/api/zones/${encodeURIComponent(zone)}/gateway/pub/${idx}`,
      { method: "DELETE" },
    ),

  setVpn: (zone: string, proto: "wireguard" | "openvpn", action: "on" | "off") =>
    req<{ ok: boolean; run_id: string }>(
      `/api/zones/${encodeURIComponent(zone)}/gateway/vpn`,
      { method: "POST", body: JSON.stringify({ proto, action }) },
    ),

  /** 整网流量监控开关（拓扑页右键用）。 */
  setMonitor: (zone: string, on: boolean, opts?: { name?: string; mgmt?: string; use?: string }) =>
    req<{ ok: boolean; run_id: string | null }>(
      `/api/zones/${encodeURIComponent(zone)}/monitor`,
      { method: "POST", body: JSON.stringify({ on, ...opts }) },
    ),

  // ── 镜像连线 ──
  mirrors: (spec?: string) =>
    req<{ mirrors: MirrorRow[]; reconcile?: ReconcileInfo; units?: Record<string, string> }>(
      `/api/mirrors${spec ? `?spec=${encodeURIComponent(spec)}` : ""}`,
    ),

  addMirror: (from: string, to: string, spec?: string) =>
    req<{ ok: boolean; spec: string; run_id: string | null }>("/api/mirrors", {
      method: "POST",
      body: JSON.stringify({ from, to, spec }),
    }),

  delMirror: (id: string) =>
    req<{ ok: boolean }>(`/api/mirrors/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }),

  // ── retire / revive ──
  retireZone: (zone: string) =>
    req<RetireResp>(`/api/zones/${encodeURIComponent(zone)}/retire`, {
      method: "POST",
    }),

  reviveZone: (zone: string) =>
    req<{ ok: boolean; started: string[]; run_id: string | null }>(
      `/api/zones/${encodeURIComponent(zone)}/revive`,
      { method: "POST" },
    ),

  retireHost: (zone: string, host: string) =>
    req<RetireResp>(
      `/api/hosts/${encodeURIComponent(zone)}/${encodeURIComponent(host)}/retire`,
      { method: "POST" },
    ),

  reviveHost: (zone: string, host: string) =>
    req<{ ok: boolean; started: string[]; run_id: string | null }>(
      `/api/hosts/${encodeURIComponent(zone)}/${encodeURIComponent(host)}/revive`,
      { method: "POST" },
    ),

  // ── 运维 ──
  runs: () => req<{ runs: RunRow[] }>("/api/runs"),

  runSteps: (id: string) =>
    req<{ steps: RunStepRow[] }>(`/api/runs/${id}/steps`),

  retry: (id: string) =>
    req<{ run_id: string }>(`/api/runs/${id}/retry`, { method: "POST" }),

  hosts: () => req<{ hosts: HostRow[] }>("/api/hosts"),

  power: (zone: string, host: string, action: "start" | "stop" | "reboot") =>
    req<{ ok: boolean; vmid: number }>(
      `/api/hosts/${zone}/${host}/power`,
      { method: "POST", body: JSON.stringify({ action }) },
    ),

  snapshots: (zone: string, host: string) =>
    req<{ snapshots: string[]; vmid: number }>(
      `/api/snapshots/${zone}/${host}`,
    ),

  snapshotOp: (
    zone: string,
    host: string,
    action: "create" | "rollback" | "delete",
    name: string,
  ) =>
    req<{ ok: boolean }>(`/api/snapshots/${zone}/${host}`, {
      method: "POST",
      body: JSON.stringify({ action, name }),
    }),

  destroy: (zone: string) =>
    req<{
      ok: boolean;
      destroyed_cts: [string, number][];
      absent_cts: string[];
      bridge_deleted: boolean;
      kept_bridge: string | null;
    }>("/api/destroy", {
      method: "POST",
      body: JSON.stringify({ zone, confirm: zone }),
    }),
};

export type RunRow = {
  id: string;
  status: string;
  spec_name: string | null;
  started_at: string;
  finished_at: string | null;
};

export type RunStepRow = {
  step_key: string;
  state: string;
  message: string;
  started_at: string;
  finished_at: string;
};

export type HostRow = {
  zone: string;
  host: string;
  vmid: number;
  exists: boolean;
  status: string;
  name: string | null;
  role: string | null;
  ipv4: string | null;
  spec: string | null;
  retired: boolean;
};

export type SpecMeta = {
  name: string;
  zones: number;
  hosts: number;
  bytes: number;
  modified: string | null;
  valid: boolean;
  error: string | null;
};

export type GatewayStatus = {
  zone: string;
  spec: string;
  gateway: { host: string; vmid: number | null; ipv4: string };
  v4_cidr: string;
  nat: boolean;
  egress: boolean;
  pubs: { proto: string; port: number; to: string; spec: string }[];
  vpn: {
    wireguard: boolean;
    openvpn: boolean;
    clients: string[];
    wireguard_active: boolean;
    openvpn_active: boolean;
  };
  retired_hosts: string[];
};

export type RetireResp = {
  ok: boolean;
  spec: string;
  stopped: string[];
  absent: string[];
  mirror_units_stopped: string[];
  note: string;
};

/** 主机上的一块网口（拓扑节点底部的 chip）。 */
export type TopoIface = {
  /** 容器内网卡名（eth0、eth1……） */
  name: string;
  /** 接在哪张网络上：zone 名或 "wan" */
  net: string;
  /** 口上的地址；抓包口没有 */
  ip: string | null;
  /** 是不是监控机的抓包口（接 mir{zone} 桥） */
  capture: boolean;
};

export type TopoNode = {
  id: string;
  /** 只有两类：网络（外网 / 各 zone 的内网）与主机 */
  kind: "network" | "host";
  label: string;
  zone: string | null;
  role: string | null;
  vmid: number | null;
  ipv4: string | null;
  status: string | null;
  retired: boolean;
  meta: Record<string, unknown>;
  /** 主机的网口清单；网络节点为空 */
  ifaces?: TopoIface[];
};

export type TopoEdge = {
  from: string;
  to: string;
  /** attach：谁插在谁上面（方向即数据流）；mirror：主机网口 → 监控机的单向虚线 */
  kind: "attach" | "mirror";
  label: string | null;
  /** attach 边的接口角色：uplink（接上层网络）| inner（接本 zone 内网）| mgmt（挂外网桥） */
  port: "uplink" | "inner" | "mgmt" | null;
  /** 发送端的网口名（镜像边 = 源主机的采集口；attach 边 = 网关的内网口）。
   * 前端据此把边锚到节点上的具体网口 chip。 */
  source_iface?: string;
  /** 接收端的网口名（镜像边 = 监控机的抓包口；attach 边 = 主机接入网络的口） */
  target_iface?: string;
  /** mirror 边落到的两层抓包桥 */
  capture_bridge?: string;
};

export type MirrorRow = {
  spec: string;
  from: string;
  to: string;
  src_bridge: string;
  capture_bridge?: string;
  source_iface?: string;
  target_iface?: string;
  /** labmirror@ 实例名（vmid 还没定的时候是 null） */
  instance: string | null;
  /** 宿主上的现实状态：active / inactive / failed / missing（未安装） */
  active: string;
};

/** 后台镜像巡检的状态（/api/mirrors 带出）。 */
export type ReconcileInfo = {
  at: string | null;
  applied: number;
  removed: number;
  skipped: string | null;
  error: string | null;
};

/** VPN 客户端配置的下载地址（后端现读容器里的私钥现拼，平台不存副本）。 */
export function vpnClientUrl(
  zone: string,
  proto: "wireguard" | "openvpn",
  client?: string,
): string {
  const q = client ? `?client=${encodeURIComponent(client)}` : "";
  return `/api/zones/${encodeURIComponent(zone)}/gateway/vpn/${proto}/client${q}`;
}

/** Web 终端的 iframe 地址（ttyd 的 base path 与后端反代一致）。 */
export function terminalUrl(zone: string, host: string): string {
  return `/terminal/${encodeURIComponent(zone)}/${encodeURIComponent(host)}/`;
}

/**
 * 订阅一次 run 的事件流。
 * 用 fetch + ReadableStream 而不是 EventSource，因为 EventSource 不能带自定义头，
 * 且无法在 401 时优雅处理（会无限重连）。
 */
export function streamRun(
  runId: string,
  onEvent: (ev: RunEvent) => void,
  onDone: () => void,
): () => void {
  const ctrl = new AbortController();
  (async () => {
    try {
      const res = await fetch(`/api/runs/${runId}/events`, {
        signal: ctrl.signal,
        credentials: "same-origin",
      });
      if (!res.body) {
        onDone();
        return;
      }
      const reader = res.body.getReader();
      const dec = new TextDecoder();
      let buf = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        // SSE 以空行分隔事件
        let idx: number;
        while ((idx = buf.indexOf("\n\n")) >= 0) {
          const chunk = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          for (const line of chunk.split("\n")) {
            if (line.startsWith("data:")) {
              const payload = line.slice(5).trim();
              if (!payload) continue;
              try {
                const ev = JSON.parse(payload) as RunEvent;
                onEvent(ev);
                if (ev.kind === "finished" && ev.key.startsWith("run:")) {
                  onDone();
                  return;
                }
              } catch {
                /* 忽略无法解析的行（例如心跳注释） */
              }
            }
          }
        }
      }
      onDone();
    } catch (e) {
      if ((e as Error).name !== "AbortError") {
        console.error("事件流中断", e);
      }
      onDone();
    }
  })();
  return () => ctrl.abort();
}
