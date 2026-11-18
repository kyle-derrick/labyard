/** 拓扑页：全局图 + 节点详情 + 镜像连线管理。
 *
 * 图本身是只读的，改结构要去「编排」（改 YAML）或用这里的镜像表单
 * —— 后者只是往规格里加一条 `mirrors:`，本质仍是改 YAML，只是不用手写。
 */
import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  App as AntApp,
  Button,
  Card,
  Descriptions,
  Empty,
  Form,
  Popconfirm,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import {
  ArrowRightOutlined,
  DeleteOutlined,
  PlusOutlined,
  ReloadOutlined,
  SwapOutlined,
} from "@ant-design/icons";
import TopologyGraph from "./TopologyGraph";
import { api, type MirrorRow, type TopoNode } from "./api";

const { Text } = Typography;

function statusTag(n: TopoNode) {
  if (n.retired) return <Tag color="orange">已废弃</Tag>;
  if (n.kind !== "host") return null;
  if (n.status === "running") return <Tag color="green">运行中</Tag>;
  if (n.status === "stopped") return <Tag>已停止</Tag>;
  return <Tag color="red">不存在</Tag>;
}

export default function TopologyPage() {
  const { message } = AntApp.useApp();
  const qc = useQueryClient();
  const [spec, setSpec] = useState<string | undefined>(undefined);
  const [picked, setPicked] = useState<TopoNode | null>(null);
  const [busy, setBusy] = useState(false);
  const [form] = Form.useForm();

  const specs = useQuery({ queryKey: ["specs"], queryFn: api.specs });
  const topo = useQuery({
    queryKey: ["topology", spec ?? "all"],
    queryFn: () => api.topology(spec),
    refetchInterval: 15000,
  });
  const mirrors = useQuery({
    queryKey: ["mirrors", spec ?? "all"],
    queryFn: () => api.mirrors(spec),
  });

  const nodes = topo.data?.nodes ?? [];
  const edges = topo.data?.edges ?? [];
  const rows = mirrors.data?.mirrors ?? [];

  // 主机下拉：源可以是任何非分析机，目标是分析机（后端也会校验）
  const hostOpts = useMemo(() => {
    const hosts = nodes.filter((n) => n.kind === "host");
    return {
      from: hosts
        .filter((n) => n.role !== "analysis")
        .map((n) => ({ value: `${n.zone}:${n.label}`, label: `${n.zone} / ${n.label}` })),
      to: hosts
        .filter((n) => n.role === "analysis")
        .map((n) => ({ value: `${n.zone}:${n.label}`, label: `${n.zone} / ${n.label}` })),
    };
  }, [nodes]);

  const addMirror = async (v: { from: string; to: string }) => {
    setBusy(true);
    try {
      const r = await api.addMirror(v.from, v.to, spec);
      message.success(
        r.run_id ? `已写入规格 ${r.spec}，镜像实例正在装载` : `已写入规格 ${r.spec}`,
      );
      form.resetFields();
      // 实例装载是异步的（起了一个只含 mirror 阶段的 run），稍等一下再刷
      setTimeout(() => {
        topo.refetch();
        mirrors.refetch();
      }, 1500);
      qc.invalidateQueries({ queryKey: ["hosts"] });
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const delMirror = async (m: MirrorRow) => {
    setBusy(true);
    try {
      await api.delMirror(`${m.from}->${m.to}`);
      message.success("已删除，镜像实例已停");
      await Promise.all([topo.refetch(), mirrors.refetch()]);
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const detail = picked;

  return (
    <Space direction="vertical" size={16} style={{ width: "100%" }}>
      <Card
        size="small"
        title={
          <Space size={8}>
            <span>全局拓扑</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              {nodes.length} 节点 / {edges.length} 条边
            </Text>
          </Space>
        }
        extra={
          <Space>
            <Select
              size="small"
              allowClear
              placeholder="全部规格"
              style={{ width: 180 }}
              value={spec}
              onChange={setSpec}
              options={(specs.data?.specs ?? []).map((s) => ({
                value: s.name,
                label: s.name,
              }))}
            />
            <Tooltip title="重新解析规格并读取容器状态">
              <Button
                size="small"
                icon={<ReloadOutlined />}
                loading={topo.isFetching}
                onClick={() => topo.refetch()}
              >
                刷新
              </Button>
            </Tooltip>
          </Space>
        }
      >
        {topo.isError ? (
          <Empty
            image={Empty.PRESENTED_IMAGE_SIMPLE}
            description={(topo.error as Error).message}
          />
        ) : nodes.length === 0 && !topo.isLoading ? (
          <Empty
            image={Empty.PRESENTED_IMAGE_SIMPLE}
            description="还没有规格——先去「规格」页建一份"
          />
        ) : (
          <TopologyGraph nodes={nodes} edges={edges} onPick={setPicked} />
        )}

        <div className="topo-legend">
          <span className="lg">
            <b style={{ background: "#eef2ff", borderColor: "#a5b4fc" }} />
            WAN
          </span>
          <span className="lg">
            <b style={{ background: "#f0f7ff", borderColor: "#93b8f7" }} />
            zone
          </span>
          <span className="lg">
            <b style={{ background: "#ffffff" }} />
            主机
          </span>
          <span className="lg">
            <b style={{ background: "#fffbeb", borderColor: "#fbbf24" }} />
            镜像桥
          </span>
          <span className="lg">
            <i />
            上联
          </span>
          <span className="lg">
            <i className="thin" />
            属于 zone
          </span>
          <span className="lg">
            <i className="dash" />
            流量镜像
          </span>
          <span className="lg">
            <b style={{ background: "#f8fafc", borderStyle: "dashed" }} />
            已废弃（停机、保留 IP 与磁盘）
          </span>
        </div>
      </Card>

      {detail && (
        <Card
          size="small"
          title={
            <Space>
              <span>{detail.label}</span>
              {statusTag(detail)}
              <Tag className="mono">{detail.id}</Tag>
            </Space>
          }
          extra={
            <Button size="small" onClick={() => setPicked(null)}>
              关闭
            </Button>
          }
        >
          <Descriptions size="small" column={3} bordered>
            <Descriptions.Item label="类型">{detail.kind}</Descriptions.Item>
            <Descriptions.Item label="Zone">{detail.zone ?? "—"}</Descriptions.Item>
            <Descriptions.Item label="角色">{detail.role ?? "—"}</Descriptions.Item>
            <Descriptions.Item label="CT">{detail.vmid ?? "—"}</Descriptions.Item>
            <Descriptions.Item label="IPv4">
              <span className="mono">{detail.ipv4 ?? "—"}</span>
            </Descriptions.Item>
            <Descriptions.Item label="状态">{detail.status ?? "—"}</Descriptions.Item>
          </Descriptions>
          <pre className="mono" style={{ marginTop: 10, marginBottom: 0, color: "#475569" }}>
            {JSON.stringify(detail.meta, null, 2)}
          </pre>
        </Card>
      )}

      <Card
        size="small"
        title={
          <Space size={8}>
            <SwapOutlined />
            <span>流量镜像</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              源机的全部二层流量复制到分析机的 mirr 桥
            </Text>
          </Space>
        }
      >
        <Table
          size="small"
          rowKey={(r) => `${r.spec}:${r.from}->${r.to}`}
          dataSource={rows}
          pagination={false}
          loading={mirrors.isLoading}
          locale={{
            emptyText: (
              <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有镜像连线" />
            ),
          }}
          columns={[
            {
              title: "规格",
              dataIndex: "spec",
              width: 120,
              render: (v: string) => <Tag className="mono">{v}</Tag>,
            },
            {
              title: "源",
              dataIndex: "from",
              render: (v: string) => <code>{v}</code>,
            },
            {
              title: "",
              width: 40,
              render: () => <ArrowRightOutlined style={{ color: "#d97706" }} />,
            },
            {
              title: "分析机",
              dataIndex: "to",
              render: (v: string) => <code>{v}</code>,
            },
            {
              title: "源网桥",
              dataIndex: "src_bridge",
              width: 140,
              render: (v: string) => <span className="mono">{v}</span>,
            },
            {
              title: "",
              width: 56,
              render: (_: unknown, r: MirrorRow) => (
                <Popconfirm
                  title="删除这条镜像？"
                  description="规格里会一并删掉，分析机上的镜像实例也会停。"
                  okText="删除"
                  okButtonProps={{ danger: true }}
                  onConfirm={() => delMirror(r)}
                >
                  <Button size="small" danger type="text" icon={<DeleteOutlined />} />
                </Popconfirm>
              ),
            },
          ]}
        />

        <Form
          form={form}
          layout="inline"
          style={{ marginTop: 12 }}
          onFinish={addMirror}
        >
          <Form.Item
            name="from"
            rules={[{ required: true, message: "选源主机" }]}
            style={{ marginBottom: 6 }}
          >
            <Select
              showSearch
              placeholder="源主机"
              style={{ width: 220 }}
              options={hostOpts.from}
            />
          </Form.Item>
          <Form.Item style={{ marginBottom: 6 }}>
            <ArrowRightOutlined style={{ color: "#94a3b8" }} />
          </Form.Item>
          <Form.Item
            name="to"
            rules={[{ required: true, message: "选分析机" }]}
            style={{ marginBottom: 6 }}
          >
            <Select
              showSearch
              placeholder="分析机（role: analysis）"
              style={{ width: 240 }}
              options={hostOpts.to}
            />
          </Form.Item>
          <Form.Item style={{ marginBottom: 6 }}>
            <Button type="primary" htmlType="submit" icon={<PlusOutlined />} loading={busy}>
              添加镜像
            </Button>
          </Form.Item>
        </Form>

        {hostOpts.to.length === 0 && (
          <Text type="secondary" style={{ fontSize: 12 }}>
            没有 role: analysis 的主机——先在规格里给某台机器写上
            <code> role: analysis </code>，它才有 mirr 桥可以接镜像。
          </Text>
        )}
      </Card>
    </Space>
  );
}
