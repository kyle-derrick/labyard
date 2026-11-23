/** 主机运维：电源、快照、Web 终端、网关策略、retire/revive、批量操作。
 *
 * 主机身份的唯一权威是后端的 vmids 分配表（`/api/hosts`），不是 PVE 的
 * 全量 VM 列表——列表里可能有别的实验留下的容器，混进来只会误导。
 *
 * retire 是"停机保号"：vmid、IP、磁盘、快照全留着，只是容器停掉、镜像实例
 * 摘掉、拓扑里折叠。它和 destroy 是两件不同的事，这里分开放，别让人点错。
 *
 * 批量操作走逐台调用：后端刻意没有批量端点——每台电源操作就是一个编排
 * run（有全局串行闸），一次提交几十台前端逐台排队打请求，进度可见、
 * 单台失败不影响别的。批量动作前先过滤掉不可操作的主机（不存在的、
 * 已废弃的），剩下的才进 confirm。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Dropdown,
  Empty,
  Form,
  Input,
  Modal,
  Popconfirm,
  Select,
  Space,
  Statistic,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import type { Key } from "react";
import {
  CloudServerOutlined,
  CodeOutlined,
  CameraOutlined,
  MoreOutlined,
  PauseCircleOutlined,
  PoweroffOutlined,
  ReloadOutlined,
  SafetyOutlined,
  SearchOutlined,
  SettingOutlined,
  SyncOutlined,
  UndoOutlined,
} from "@ant-design/icons";
import { api, terminalUrl, type HostRow, type UnmanagedVm } from "./api";
import { HostConfigEditor, type ConfigTarget } from "./HostConfig";
import GatewayPanel from "./Gateway";

const { Text } = Typography;

const ROLE_LABEL: Record<string, string> = {
  gateway: "网关",
  monitor: "流量监控",
  host: "主机",
  // 老数据兜底（后端已把 server/client 归并为 host）
  server: "主机",
  client: "主机",
};

const ROLE_COLOR: Record<string, string> = {
  gateway: "blue",
  monitor: "cyan",
  host: "green",
};

function statusTag(status: string, retired: boolean) {
  if (retired) return <Tag color="orange">已废弃</Tag>;
  switch (status) {
    case "running":
      return <Tag color="green">运行中</Tag>;
    case "stopped":
      return <Tag color="default">已停止</Tag>;
    case "absent":
      return <Tag color="red">不存在</Tag>;
    default:
      return <Tag>{status}</Tag>;
  }
}

/** 终端弹窗：iframe 嵌 ttyd 页面（自带 xterm.js、CJK 输入与 ZMODEM 传文件）。
 *  cookie 认证，同源反代，不需要额外令牌。 */
function TerminalModal({
  target,
  onClose,
}: {
  target: { zone: string; host: string } | null;
  onClose: () => void;
}) {
  return (
    <Modal
      open={!!target}
      onCancel={onClose}
      footer={null}
      width="92vw"
      style={{ top: 24, maxWidth: 1400 }}
      destroyOnClose
      title={
        target ? (
          <Space>
            <CodeOutlined />
            <span>
              {target.zone} / {target.host}
            </span>
            <Text type="secondary" style={{ fontSize: 12 }}>
              终端内可直接拖文件上传（ZMODEM），或用 rz / sz 收传
            </Text>
          </Space>
        ) : (
          ""
        )
      }
    >
      {target ? (
        <iframe
          src={terminalUrl(target.zone, target.host)}
          title="terminal"
          style={{
            width: "100%",
            height: "78vh",
            border: "none",
            borderRadius: 6,
            background: "#111",
          }}
        />
      ) : null}
    </Modal>
  );
}

/** 快照管理：列、建、回滚、删。回滚会重启容器（恢复到快照点）。 */
function SnapshotModal({
  target,
  onClose,
}: {
  target: { zone: string; host: string } | null;
  onClose: () => void;
}) {
  const { message } = AntApp.useApp();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["snapshots", target?.zone, target?.host],
    queryFn: () => api.snapshots(target!.zone, target!.host),
    enabled: !!target,
  });

  const op = async (action: "create" | "rollback" | "delete", n: string) => {
    if (!target) return;
    setBusy(true);
    try {
      await api.snapshotOp(target.zone, target.host, action, n);
      message.success(
        action === "create" ? "快照已创建" : action === "rollback" ? "已回滚并重启" : "快照已删除",
      );
      setName("");
      await q.refetch();
      qc.invalidateQueries({ queryKey: ["hosts"] });
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={!!target}
      onCancel={onClose}
      footer={null}
      width={560}
      destroyOnClose
      title={
        focusTargetTitle(target) + " 的快照"
      }
    >
      <Space.Compact style={{ width: "100%", marginBottom: 16 }}>
        <Input
          placeholder="快照名（字母数字 -_）"
          value={name}
          onChange={(e) => setName(e.target.value)}
          onPressEnter={() => name && op("create", name)}
        />
        <Button
          type="primary"
          loading={busy}
          disabled={!name}
          onClick={() => op("create", name)}
        >
          创建快照
        </Button>
      </Space.Compact>

      {q.data?.snapshots.length ? (
        <Table
          size="small"
          rowKey={(r) => r}
          dataSource={q.data.snapshots}
          pagination={false}
          columns={[
            { title: "名称", dataIndex: undefined, render: (_: unknown, r: string) => <code>{r}</code> },
            {
              title: "操作",
              width: 180,
              render: (_: unknown, r: string) => (
                <Space size={4}>
                  <Popconfirm
                    title={`回滚到 ${r}？`}
                    description="容器会恢复到该快照点并重启，之后的改动丢失。"
                    okText="回滚"
                    cancelText="取消"
                    onConfirm={() => op("rollback", r)}
                  >
                    <Button size="small" icon={<SyncOutlined />} loading={busy}>
                      回滚
                    </Button>
                  </Popconfirm>
                  <Popconfirm
                    title={`删除快照 ${r}？`}
                    okText="删除"
                    cancelText="取消"
                    onConfirm={() => op("delete", r)}
                  >
                    <Button size="small" danger loading={busy}>
                      删除
                    </Button>
                  </Popconfirm>
                </Space>
              ),
            },
          ]}
        />
      ) : (
        <Empty description="还没有快照" image={Empty.PRESENTED_IMAGE_SIMPLE} />
      )}
    </Modal>
  );
}

