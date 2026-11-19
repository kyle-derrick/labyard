/** 规格管理：多份规格的文件级 CRUD + 文件内的 zone 级 CRUD。
 *
 * 规格文件是唯一真相源（`~/.config/labyard/specs/*.yaml`），这一页只做两件事：
 * 让"复制一份做变体"和"加个 zone"不用打开编辑器；改结构时仍然落回 YAML，
 * 且走 yaml-edit 就地替换——注释、顺序都保住。
 *
 * 删 zone 只从规格里删，**不动 PVE**：删容器是「主机」页 destroy 的事，
 * 那里要两次确认。这里悄悄删机房是灾难。
 */
import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Drawer,
  Empty,
  Input,
  Modal,
  Popconfirm,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import type { Key } from "react";
import {
  CaretRightOutlined,
  CopyOutlined,
  DeleteOutlined,
  EditOutlined,
  FileAddOutlined,
  PlusOutlined,
  ReloadOutlined,
  SaveOutlined,
} from "@ant-design/icons";
import { api, type SpecMeta, type TopoNode } from "./api";

const { Text, Paragraph } = Typography;

/** 规格名与 zone 名共用同一套白名单（后端 check_name 也是这么收的）。 */
const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,39}$/;

const ZONE_TEMPLATE = `uplink: wan          # 也可以是上游 zone 名，做嵌套内网
v4: 10.30.1.0/24
hosts:
  gateway: { role: gateway }
  server1: { role: server }
`;

