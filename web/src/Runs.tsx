/** 运行历史：每次 apply 的结果、失败原因、一键重试。
 *
 * 重试的语义是"用当前规格重新对账"（引擎幂等：没变的跳过、变了的重跑），
 * 不是回滚也不是重建——界面上要写清楚，否则用户会以为是重放。
 */
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  App as AntApp,
  Button,
  Drawer,
  Empty,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import { HistoryOutlined, RedoOutlined, ReloadOutlined } from "@ant-design/icons";
import { api, type RunRow, type RunStepRow } from "./api";

const { Text } = Typography;

function runStatusTag(s: string) {
  switch (s) {
    case "ok":
      return <Tag color="green">成功</Tag>;
    case "failed":
      return <Tag color="red">失败</Tag>;
    case "cancelled":
      return <Tag color="orange">已取消</Tag>;
    case "interrupted":
      // 服务重启/进程被杀留下的：它不会自己再跑完，别显示成"进行中"
      return (
        <Tooltip title="执行进程已不在（服务重启或被杀）。点「重试」按当前规格续跑。">
          <Tag color="volcano">已中断</Tag>
        </Tooltip>
      );
    case "running":
      return <Tag color="blue">进行中</Tag>;
    default:
      return <Tag>{s}</Tag>;
  }
}

function stepStateTag(s: string) {
  switch (s) {
    case "done":
      return <Tag color="green">完成</Tag>;
    case "applied":
      return <Tag color="green">已执行</Tag>;
    case "skipped":
      return <Tag color="default">跳过</Tag>;
    case "failed":
      return <Tag color="red">失败</Tag>;
    default:
      return <Tag>{s}</Tag>;
  }
}

function fmt(ts: string | null): string {
  if (!ts) return "—";
  // 后端存的是 UTC ISO；本地化显示
  const d = new Date(ts.includes("T") ? ts : ts.replace(" ", "T") + "Z");
  if (isNaN(d.getTime())) return ts;
  return d.toLocaleString("zh-CN", { hour12: false });
}

export default function Runs() {
  const { message } = AntApp.useApp();
  const [detail, setDetail] = useState<string | null>(null);
  const q = useQuery({
    queryKey: ["runs"],
    queryFn: api.runs,
    refetchInterval: 10000,
  });
  const steps = useQuery({
    queryKey: ["run-steps", detail],
    queryFn: () => api.runSteps(detail!),
    enabled: !!detail,
  });

  const retry = async (id: string) => {
    try {
      const r = await api.retry(id);
      message.success(`已按当前规格重新对账（新 run ${r.run_id}）`);
      await q.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const columns: ColumnsType<RunRow> = [
    {
      title: "Run",
      dataIndex: "id",
      width: 200,
      render: (v: string) => <code style={{ fontSize: 12 }}>{v}</code>,
    },
    {
      title: "来源",
      dataIndex: "spec_name",
      width: 110,
      render: (v: string | null) => v ?? "—",
    },
    { title: "开始", dataIndex: "started_at", width: 180, render: fmt },
    { title: "结束", dataIndex: "finished_at", width: 180, render: fmt },
    {
      title: "结果",
      dataIndex: "status",
      width: 100,
      render: (v: string) => runStatusTag(v),
    },
    {
      title: "操作",
      render: (_: unknown, r: RunRow) => (
        <Space size={4}>
          <Button size="small" onClick={() => setDetail(r.id)}>
            明细
          </Button>
          <Tooltip title="用当前已保存的规格重新对账（幂等：没变的跳过）">
            <Button size="small" icon={<RedoOutlined />} onClick={() => retry(r.id)}>
              重试
            </Button>
          </Tooltip>
        </Space>
      ),
    },
  ];

  const stepCols: ColumnsType<RunStepRow> = [
    {
      title: "步骤",
      dataIndex: "step_key",
      render: (v: string) => <code style={{ fontSize: 12 }}>{v}</code>,
    },
    {
      title: "状态",
      dataIndex: "state",
      width: 90,
      render: (v: string) => stepStateTag(v),
    },
    {
      title: "耗时",
      width: 90,
      render: (_: unknown, r: RunStepRow) => {
        if (!r.started_at || !r.finished_at) return "—";
        const a = new Date(r.started_at.replace(" ", "T") + "Z").getTime();
        const b = new Date(r.finished_at.replace(" ", "T") + "Z").getTime();
        if (isNaN(a) || isNaN(b)) return "—";
        return `${Math.max(0, Math.round((b - a) / 1000))}s`;
      },
    },
    {
      title: "信息",
      dataIndex: "message",
      render: (v: string) =>
        v ? (
          <Text type="danger" style={{ fontFamily: "ui-monospace, monospace", fontSize: 12 }}>
            {v}
          </Text>
        ) : (
          <Text type="secondary">—</Text>
        ),
    },
  ];

  return (
    <>
      <Space style={{ marginBottom: 16 }}>
        <Button
          icon={<ReloadOutlined />}
          size="small"
          loading={q.isFetching}
          onClick={() => q.refetch()}
        >
          刷新
        </Button>
        <Text type="secondary" style={{ fontSize: 12 }}>
          保留最近 100 次 · 重试 = 用当前规格重新对账
        </Text>
      </Space>

      <Table
        rowKey={(r) => r.id}
        loading={q.isLoading}
        dataSource={q.data?.runs ?? []}
        columns={columns}
        size="small"
        pagination={{ pageSize: 15, size: "small" }}
        locale={{
          emptyText: (
            <Empty
              image={Empty.PRESENTED_IMAGE_SIMPLE}
              description={
                <span>
                  <HistoryOutlined /> 还没有运行记录
                </span>
              }
            />
          ),
        }}
      />

      <Drawer
        open={!!detail}
        onClose={() => setDetail(null)}
        width={760}
        title={<code>{detail}</code>}
      >
        <Table
          rowKey={(r) => r.step_key}
          loading={steps.isLoading}
          dataSource={steps.data?.steps ?? []}
          columns={stepCols}
          size="small"
          pagination={false}
        />
      </Drawer>
    </>
  );
}