function focusTargetTitle(t: { zone: string; host: string } | null): string {
  return t ? `${t.zone} / ${t.host}` : "";
}

export default function Hosts() {
  const { message, modal } = AntApp.useApp();
  const qc = useQueryClient();
  const [term, setTerm] = useState<{ zone: string; host: string } | null>(null);
  const [snap, setSnap] = useState<{ zone: string; host: string } | null>(null);
  const [gw, setGw] = useState<string | null>(null);
  const [cfg, setCfg] = useState<ConfigTarget | null>(null);
  const [destroyZone, setDestroyZone] = useState<string | null>(null);
  const [confirmName, setConfirmName] = useState("");
  const [selected, setSelected] = useState<Key[]>([]);
  const [search, setSearch] = useState("");
  const [snapBatch, setSnapBatch] = useState<HostRow[] | null>(null);
  const [snapName, setSnapName] = useState("");

  const hosts = useQuery({
    queryKey: ["hosts"],
    queryFn: api.hosts,
    refetchInterval: 8000,
  });

  const rows = hosts.data?.hosts ?? [];

  /** 规格里已经没有的主机（vmid 记录还在）。默认不展示：它们是删掉的实验
   *  留下的历史分配，混在当前环境里会让人以为"多了三个网络"。 */
  const liveRows = useMemo(() => rows.filter((r) => !r.orphan), [rows]);
  const orphanRows = useMemo(() => rows.filter((r) => r.orphan), [rows]);
  const orphanCount = orphanRows.length;
  /** 孤儿里容器还真实存在的：这些才是能销毁、也最该处理的 */
  const orphanAlive = useMemo(() => orphanRows.filter((r) => r.exists), [orphanRows]);

  const zones = useMemo(
    () => Array.from(new Set(liveRows.map((r) => r.zone))),
    [liveRows],
  );

  /** 全局搜索：主机名 / IP / CT 号 / zone 任意命中就保留 */
  const filtered = useMemo(() => {
    const s = search.trim().toLowerCase();
    if (!s) return liveRows.filter((r) => r.host !== "*");
    return liveRows.filter(
      (r) =>
        r.host !== "*" &&
        (r.host.toLowerCase().includes(s) ||
          r.zone.toLowerCase().includes(s) ||
          (r.ipv4 ?? "").replace(/\/\d+$/, "").includes(s) ||
          String(r.vmid).includes(s)),
    );
  }, [liveRows, search]);

  /** 选中行里"真的能操作"的：容器存在且未废弃。批量动作前先算好这个集合。 */
  const selectedRows = useMemo(
    () =>
      selected
        .map((k) => liveRows.find((r) => `${r.zone}/${r.host}` === k))
        .filter((r): r is HostRow => !!r && r.exists && !r.retired),
    [liveRows, selected],
  );

  /** 统计卡片：只算现存规格里的机器 */
  const stats = useMemo(() => {
    const real = liveRows.filter((r) => r.host !== "*");
    return {
      total: real.length,
      running: real.filter((r) => r.status === "running" && !r.retired).length,
      stopped: real.filter((r) => r.status === "stopped" && !r.retired).length,
      retired: real.filter((r) => r.retired).length,
    };
  }, [liveRows]);

  // zone 整体废弃的判据：该 zone 下所有主机都 retired。
  // `/api/hosts` 只列有 vmid 的真实主机，`*` 这种"整 zone"标记不会出现在行里，
  // 所以不能直接找 host === "*"。
  const zoneRetired = useCallback(
    (z: string) => {
      const zs = liveRows.filter((r) => r.zone === z);
      return zs.length > 0 && zs.every((r) => r.retired);
    },
    [liveRows],
  );

  // 网络/网关概览：zone 层的网段、出网、VPN、监控机只能在拓扑接口里拿到，
  // 主机列表本身只有每台机器的一行。15 秒缓存，够新也不至于一直打。
  const topo = useQuery({
    queryKey: ["topology"],
    queryFn: () => api.topology(),
    staleTime: 15000,
    refetchInterval: 20000,
  });

  const zoneRows = useMemo(() => {
    type ZoneMeta = { cidr?: string; egress?: boolean; vpn?: string[]; monitor?: string };
    const meta = new Map<string, ZoneMeta>();
    for (const n of topo.data?.nodes ?? []) {
      if (n.kind === "network" && n.zone) meta.set(n.zone, (n.meta ?? {}) as ZoneMeta);
    }
    return zones.map((z) => {
      const list = liveRows.filter((r) => r.zone === z && r.host !== "*");
      return {
        zone: z,
        gw: list.find((r) => r.role === "gateway"),
        meta: meta.get(z) ?? {},
        total: list.length,
        running: list.filter((r) => r.status === "running" && !r.retired).length,
        retired: zoneRetired(z),
      };
    });
  }, [zones, liveRows, topo.data, zoneRetired]);

  /** 清掉"规格已删、容器也不存在"的 vmid 记录。容器还在的那些后端会保留。 */
  const pruneOrphans = useCallback(async () => {
    try {
      const r = await api.pruneHosts();
      if (r.removed === 0) {
        message.info("没有可清理的记录");
      } else {
        message.success(
          r.kept > 0
            ? `已清理 ${r.removed} 条；另有 ${r.kept} 条容器还在，已保留`
            : `已清理 ${r.removed} 条历史记录`,
        );
      }
      await hosts.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  }, [hosts, message]);

  /** 宿主网桥现状：删规格不会删桥，残留的桥界面上原本完全看不见 */
  const bridges = useQuery({
    queryKey: ["bridges"],
    queryFn: api.bridges,
    staleTime: 30_000,
  });
  const orphanBridges = useMemo(
    () => (bridges.data?.bridges ?? []).filter((b) => b.orphan),
    [bridges.data],
  );

  /** PVE 上有、但不在 vmid 分配表里的虚拟机（面板里手工建的等）。 */
  const unmanaged = useMemo(() => hosts.data?.unmanaged ?? [], [hosts.data]);

  // ── 纳管：把面板里手工建的容器写进规格 ──
  const [adopt, setAdopt] = useState<UnmanagedVm | null>(null);
  const [adoptSpec, setAdoptSpec] = useState<string>();
  const [adoptZone, setAdoptZone] = useState<string>();
  const [adoptHost, setAdoptHost] = useState("");
  const [adoptIp, setAdoptIp] = useState("");
  const [adopting, setAdopting] = useState(false);

  const specList = useQuery({
    queryKey: ["specs"],
    queryFn: api.specs,
    enabled: !!adopt,
  });
  const adoptModel = useQuery({
    queryKey: ["spec-model", adoptSpec],
    queryFn: () => api.specModel(adoptSpec!),
    enabled: !!adoptSpec,
  });
  const adoptZones = useMemo(() => {
    const m = adoptModel.data?.model as
      | { zones?: Record<string, unknown> }
      | undefined;
    return Object.keys(m?.zones ?? {});
  }, [adoptModel.data]);

  // 打开时默认选第一份规格（多数环境只有一份）
  useEffect(() => {
    if (adopt && !adoptSpec && specList.data?.specs.length) {
      const first = specList.data.specs.find((s) => s.valid) ?? specList.data.specs[0];
      setAdoptSpec(first.name);
    }
  }, [adopt, adoptSpec, specList.data]);

  /** 容器名当主机名：转小写、非法字符换 `-`、去掉首尾 `-`，截到 31。 */
  const openAdopt = useCallback((r: UnmanagedVm) => {
    const slug = (r.name || `ct${r.vmid}`)
      .toLowerCase()
      .replace(/[^a-z0-9-]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 31);
    setAdopt(r);
    setAdoptHost(slug || `ct${r.vmid}`);
    setAdoptIp("");
    setAdoptZone(undefined);
  }, []);

  const submitAdopt = useCallback(async () => {
    if (!adopt?.vmid || !adoptSpec || !adoptZone || !adoptHost.trim()) {
      message.warning("规格、zone、主机名都要填");
      return;
    }
    setAdopting(true);
    try {
      await api.adoptHost({
        vmid: adopt.vmid,
        spec: adoptSpec,
        zone: adoptZone,
        host: adoptHost.trim(),
        ip: adoptIp.trim() || undefined,
      });
      message.success(
        `已写入规格：${adoptZone}:${adoptHost.trim()} → vmid ${adopt.vmid}。点「应用」后才会按规格配网络`,
      );
      setAdopt(null);
      await hosts.refetch();
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setAdopting(false);
    }
  }, [adopt, adoptSpec, adoptZone, adoptHost, adoptIp, message, hosts]);

  /** 销毁"规格里已经没有、容器还在"的孤儿。没有这个入口就只能上机器手工删。 */
  const purgeOrphans = useCallback(async () => {
    try {
      const r = await api.purgeOrphans();
      if (r.destroyed.length) {
        message.success(`已销毁 ${r.destroyed.length} 台：${r.destroyed.join("、")}`);
      }
      if (r.forgotten.length) {
        message.info(`顺带清掉 ${r.forgotten.length} 条失效记录`);
      }
      if (r.errors.length) {
        message.error(r.errors.join("；"));
      }
      await hosts.refetch();
      await bridges.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  }, [hosts, bridges, message]);

  const pruneBridges = async () => {
    try {
      const r = await api.pruneBridges();
      if (r.removed.length) {
        message.success(`已删除 ${r.removed.length} 个残留网桥：${r.removed.join("、")}`);
      } else {
        message.info("没有可自动清理的网桥");
      }
      if (r.kept.length) {
        message.warning(
          `这些还挂着地址或成员口，需要你确认后手工处理：${r.kept.join("、")}`,
        );
      }
      await bridges.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const doPower = useCallback(
    async (r: HostRow, action: "start" | "stop" | "reboot") => {
      try {
        await api.power(r.zone, r.host, action);
        message.success(
          action === "start" ? "已启动" : action === "stop" ? "已停止" : "已重启",
        );
        await hosts.refetch();
      } catch (e) {
        message.error((e as Error).message);
      }
    },
    [hosts, message],
  );

  /** 批量电源：逐台排队（后端 run 有全局闸，请求会自然串行），逐台报进度 */
  const doBatchPower = useCallback(
    (action: "start" | "stop" | "reboot", list: HostRow[]) => {
      if (list.length === 0) return;
      const label = action === "start" ? "启动" : action === "stop" ? "停止" : "重启";
      modal.confirm({
        title: `批量${label} ${list.length} 台？`,
        content: list.map((r) => `${r.zone}/${r.host}`).join("、"),
        okText: `批量${label}`,
        cancelText: "取消",
        onOk: async () => {
          let ok = 0;
          const fail: string[] = [];
          for (const r of list) {
            try {
              await api.power(r.zone, r.host, action);
              ok += 1;
            } catch (e) {
              fail.push(`${r.zone}/${r.host}: ${(e as Error).message}`);
            }
          }
          message[fail.length ? "warning" : "success"](
            `批量${label}：${ok}/${list.length} 台成功` + (fail.length ? `，失败：${fail.join("；")}` : ""),
          );
          setSelected([]);
          await hosts.refetch();
        },
      });
    },
    [message, modal, hosts],
  );

  /** 批量废弃：和单台一样走确认框，只是列表更长 */
  const doBatchRetire = useCallback(
    (list: HostRow[]) => {
      if (list.length === 0) return;
      modal.confirm({
        title: `批量废弃 ${list.length} 台？`,
        content: (
          <Space direction="vertical" size={4}>
            <Text>{list.map((r) => `${r.zone}/${r.host}`).join("、")}</Text>
            <Text type="secondary">
              容器停机、镜像实例摘掉；vmid/IP/磁盘/快照全保留，可随时恢复。
            </Text>
          </Space>
        ),
        okText: "批量废弃",
        okButtonProps: { danger: true },
        cancelText: "取消",
        onOk: async () => {
          let ok = 0;
          const fail: string[] = [];
          for (const r of list) {
            try {
              await api.retireHost(r.zone, r.host);
              ok += 1;
            } catch (e) {
              fail.push(`${r.zone}/${r.host}: ${(e as Error).message}`);
            }
          }
          message[fail.length ? "warning" : "success"](
            `批量废弃：${ok}/${list.length} 台成功` + (fail.length ? `，失败：${fail.join("；")}` : ""),
          );
          setSelected([]);
          await hosts.refetch();
          qc.invalidateQueries({ queryKey: ["topology"] });
        },
      });
    },
    [message, modal, hosts, qc],
  );

  /** 废弃：停机但保留一切。比 destroy 轻得多，所以不要求手打名字确认。 */
  const doRetire = (zone: string, host: string) => {
    const whole = host === "*";
    modal.confirm({
      title: whole ? `废弃整个 zone ${zone}？` : `废弃 ${zone} / ${host}？`,
      content: (
        <Space direction="vertical" size={4}>
          <Text>容器停机，镜像实例摘掉，拓扑里折叠成灰色虚线。</Text>
          <Text type="secondary">
            vmid、IP、磁盘、快照全部保留——「恢复」时原样回来，不会重新装一遍。
          </Text>
        </Space>
      ),
      okText: "废弃",
      okButtonProps: { danger: true },
      cancelText: "取消",
      onOk: async () => {
        try {
          const r = whole ? await api.retireZone(zone) : await api.retireHost(zone, host);
          message.success(
            `已废弃 ${r.stopped.length} 台` +
              (r.mirror_units_stopped.length
                ? `，停掉 ${r.mirror_units_stopped.length} 个镜像实例`
                : ""),
          );
          await hosts.refetch();
          qc.invalidateQueries({ queryKey: ["topology"] });
        } catch (e) {
          message.error((e as Error).message);
        }
      },
    });
  };

  const doRevive = async (zone: string, host: string) => {
    try {
      const r =
        host === "*" ? await api.reviveZone(zone) : await api.reviveHost(zone, host);
      message.success(
        `已恢复，启动 ${r.started.length} 台` +
          (r.run_id ? "，正在重装网关策略与镜像" : ""),
      );
      await hosts.refetch();
      qc.invalidateQueries({ queryKey: ["topology"] });
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const doDestroy = async () => {
    if (!destroyZone) return;
    try {
      const r = await api.destroy(destroyZone);
      message.success(
        `已销毁 ${destroyZone}：删除 ${r.destroyed_cts.length} 台容器` +
          (r.bridge_deleted ? "、网桥" : r.kept_bridge ? `（桥保留：${r.kept_bridge}）` : ""),
      );
      setDestroyZone(null);
      setConfirmName("");
      await hosts.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const columns: ColumnsType<HostRow> = [
    {
      title: "Zone",
      dataIndex: "zone",
      width: 120,
      render: (v: string) => (
        <Space size={4}>
          <Text strong>{v}</Text>
          {zoneRetired(v) && <Tag color="orange">整 zone 废弃</Tag>}
        </Space>
      ),
      filters: zones.map((z) => ({ text: z, value: z })),
      onFilter: (v, r) => r.zone === v,
    },
    {
      title: "主机",
      dataIndex: "host",
      width: 150,
      render: (v: string, r) => (
        <Space size={4}>
          <span>{v}</span>
          {r.retired && r.host !== "*" && <Tag color="orange">废弃</Tag>}
        </Space>
      ),
    },
    {
      title: "角色",
      dataIndex: "role",
      width: 100,
      render: (v: string | null) =>
        v ? <Tag color={ROLE_COLOR[v] ?? "default"}>{ROLE_LABEL[v] ?? v}</Tag> : <Text type="secondary">—</Text>,
    },
    {
      title: "IPv4",
      dataIndex: "ipv4",
      width: 130,
      render: (v: string | null) => (
        <span className="mono">{(v ?? "—").replace(/\/\d+$/, "")}</span>
      ),
    },
    {
      title: "CT",
      dataIndex: "vmid",
      width: 76,
      render: (v: number) => <code>{v}</code>,
    },
    {
      title: "状态",
      dataIndex: "status",
      width: 100,
      render: (v: string, r) => statusTag(v, r.retired),
    },
    {
      title: "操作",
      render: (_: unknown, r) => (
        <Space size={4} wrap>
          <Tooltip title="Web 终端（ttyd + ZMODEM 传文件）">
            <Button
              size="small"
              icon={<CodeOutlined />}
              disabled={!r.exists || r.status !== "running" || r.retired}
              onClick={() => setTerm({ zone: r.zone, host: r.host })}
            >
              终端
            </Button>
          </Tooltip>
          <Tooltip title="改 CPU / 内存 / 系统盘（写回规格，执行后生效）">
            <Button
              size="small"
              icon={<SettingOutlined />}
              disabled={!r.exists || r.retired || !r.spec}
              onClick={() => setCfg({ spec: r.spec!, zone: r.zone, host: r.host })}
            >
              配置
            </Button>
          </Tooltip>
          <Tooltip title="快照：创建 / 回滚 / 删除">
            <Button
              size="small"
              icon={<CameraOutlined />}
              disabled={!r.exists || r.retired}
              onClick={() => setSnap({ zone: r.zone, host: r.host })}
            >
              快照
            </Button>
          </Tooltip>
          {r.role === "gateway" && (
            <Tooltip title="出网开关 / 端口映射 / VPN">
              <Button size="small" icon={<SafetyOutlined />} onClick={() => setGw(r.zone)}>
                网关
              </Button>
            </Tooltip>
          )}
          {r.retired ? (
            <Tooltip title="容器重新启动，网关策略与镜像实例自动装回">
              <Button
                size="small"
                type="primary"
                ghost
                icon={<UndoOutlined />}
                onClick={() => doRevive(r.zone, r.host)}
              >
                恢复
              </Button>
            </Tooltip>
          ) : (
            <>
              <Select
                size="small"
                style={{ width: 96 }}
                placeholder="电源"
                value={undefined}
                disabled={!r.exists}
                onChange={(a: "start" | "stop" | "reboot") => doPower(r, a)}
                options={[
                  { value: "start", label: "启动", disabled: r.status === "running" },
                  { value: "stop", label: "停止", disabled: r.status !== "running" },
                  { value: "reboot", label: "重启", disabled: r.status !== "running" },
                ]}
              />
              <Tooltip title="停机但保留 vmid / IP / 磁盘 / 快照">
                <Button
                  size="small"
                  type="text"
                  icon={<PauseCircleOutlined />}
                  onClick={() => doRetire(r.zone, r.host)}
                />
              </Tooltip>
            </>
          )}
        </Space>
      ),
    },
  ];

  return (
    <>
      {/* 状态总览：一眼看出机群规模，不用数表格 */}
      <Card size="small" style={{ marginBottom: 16 }}>
        <Space size={32} wrap>
          <Statistic title="主机总数" value={stats.total} />
          <Statistic
            title="运行中"
            value={stats.running}
            valueStyle={{ color: "#52c41a" }}
          />
          <Statistic title="已停止" value={stats.stopped} />
          <Statistic
            title="已废弃"
            value={stats.retired}
            valueStyle={stats.retired ? { color: "#faad14" } : undefined}
          />
        </Space>
      </Card>

      {/* 已删除规格留下的记录：不删，但要让人知道它们存在、且可以清掉 */}
      {orphanCount > 0 && (
        <Alert
          type={orphanAlive.length > 0 ? "warning" : "info"}
          showIcon
          style={{ marginBottom: 16 }}
          message={
            orphanAlive.length > 0
              ? `已隐藏 ${orphanCount} 条记录，其中 ${orphanAlive.length} 台容器还在`
              : `已隐藏 ${orphanCount} 条历史记录`
          }
          description={
            <>
              这些主机属于已删除的规格（
              {Array.from(
                new Set(
                  orphanRows.map((r) =>
                    // zone 还在（比如 lab 里的 probe）就具体到主机，
                    // 否则只报 zone 名，免得让人以为整个 lab 都失效了
                    zones.includes(r.zone) ? `${r.zone}/${r.host}` : r.zone,
                  ),
                ),
              ).join("、")}
              ）。
              {orphanAlive.length > 0 ? (
                <>
                  容器本身还在宿主机上跑着，占用 CPU/内存，也占着桥——
                  「销毁容器」会停机后删除（磁盘一起删，不可恢复）；
                  还可能挂着网桥残留，销毁完再清桥。
                </>
              ) : (
                <>容器已不存在，但 vmid 分配记录还留在库里。</>
              )}
              上面的统计与网络列表只算现存规格。
              {/* 按钮放在描述块里而不是 action 里：文案一长，antd 的 action
                  会和文字挤在同一行，最后几个字被按钮压住看不清。 */}
              <div style={{ marginTop: 10 }}>
                <Space size={8}>
                  <Popconfirm
                    title="清理历史记录？"
                    description="只删除「规格里没有、容器也不存在」的记录；容器还在的会保留。"
                    okText="清理"
                    cancelText="取消"
                    onConfirm={pruneOrphans}
                  >
                    <Button size="small">清理记录</Button>
                  </Popconfirm>
                  {orphanAlive.length > 0 && (
                    <Popconfirm
                      title={`销毁 ${orphanAlive.length} 台孤儿容器？`}
                      description={
                        <div style={{ maxWidth: 320 }}>
                          停机后执行 pct destroy（磁盘一起删，不可恢复）：
                          <div style={{ marginTop: 6 }}>
                            {orphanAlive.map((r) => `${r.zone}/${r.host}`).join("、")}
                          </div>
                        </div>
                      }
                      okText="销毁"
                      okButtonProps={{ danger: true }}
                      cancelText="取消"
                      onConfirm={purgeOrphans}
                    >
                      <Button size="small" danger>
                        销毁容器
                      </Button>
                    </Popconfirm>
                  )}
                </Space>
              </div>
            </>
          }
        />
      )}

      <Card
        size="small"
        title="网络与网关"
        style={{ marginBottom: 16 }}
        extra={
          <Text type="secondary" style={{ fontSize: 12 }}>
            网关策略、出网与 VPN 都在这里进
          </Text>
        }
      >
        <Table
          size="small"
          rowKey="zone"
          pagination={false}
          dataSource={zoneRows}
          locale={{
            emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有网络" />,
          }}
          columns={[
            {
              title: "网络",
              dataIndex: "zone",
              width: 130,
              render: (z: string, r) => (
                <Space size={6}>
                  <Text strong>{z}</Text>
                  {r.retired && <Tag color="orange">已废弃</Tag>}
                </Space>
              ),
            },
            {
              title: "网关主机",
              width: 190,
              render: (_: unknown, r) =>
                r.gw ? (
                  <Space size={6}>
                    <Text className="mono">{r.gw.host}</Text>
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      CT {r.gw.vmid}
                    </Text>
                    {r.gw.status === "running" ? (
                      <Tag color="green">运行</Tag>
                    ) : (
                      <Tag>停止</Tag>
                    )}
                  </Space>
                ) : (
                  <Text type="secondary">—</Text>
                ),
            },
            {
              title: "网段",
              width: 170,
              render: (_: unknown, r) => (
                <Text className="mono" type="secondary">
                  {r.meta.cidr ?? "—"}
                </Text>
              ),
            },
            {
              title: "出网",
              width: 90,
              render: (_: unknown, r) =>
                !r.gw ? (
                  <Text type="secondary">—</Text>
                ) : r.meta.egress ? (
                  <Tag color="green">开</Tag>
                ) : (
                  <Tag>关</Tag>
                ),
            },
            {
              title: "VPN",
              width: 150,
              render: (_: unknown, r) => {
                const v = (r.meta.vpn as string[] | undefined) ?? [];
                return v.length === 0 ? (
                  <Text type="secondary">—</Text>
                ) : (
                  <Space size={4} wrap>
                    {v.map((x) => (
                      <Tag key={x} className="mono">
                        {x}
                      </Tag>
                    ))}
                  </Space>
                );
              },
            },
            {
              title: "主机",
              width: 100,
              render: (_: unknown, r) => (
                <Text>
                  {r.running}/{r.total}
                </Text>
              ),
            },
            {
              title: "流量监控",
              width: 140,
              render: (_: unknown, r) => {
                const name = r.meta.monitor ? String(r.meta.monitor) : "";
                if (!name) return <Text type="secondary">—</Text>;
                // 监控机可能建在别的网络里（管理口挂外网时落在 wan）。
                // 只写名字会让人以为本网络里多了一台机器，所以标出实际落点。
                const actual = liveRows.find(
                  (x) => x.host === name && x.role === "monitor",
                )?.zone;
                const elsewhere = actual && actual !== r.zone;
                return (
                  <Tooltip
                    title={
                      elsewhere ? `声明在 ${r.zone}，实际建在 ${actual}` : undefined
                    }
                  >
                    <Text className="mono" style={{ fontSize: 12 }}>
                      {name}
                      {elsewhere && (
                        <Text type="secondary" style={{ fontSize: 11 }}>
                          {" → "}
                          {actual}
                        </Text>
                      )}
                    </Text>
                  </Tooltip>
                );
              },
            },
            {
              title: "操作",
              width: 210,
              render: (_: unknown, r) => {
                // `wan` 是外部网络（宿主上的外网桥），不是规格里的 zone：
                // 它没有网关容器，出网/端口映射/VPN 都配不到它头上，
                // 也不该出现"销毁网络"——点了只会得到一句找不到 zone。
                if (r.zone === "wan") {
                  return (
                    <Tooltip title="wan 是宿主上的外部网络，不属于任何规格：没有网关容器，也没有可配的策略。出网开关、端口映射与 VPN 在承载它的 zone 的网关上配置。">
                      <Text type="secondary" style={{ fontSize: 12 }}>
                        仅查看
                      </Text>
                    </Tooltip>
                  );
                }
                return (
                  <Space size={4}>
                    <Button
                      size="small"
                      icon={<SafetyOutlined />}
                      onClick={() => setGw(r.zone)}
                    >
                      网关策略
                    </Button>
                    <Dropdown
                    menu={{
                      items: [
                        {
                          key: "retire",
                          icon: <PauseCircleOutlined />,
                          label: "废弃整个网络（停机保号）",
                          onClick: () => doRetire(r.zone, "*"),
                        },
                        {
                          key: "revive",
                          icon: <UndoOutlined />,
                          label: "恢复整个网络",
                          disabled: !r.retired,
                          onClick: () => doRevive(r.zone, "*"),
                        },
                      ],
                    }}
                  >
                    <Button size="small" icon={<MoreOutlined />} />
                  </Dropdown>
                  <Popconfirm
                    open={destroyZone === r.zone}
                    title={`销毁网络 ${r.zone}？`}
                    description={
                      <Space direction="vertical" size={8} style={{ maxWidth: 360 }}>
                        <Text type="danger">
                          删除该网络的全部容器、网桥与分配记录，不可逆。
                        </Text>
                        <Text>输入网络名确认：</Text>
                        <Input
                          size="small"
                          value={confirmName}
                          onChange={(e) => setConfirmName(e.target.value)}
                          placeholder={r.zone}
                        />
                      </Space>
                    }
                    okText="销毁"
                    okButtonProps={{ danger: true, disabled: confirmName !== r.zone }}
                    cancelText="取消"
                    onConfirm={doDestroy}
                    onCancel={() => {
                      setDestroyZone(null);
                      setConfirmName("");
                    }}
                  >
                    <Button
                      size="small"
                      danger
                      type="text"
                      icon={<PoweroffOutlined />}
                      onClick={() => {
                        setDestroyZone(r.zone);
                        setConfirmName("");
                      }}
                    />
                  </Popconfirm>
                </Space>
                );
              },
            },
          ]}
        />
      </Card>

      {/* 规格删了不等于桥删了：destroy 需要 zone 还在规格里，先删规格就留下孤儿桥 */}
      {orphanBridges.length > 0 && (
        <Card
          size="small"
          title={
            <Space size={8}>
              <span>宿主网桥残留</span>
              <Tag color="orange">{orphanBridges.length}</Tag>
            </Space>
          }
          style={{ marginBottom: 16 }}
          extra={
            <Popconfirm
              title="清理残留网桥？"
              description="只删「无成员口」的孤儿桥；有成员口的会保留并列出来。桥上残留的地址会随接口一起删掉。"
              okText="清理"
              cancelText="取消"
              onConfirm={pruneBridges}
            >
              <Button size="small">清理可自动删除的</Button>
            </Popconfirm>
          }
        >
          <Table
            size="small"
            rowKey="name"
            pagination={false}
            dataSource={orphanBridges}
            columns={[
              {
                title: "网桥",
                dataIndex: "name",
                width: 130,
                render: (v: string) => <code>{v}</code>,
              },
              {
                title: "原属网络",
                dataIndex: "zone",
                width: 110,
                render: (v: string | null) => v ?? "—",
              },
              {
                title: "地址",
                dataIndex: "ips",
                width: 190,
                render: (v: string) =>
                  v ? (
                    <span className="mono">{v}</span>
                  ) : (
                    <Text type="secondary">无</Text>
                  ),
              },
              {
                title: "成员口",
                dataIndex: "members",
                width: 90,
                render: (v: number) =>
                  v > 0 ? <Text>{v} 个</Text> : <Text type="secondary">无</Text>,
              },
              {
                title: "存在形态",
                width: 130,
                render: (_: unknown, r) =>
                  r.kernel ? (
                    <Text>内核 + 配置</Text>
                  ) : (
                    <Tooltip title="内核里已经没了，但 /etc/network/interfaces 里还留着 auto 段，宿主重启或 ifreload 会把它重建回来">
                      <Tag>仅配置残留</Tag>
                    </Tooltip>
                  ),
              },
              {
                title: "状态",
                width: 130,
                render: (_: unknown, r) =>
                  r.safe ? (
                    <Tooltip title="网络已从规格删掉，桥上也没有成员口：删除接口时连残留地址一起清掉">
                      <Tag color="orange">可自动删除</Tag>
                    </Tooltip>
                  ) : (
                    <Tooltip title="还有容器挂在它上面（成员口 > 0），删掉会断开正在用的链路">
                      <Tag color="red">需要人工确认</Tag>
                    </Tooltip>
                  ),
              },
            ]}
          />
          {orphanBridges.some((b) => !b.safe) && (
            <Text type="secondary" style={{ fontSize: 12 }}>
              标记「需要人工确认」的桥上还有成员口（容器仍挂着），删掉会断链。
              先在主机页把它们销毁或改挂到别的网络，再回来清理。确认不再使用后也可以
              在宿主机执行 <code>ip link del 桥名</code>，并删掉{" "}
              <code>/etc/network/interfaces</code> 里对应的 <code>auto 桥名</code> 段
              ——只删接口不删配置，重启会被重建。
            </Text>
          )}
        </Card>
      )}

      {/* PVE 面板里手工建的虚拟机不会进 vmid 分配表，以前界面上完全看不见 */}
      {unmanaged.length > 0 && (
        <Card
          size="small"
          title={
            <Space size={8}>
              <span>宿主上的其它虚拟机</span>
              <Tag>{unmanaged.length}</Tag>
            </Space>
          }
          style={{ marginBottom: 16 }}
          extra={
            <Text type="secondary" style={{ fontSize: 12 }}>
              不是本平台创建的，不受规格管理
            </Text>
          }
        >
          <Table
            size="small"
            rowKey={(r) => String(r.vmid)}
            pagination={false}
            dataSource={unmanaged}
            columns={[
              {
                title: "CT / VM",
                width: 110,
                render: (_: unknown, r) => (
                  <Text className="mono">{r.vmid ?? "—"}</Text>
                ),
              },
              {
                title: "类型",
                width: 100,
                render: (_: unknown, r) =>
                  r.kind === "lxc" ? <Tag>容器</Tag> : <Tag color="blue">虚拟机</Tag>,
              },
              {
                title: "名称",
                dataIndex: "name",
                render: (v: string) => v || <Text type="secondary">—</Text>,
              },
              {
                title: "状态",
                width: 100,
                render: (_: unknown, r) =>
                  r.status === "running" ? (
                    <Tag color="green">运行</Tag>
                  ) : (
                    <Tag>停止</Tag>
                  ),
              },
              { title: "节点", dataIndex: "node", width: 120 },
              {
                title: "",
                width: 88,
                render: (_: unknown, r) =>
                  r.kind === "lxc" && r.vmid ? (
                    <Button size="small" onClick={() => openAdopt(r)}>
                      纳管
                    </Button>
                  ) : (
                    <Tooltip title="目前只支持 LXC 容器">
                      <Button size="small" disabled>
                        纳管
                      </Button>
                    </Tooltip>
                  ),
              },
            ]}
          />
          <Text type="secondary" style={{ fontSize: 12 }}>
            这些是在 PVE 面板或用其它工具直接建的。点「纳管」会把主机写进规格并绑定
            vmid——只登记，不改造：下次「应用」才真正接管（配网络、主机名、资源）。
            不用的请到 PVE 里删除。
          </Text>

          <Modal
            title={`纳管 ${adopt?.name || ""}（vmid ${adopt?.vmid ?? ""}）`}
            open={!!adopt}
            onCancel={() => setAdopt(null)}
            onOk={submitAdopt}
            okText="写入规格"
            confirmLoading={adopting}
            destroyOnClose
          >
            <Alert
              type="info"
              showIcon
              style={{ marginBottom: 14 }}
              message="只登记，不改造"
              description="纳管只写规格与 vmid 映射；下一次「应用」才会按规格配网络、主机名与资源。"
            />
            <Form layout="vertical" size="small">
              <Form.Item label="规格" required>
                <Select
                  placeholder="选择规格"
                  value={adoptSpec}
                  onChange={(v) => {
                    setAdoptSpec(v);
                    setAdoptZone(undefined);
                  }}
                  options={(specList.data?.specs ?? []).map((s) => ({
                    value: s.name,
                    label: s.valid ? `${s.name}（${s.hosts} 台）` : `${s.name}（解析失败）`,
                    disabled: !s.valid,
                  }))}
                />
              </Form.Item>
              <Form.Item label="zone" required>
                <Select
                  placeholder={adoptSpec ? "选择 zone" : "先选规格"}
                  value={adoptZone}
                  onChange={setAdoptZone}
                  disabled={!adoptSpec}
                  loading={adoptModel.isFetching}
                  options={adoptZones.map((z) => ({ value: z, label: z }))}
                />
              </Form.Item>
              <Form.Item
                label="主机名"
                required
                extra="小写字母、数字、连字符；会成为容器主机名"
              >
                <Input
                  value={adoptHost}
                  onChange={(e) => setAdoptHost(e.target.value)}
                  placeholder="pc1"
                />
              </Form.Item>
              <Form.Item label="主机位" extra="留空则按角色自动分配（推荐先留空）">
                <Input
                  value={adoptIp}
                  onChange={(e) => setAdoptIp(e.target.value)}
                  placeholder="30"
                />
              </Form.Item>
            </Form>
          </Modal>
        </Card>
      )}

      <Space style={{ marginBottom: 12 }} size={12} wrap>
        <Input
          allowClear
          size="small"
          prefix={<SearchOutlined style={{ color: "rgba(0,0,0,.25)" }} />}
          placeholder="搜主机名 / IP / CT 号 / zone"
          style={{ width: 240 }}
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
        <Button
          icon={<ReloadOutlined />}
          size="small"
          loading={hosts.isFetching}
          onClick={() => hosts.refetch()}
        >
          刷新
        </Button>
      </Space>

      {/* 批量操作条：选中才出现，动作前先按可操作性过滤 */}
      {selectedRows.length > 0 && (
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 12 }}
          message={
            <Space size={8} wrap>
              <span>
                已选 <Text strong>{selectedRows.length}</Text> 台可操作
                {selected.length > selectedRows.length
                  ? `（另有 ${selected.length - selectedRows.length} 台不可操作，将被跳过）`
                  : ""}
              </span>
              <Button size="small" onClick={() => doBatchPower("start", selectedRows)}>
                批量启动
              </Button>
              <Button size="small" onClick={() => doBatchPower("stop", selectedRows)}>
                批量停止
              </Button>
              <Button size="small" onClick={() => doBatchPower("reboot", selectedRows)}>
                批量重启
              </Button>
              <Button size="small" icon={<CameraOutlined />} onClick={() => setSnapBatch(selectedRows)}>
                批量快照
              </Button>
              <Button size="small" danger onClick={() => doBatchRetire(selectedRows)}>
                批量废弃
              </Button>
              <Button size="small" type="text" onClick={() => setSelected([])}>
                取消选择
              </Button>
            </Space>
          }
        />
      )}

      <Table
        rowKey={(r) => `${r.zone}/${r.host}`}
        loading={hosts.isLoading}
        dataSource={filtered}
        columns={columns}
        size="small"
        pagination={filtered.length > 20 ? { pageSize: 20, showSizeChanger: false } : false}
        rowClassName={(r) => (r.retired ? "row-retired" : "")}
        rowSelection={{
          selectedRowKeys: selected,
          onChange: setSelected,
          // 整 zone 行（host === "*"）不参与勾选：它不是一台真主机
          getCheckboxProps: (r) => ({ disabled: r.host === "*" }),
        }}
        locale={{
          emptyText: (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description={
                search ? "没有匹配的主机" : (
                  <span>
                    <CloudServerOutlined /> 还没有主机——先在「编排」里 apply 一份规格
                  </span>
                )
              }
            />
          ),
        }}
      />

      <TerminalModal target={term} onClose={() => setTerm(null)} />
      <SnapshotModal target={snap} onClose={() => setSnap(null)} />
      <GatewayPanel zone={gw} onClose={() => setGw(null)} />

      {/* 批量快照：一个名字，逐台创建——方便之后用同一个还原点对比 */}
      <Modal
        open={!!snapBatch}
        title={`为 ${snapBatch?.length ?? 0} 台主机创建快照`}
        okText="创建"
        cancelText="取消"
        okButtonProps={{ disabled: !snapName }}
        onCancel={() => {
          setSnapBatch(null);
          setSnapName("");
        }}
        onOk={async () => {
          const list = snapBatch ?? [];
          let ok = 0;
          const fail: string[] = [];
          for (const r of list) {
            try {
              await api.snapshotOp(r.zone, r.host, "create", snapName);
              ok += 1;
            } catch (e) {
              fail.push(`${r.zone}/${r.host}: ${(e as Error).message}`);
            }
          }
          message[fail.length ? "warning" : "success"](
            `快照：${ok}/${list.length} 台成功` + (fail.length ? `，失败：${fail.join("；")}` : ""),
          );
          setSnapBatch(null);
          setSnapName("");
          setSelected([]);
        }}
      >
        <Input
          placeholder="快照名（字母数字 -_）"
          value={snapName}
          onChange={(e) => setSnapName(e.target.value)}
          onPressEnter={() => {}}
          style={{ marginTop: 8 }}
        />
        <Text type="secondary" style={{ fontSize: 12, display: "block", marginTop: 8 }}>
          {snapBatch?.map((r) => `${r.zone}/${r.host}`).join("、")}
        </Text>
      </Modal>

      <HostConfigEditor target={cfg} onClose={() => setCfg(null)} />
    </>
  );
}