/** 整份规格的文本编辑抽屉。保存前由后端 parse + resolve，非法直接拒。 */
function SpecEditor({
  name,
  onClose,
  onSaved,
}: {
  name: string | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const { message } = AntApp.useApp();
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const q = useQuery({
    queryKey: ["spec-detail", name],
    queryFn: () => api.specDetail(name!),
    enabled: !!name,
  });

  // 换了一份规格就把编辑缓冲清掉，否则会把上一份的内容带过来
  useEffect(() => {
    setText(null);
    setErr(null);
  }, [name]);

  // 打开时把服务端那份灌进编辑框（只灌一次，之后不再覆盖用户输入）
  const value = text ?? q.data?.yaml ?? "";
  const dirty = text !== null && text !== (q.data?.yaml ?? "");

  const save = async () => {
    if (!name) return;
    setBusy(true);
    setErr(null);
    try {
      await api.putSpecNamed(name, value);
      message.success("已保存");
      setText(null);
      onSaved();
      await q.refetch();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Drawer
      open={!!name}
      onClose={onClose}
      width={720}
      destroyOnClose
      title={
        <Space>
          <EditOutlined />
          <span>编辑规格 · {name}</span>
          {dirty && <Tag color="orange">未保存</Tag>}
        </Space>
      }
      extra={
        <Space>
          <Button size="small" onClick={onClose}>
            关闭
          </Button>
          <Button
            size="small"
            type="primary"
            icon={<SaveOutlined />}
            disabled={!dirty}
            loading={busy}
            onClick={save}
          >
            保存
          </Button>
        </Space>
      }
    >
      {err && (
        <Alert
          type="error"
          showIcon
          message="校验失败，未保存"
          description={<span className="mono">{err}</span>}
          style={{ marginBottom: 12 }}
        />
      )}
      <Input.TextArea
        value={value}
        onChange={(e) => setText(e.target.value)}
        autoSize={{ minRows: 26, maxRows: 40 }}
        style={{ fontFamily: "var(--mono)", fontSize: 12 }}
        spellCheck={false}
      />
      <Paragraph type="secondary" style={{ fontSize: 12, marginTop: 8, marginBottom: 0 }}>
        保存即重新解析并校验三层合并；非法就整份拒绝，磁盘上的文件保持不变。
      </Paragraph>
    </Drawer>
  );
}

/** 单个 zone 的片段编辑抽屉。新建与编辑共用一个框，只是接口不同。 */
function ZoneEditor({
  target,
  onClose,
  onSaved,
}: {
  target: { spec: string; zone: string | null } | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const { message } = AntApp.useApp();
  const [zoneName, setZoneName] = useState("");
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const isNew = !!target && target.zone === null;
  const q = useQuery({
    queryKey: ["zone-detail", target?.spec, target?.zone],
    queryFn: () => api.zoneDetail(target!.spec, target!.zone!),
    enabled: !!target && target.zone !== null,
  });

  // 换了目标（或从"编辑"切到"新增"）就清空缓冲与报错
  useEffect(() => {
    setText(null);
    setErr(null);
    setZoneName("");
  }, [target?.spec, target?.zone]);

  const initial = isNew ? ZONE_TEMPLATE : (q.data?.yaml ?? "");
  const value = text ?? initial;
  const zone = isNew ? zoneName : (target?.zone ?? "");
  const canSave = !!zone && !!value.trim() && (isNew ? NAME_RE.test(zone) : true);

  const save = async () => {
    if (!target) return;
    setBusy(true);
    setErr(null);
    try {
      if (isNew) {
        await api.addZone(target.spec, zone, value);
        message.success(`zone ${zone} 已加入 ${target.spec}`);
      } else {
        await api.updateZone(target.spec, zone, value);
        message.success(`zone ${zone} 已更新`);
      }
      setText(null);
      setZoneName("");
      onSaved();
      onClose();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Drawer
      open={!!target}
      onClose={onClose}
      width={560}
      destroyOnClose
      title={
        <Space>
          <EditOutlined />
          <span>
            {isNew ? "新增 zone" : `编辑 zone · ${target?.zone}`} @ {target?.spec}
          </span>
        </Space>
      }
      extra={
        <Space>
          <Button size="small" onClick={onClose}>
            取消
          </Button>
          <Button
            size="small"
            type="primary"
            icon={<SaveOutlined />}
            disabled={!canSave}
            loading={busy}
            onClick={save}
          >
            保存
          </Button>
        </Space>
      }
    >
      {isNew && (
        <Space direction="vertical" size={6} style={{ width: "100%", marginBottom: 10 }}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            zone 名（小写字母 / 数字 / - _）
          </Text>
          <Input
            placeholder="sub"
            value={zoneName}
            onChange={(e) => setZoneName(e.target.value.trim())}
            status={zoneName && !NAME_RE.test(zoneName) ? "error" : undefined}
            className="mono"
          />
        </Space>
      )}
      {err && (
        <Alert
          type="error"
          showIcon
          message="校验失败，未保存"
          description={<span className="mono">{err}</span>}
          style={{ marginBottom: 12 }}
        />
      )}
      <Input.TextArea
        value={value}
        onChange={(e) => setText(e.target.value)}
        autoSize={{ minRows: 14, maxRows: 28 }}
        style={{ fontFamily: "var(--mono)", fontSize: 12 }}
        spellCheck={false}
      />
      <Paragraph type="secondary" style={{ fontSize: 12, marginTop: 8, marginBottom: 0 }}>
        这是该 zone 在文件里的那一段。<b>保存时整段替换</b>
        ，文件里其余的注释与其他 zone 不受影响。
      </Paragraph>
    </Drawer>
  );
}

/** 展开行：该规格下的 zone 列表 + 增删改。 */
function ZoneTable({
  spec,
  onEdit,
}: {
  spec: string;
  onEdit: (t: { spec: string; zone: string | null }) => void;
}) {
  const { message } = AntApp.useApp();
  const qc = useQueryClient();
  const q = useQuery({
    queryKey: ["topology", spec],
    queryFn: () => api.topology(spec),
  });

  const nodes = q.data?.nodes ?? [];
  // 拓扑里的 zone 现在是一个"内网"节点（外网单独一个节点，不算 zone）
  const zones = nodes.filter((n) => n.kind === "network" && n.meta.external !== true);
  const hostCount = useMemo(() => {
    const m = new Map<string, number>();
    for (const n of nodes) {
      if (n.kind === "host" && n.zone) m.set(n.zone, (m.get(n.zone) ?? 0) + 1);
    }
    return m;
  }, [nodes]);

  const num = (n: TopoNode, k: string) =>
    typeof n.meta[k] === "number" ? (n.meta[k] as number) : 0;
  const yes = (n: TopoNode, k: string) => n.meta[k] === true;

  const del = async (zone: string) => {
    try {
      await api.deleteZone(spec, zone);
      message.success(`已从 ${spec} 删掉 zone ${zone}（PVE 上的容器没动）`);
      qc.invalidateQueries({ queryKey: ["topology"] });
      qc.invalidateQueries({ queryKey: ["specs"] });
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  return (
    <Space direction="vertical" size={10} style={{ width: "100%" }}>
      <Space size={8}>
        <Button
          size="small"
          icon={<PlusOutlined />}
          onClick={() => onEdit({ spec, zone: null })}
        >
          新增 zone
        </Button>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {zones.length} 个 zone
        </Text>
      </Space>

      <Table
        size="small"
        rowKey={(r) => r.zone ?? r.id}
        dataSource={zones}
        pagination={false}
        loading={q.isLoading}
        locale={{
          emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="这份规格还没有 zone" />,
        }}
        columns={[
          {
            title: "zone",
            dataIndex: "zone",
            width: 140,
            render: (_: unknown, r: TopoNode) => (
              <Text strong>{r.zone ?? r.label}</Text>
            ),
          },
          {
            title: "网段",
            width: 150,
            render: (_: unknown, r: TopoNode) => (
              <code>{(r.meta.cidr as string) ?? "—"}</code>
            ),
          },
          {
            title: "网桥",
            width: 120,
            render: (_: unknown, r: TopoNode) => (
              <span className="mono">{(r.meta.bridge as string) ?? "—"}</span>
            ),
          },
          {
            title: "特性",
            render: (_: unknown, r: TopoNode) => (
              <Space size={4} wrap>
                {yes(r, "nat") && <Tag>NAT44</Tag>}
                {yes(r, "egress") && <Tag color="green">出网</Tag>}
                {num(r, "pubs") > 0 && <Tag color="blue">映射 {num(r, "pubs")}</Tag>}
                {(r.meta.vpn as string[] | undefined)?.map((v) => (
                  <Tag key={v} color="purple">
                    {v}
                  </Tag>
                ))}
              </Space>
            ),
          },
          {
            title: "主机",
            width: 70,
            render: (_: unknown, r: TopoNode) => hostCount.get(r.zone ?? "") ?? 0,
          },
          {
            title: "操作",
            width: 160,
            render: (_: unknown, r: TopoNode) => (
              <Space size={4}>
                <Button
                  size="small"
                  icon={<EditOutlined />}
                  onClick={() => onEdit({ spec, zone: r.zone })}
                >
                  编辑
                </Button>
                <Popconfirm
                  title={`从规格里删掉 zone ${r.zone}？`}
                  description="只改 YAML，不删容器。要删容器去「主机」页。"
                  okText="删除"
                  okButtonProps={{ danger: true }}
                  onConfirm={() => del(r.zone!)}
                >
                  <Button size="small" danger type="text" icon={<DeleteOutlined />} />
                </Popconfirm>
              </Space>
            ),
          },
        ]}
      />
    </Space>
  );
}

export default function Specs() {
  const { message } = AntApp.useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [copyOf, setCopyOf] = useState<string | undefined>(undefined);
  const [editSpec, setEditSpec] = useState<string | null>(null);
  const [zoneEdit, setZoneEdit] = useState<{ spec: string; zone: string | null } | null>(null);
  const [expanded, setExpanded] = useState<readonly Key[]>([]);
  const [busy, setBusy] = useState(false);

  const q = useQuery({ queryKey: ["specs"], queryFn: api.specs });
  const rows = q.data?.specs ?? [];

  const refresh = () => {
    qc.invalidateQueries({ queryKey: ["specs"] });
    qc.invalidateQueries({ queryKey: ["topology"] });
  };

  const create = async () => {
    setBusy(true);
    try {
      await api.createSpec({ name: newName, copy_of: copyOf });
      message.success(copyOf ? `已从 ${copyOf} 复制出 ${newName}` : `已创建 ${newName}`);
      setCreating(false);
      setNewName("");
      setCopyOf(undefined);
      refresh();
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const del = async (name: string) => {
    try {
      await api.deleteSpec(name);
      message.success(`已删除规格 ${name}`);
      refresh();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const apply = async (name: string) => {
    try {
      const r = await api.applySpec(name);
      message.info(`已启动 ${r.run_id}，去「运行历史」看进度`);
      qc.invalidateQueries({ queryKey: ["runs"] });
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const columns: ColumnsType<SpecMeta> = [
    {
      title: "规格",
      dataIndex: "name",
      width: 150,
      render: (v: string, r) => (
        <Space size={6}>
          <Text strong>{v}</Text>
          {!r.valid && (
            <Tooltip title={r.error ?? "解析失败"}>
              <Tag color="red">坏文件</Tag>
            </Tooltip>
          )}
        </Space>
      ),
    },
    {
      title: "size",
      dataIndex: "bytes",
      width: 80,
      render: (v: number) => <span className="mono">{(v / 1024).toFixed(1)} KB</span>,
    },
    { title: "zone", dataIndex: "zones", width: 70 },
    { title: "主机", dataIndex: "hosts", width: 70 },
    {
      title: "修改时间",
      dataIndex: "modified",
      width: 180,
      render: (v: string | null) => (
        <span className="mono" style={{ fontSize: 11 }}>
          {v ?? "—"}
        </span>
      ),
    },
    {
      title: "操作",
      render: (_: unknown, r) => (
        <Space size={4} wrap>
          <Tooltip title="按这份规格执行一次（幂等，已完成的步骤会跳过）">
            <Button
              size="small"
              type="primary"
              ghost
              icon={<CaretRightOutlined />}
              disabled={!r.valid}
              onClick={() => apply(r.name)}
            >
              执行
            </Button>
          </Tooltip>
          <Button size="small" icon={<EditOutlined />} onClick={() => setEditSpec(r.name)}>
            编辑
          </Button>
          <Button
            size="small"
            icon={<CopyOutlined />}
            onClick={() => {
              setNewName("");
              setCopyOf(r.name);
              setCreating(true);
            }}
          >
            复制
          </Button>
          <Popconfirm
            title={`删除规格 ${r.name}？`}
            description="只删文件。已建的容器留在 PVE 上，去「主机」页处理。"
            okText="删除"
            okButtonProps={{ danger: true }}
            onConfirm={() => del(r.name)}
          >
            <Button size="small" danger type="text" icon={<DeleteOutlined />} />
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <>
      <Card
        size="small"
        title={
          <Space size={8}>
            <span>规格</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              {rows.length} 份 · 文件是唯一真相源
            </Text>
          </Space>
        }
        extra={
          <Space>
            <Button
              size="small"
              icon={<ReloadOutlined />}
              loading={q.isFetching}
              onClick={refresh}
            >
              刷新
            </Button>
            <Button
              size="small"
              type="primary"
              icon={<FileAddOutlined />}
              onClick={() => {
                setNewName("");
                setCopyOf(undefined);
                setCreating(true);
              }}
            >
              新建
            </Button>
          </Space>
        }
      >
        {q.isError && (
          <Alert type="error" showIcon message={(q.error as Error).message} style={{ marginBottom: 12 }} />
        )}
        <Table
          size="small"
          rowKey="name"
          loading={q.isLoading}
          dataSource={rows}
          columns={columns}
          pagination={false}
          expandable={{
            expandedRowKeys: expanded,
            onExpandedRowsChange: setExpanded,
            expandedRowRender: (r) => <ZoneTable spec={r.name} onEdit={setZoneEdit} />,
            rowExpandable: (r) => r.valid,
          }}
          locale={{
            emptyText: (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description="还没有规格——点右上「新建」"
              />
            ),
          }}
        />
      </Card>

      <Modal
        open={creating}
        title={copyOf ? `从 ${copyOf} 复制新规格` : "新建规格"}
        okText="创建"
        cancelText="取消"
        confirmLoading={busy}
        okButtonProps={{ disabled: !NAME_RE.test(newName) }}
        onOk={create}
        onCancel={() => setCreating(false)}
        destroyOnClose
      >
        <Space direction="vertical" size={10} style={{ width: "100%" }}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            规格名会成为文件名（小写字母 / 数字 / - _）
          </Text>
          <Input
            placeholder="red"
            value={newName}
            onChange={(e) => setNewName(e.target.value.trim())}
            className="mono"
            status={newName && !NAME_RE.test(newName) ? "error" : undefined}
          />
          <Text type="secondary" style={{ fontSize: 12 }}>
            {copyOf
              ? `内容复制自 ${copyOf}，之后两份独立演化。`
              : "内容是一个能直接执行的最小骨架（一个 zone + 网关 + 一台靶机）。"}
          </Text>
        </Space>
      </Modal>

      <SpecEditor
        name={editSpec}
        onClose={() => setEditSpec(null)}
        onSaved={refresh}
      />
      <ZoneEditor
        target={zoneEdit}
        onClose={() => setZoneEdit(null)}
        onSaved={refresh}
      />
    </>
  );
}
