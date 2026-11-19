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
import { useCallback, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Dropdown,
  Empty,
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
  SyncOutlined,
  UndoOutlined,
} from "@ant-design/icons";
import { api, terminalUrl, type HostRow } from "./api";
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
  monitor: "orange",
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
  const zones = useMemo(
    () => Array.from(new Set(rows.map((r) => r.zone))),
    [rows],
  );

  /** 全局搜索：主机名 / IP / CT 号 / zone 任意命中就保留 */
  const filtered = useMemo(() => {
    const s = search.trim().toLowerCase();
    if (!s) return rows.filter((r) => r.host !== "*");
    return rows.filter(
      (r) =>
        r.host !== "*" &&
        (r.host.toLowerCase().includes(s) ||
          r.zone.toLowerCase().includes(s) ||
          (r.ipv4 ?? "").replace(/\/\d+$/, "").includes(s) ||
          String(r.vmid).includes(s)),
    );
  }, [rows, search]);

  /** 选中行里"真的能操作"的：容器存在且未废弃。批量动作前先算好这个集合。 */
  const selectedRows = useMemo(
    () =>
      selected
        .map((k) => rows.find((r) => `${r.zone}/${r.host}` === k))
        .filter((r): r is HostRow => !!r && r.exists && !r.retired),
    [rows, selected],
  );

  /** 统计卡片 */
  const stats = useMemo(() => {
    const real = rows.filter((r) => r.host !== "*");
    return {
      total: real.length,
      running: real.filter((r) => r.status === "running" && !r.retired).length,
      stopped: real.filter((r) => r.status === "stopped" && !r.retired).length,
      retired: real.filter((r) => r.retired).length,
    };
  }, [rows]);

  // zone 整体废弃的判据：该 zone 下所有主机都 retired。
  // `/api/hosts` 只列有 vmid 的真实主机，`*` 这种"整 zone"标记不会出现在行里，
  // 所以不能直接找 host === "*"。
  const zoneRetired = useCallback(
    (z: string) => {
      const zs = rows.filter((r) => r.zone === z);
      return zs.length > 0 && zs.every((r) => r.retired);
    },
    [rows],
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
      const list = rows.filter((r) => r.zone === z && r.host !== "*");
      return {
        zone: z,
        gw: list.find((r) => r.role === "gateway"),
        meta: meta.get(z) ?? {},
        total: list.length,
        running: list.filter((r) => r.status === "running" && !r.retired).length,
        retired: zoneRetired(z),
      };
    });
  }, [zones, rows, topo.data, zoneRetired]);

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
              render: (_: unknown, r) =>
                r.meta.monitor ? (
                  <Text className="mono" style={{ fontSize: 12 }}>
                    {String(r.meta.monitor)}
                  </Text>
                ) : (
                  <Text type="secondary">—</Text>
                ),
            },
            {
              title: "操作",
              width: 210,
              render: (_: unknown, r) => (
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
              ),
            },
          ]}
        />
      </Card>

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
    </>
  );
}
