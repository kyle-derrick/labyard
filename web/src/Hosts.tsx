/** 主机运维：电源、快照、Web 终端。
 *
 * 主机身份的唯一权威是后端的 vmids 分配表（`/api/hosts`），不是 PVE 的
 * 全量 VM 列表——列表里可能有别的实验留下的容器，混进来只会误导。
 */
import { useCallback, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  App as AntApp,
  Button,
  Empty,
  Input,
  Modal,
  Popconfirm,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import {
  CloudServerOutlined,
  CodeOutlined,
  CameraOutlined,
  PoweroffOutlined,
  ReloadOutlined,
  SyncOutlined,
} from "@ant-design/icons";
import { api, terminalUrl, type HostRow } from "./api";

const { Text } = Typography;

function statusTag(status: string) {
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
  const { message } = AntApp.useApp();
  const [term, setTerm] = useState<{ zone: string; host: string } | null>(null);
  const [snap, setSnap] = useState<{ zone: string; host: string } | null>(null);
  const [destroyZone, setDestroyZone] = useState<string | null>(null);
  const [confirmName, setConfirmName] = useState("");

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
      render: (v: string) => <Text strong>{v}</Text>,
      filters: zones.map((z) => ({ text: z, value: z })),
      onFilter: (v, r) => r.zone === v,
    },
    { title: "主机", dataIndex: "host", width: 160 },
    {
      title: "CT",
      dataIndex: "vmid",
      width: 90,
      render: (v: number) => <code>{v}</code>,
    },
    {
      title: "状态",
      dataIndex: "status",
      width: 110,
      render: (v: string) => statusTag(v),
    },
    {
      title: "操作",
      render: (_: unknown, r: HostRow) => (
        <Space size={4} wrap>
          <Tooltip title="Web 终端（ttyd + ZMODEM 传文件）">
            <Button
              size="small"
              icon={<CodeOutlined />}
              disabled={!r.exists || r.status !== "running"}
              onClick={() => setTerm({ zone: r.zone, host: r.host })}
            >
              终端
            </Button>
          </Tooltip>
          <Tooltip title="快照：创建 / 回滚 / 删除">
            <Button
              size="small"
              icon={<CameraOutlined />}
              disabled={!r.exists}
              onClick={() => setSnap({ zone: r.zone, host: r.host })}
            >
              快照
            </Button>
          </Tooltip>
          <Select
            size="small"
            style={{ width: 108 }}
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
        </Space>
      ),
    },
  ];

  return (
    <>
      <Space style={{ marginBottom: 16 }} size={12} wrap>
        <Button
          icon={<ReloadOutlined />}
          size="small"
          loading={hosts.isFetching}
          onClick={() => hosts.refetch()}
        >
          刷新
        </Button>
        {zones.map((z) => (
          <Popconfirm
            key={z}
            open={destroyZone === z}
            title={`销毁 zone ${z}？`}
            description={
              <Space direction="vertical" size={8} style={{ maxWidth: 360 }}>
                <Text type="danger">
                  删除该 zone 全部容器、网桥与分配记录，不可逆。
                </Text>
                <Text>输入 zone 名确认：</Text>
                <Input
                  size="small"
                  value={confirmName}
                  onChange={(e) => setConfirmName(e.target.value)}
                  placeholder={z}
                />
              </Space>
            }
            okText="销毁"
            okButtonProps={{ danger: true, disabled: confirmName !== z }}
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
              icon={<PoweroffOutlined />}
              onClick={() => {
                setDestroyZone(z);
                setConfirmName("");
              }}
            >
              销毁 {z}
            </Button>
          </Popconfirm>
        ))}
      </Space>

      <Table
        rowKey={(r) => `${r.zone}/${r.host}`}
        loading={hosts.isLoading}
        dataSource={rows}
        columns={columns}
        size="small"
        pagination={false}
        locale={{
          emptyText: (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description={
                <span>
                  <CloudServerOutlined /> 还没有主机——先在「编排」里 apply 一份规格
                </span>
              }
            />
          ),
        }}
      />

      <TerminalModal target={term} onClose={() => setTerm(null)} />
      <SnapshotModal target={snap} onClose={() => setSnap(null)} />
    </>
  );
}
