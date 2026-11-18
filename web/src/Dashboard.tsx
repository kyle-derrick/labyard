import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Row,
  Space,
  Spin,
  Statistic,
  Table,
  Tag,
  Typography,
} from "antd";
import {
  CheckCircleFilled,
  CloseCircleFilled,
  ReloadOutlined,
} from "@ant-design/icons";
import { api, type DoctorItem, type StepState } from "./api";

const { Text, Paragraph } = Typography;

const STATE_COLOR: Record<string, string> = {
  done: "green",
  failed: "red",
  running: "blue",
};

export default function Dashboard() {
  const doctor = useQuery({
    queryKey: ["doctor"],
    queryFn: api.doctor,
    staleTime: 30_000,
  });

  const state = useQuery({
    queryKey: ["state"],
    queryFn: api.state,
    // 状态页 10 秒轮询；页面隐藏时 TanStack 会自动暂停
    refetchInterval: 10_000,
  });

  const steps: StepState[] = state.data?.steps ?? [];
  const byState = steps.reduce<Record<string, number>>((acc, s) => {
    acc[s.state] = (acc[s.state] ?? 0) + 1;
    return acc;
  }, {});

  const items: DoctorItem[] = doctor.data?.items ?? [];
  const failed = items.filter((i) => !i.ok);

  return (
    <Space direction="vertical" size={16} style={{ width: "100%" }}>
      {failed.length > 0 && (
        <Alert
          type="warning"
          showIcon
          message={`${failed.length} 项检查未通过`}
          description={
            <span className="mono">
              {failed.map((f) => `${f.name}: ${f.detail}`).join("；")}
            </span>
          }
        />
      )}

      <Row gutter={16}>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic
              title="已就绪步骤"
              value={byState["done"] ?? 0}
              valueStyle={{ color: "#16a34a" }}
            />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic
              title="失败步骤"
              value={byState["failed"] ?? 0}
              valueStyle={{ color: (byState["failed"] ?? 0) > 0 ? "#dc2626" : undefined }}
            />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="环境检查通过" value={items.length - failed.length} suffix={`/ ${items.length}`} />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic
              title="PVE"
              value={items.find((i) => i.name === "PVE 版本")?.ok ? "9.x" : "未知"}
              valueStyle={{ fontSize: 20 }}
            />
          </Card>
        </Col>
      </Row>

      <Row gutter={16}>
        <Col xs={24} lg={12}>
          <Card
            title="环境自检"
            size="small"
            extra={
              <Button
                size="small"
                icon={<ReloadOutlined />}
                onClick={() => doctor.refetch()}
                loading={doctor.isFetching}
              >
                重新检查
              </Button>
            }
          >
            {doctor.isLoading ? (
              <Spin />
            ) : (
              <Table<DoctorItem>
                size="small"
                pagination={false}
                rowKey="name"
                dataSource={items}
                columns={[
                  {
                    title: "",
                    dataIndex: "ok",
                    width: 36,
                    render: (ok: boolean) =>
                      ok ? (
                        <CheckCircleFilled style={{ color: "#16a34a" }} />
                      ) : (
                        <CloseCircleFilled style={{ color: "#dc2626" }} />
                      ),
                  },
                  { title: "检查项", dataIndex: "name", width: 140 },
                  {
                    title: "详情",
                    dataIndex: "detail",
                    render: (d: string) => <span className="mono">{d}</span>,
                  },
                ]}
              />
            )}
          </Card>
        </Col>

        <Col xs={24} lg={12}>
          <Card
            title="步骤状态"
            size="small"
            extra={
              <Text type="secondary" style={{ fontSize: 12 }}>
                每 10 秒刷新
              </Text>
            }
          >
            {steps.length === 0 ? (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description="还没有执行过任何规格"
              />
            ) : (
              <div style={{ maxHeight: 420, overflow: "auto" }}>
                {steps.map((s) => (
                  <div className="step-row" key={s.key}>
                    <Tag color={STATE_COLOR[s.state] ?? "default"}>{s.state}</Tag>
                    <span className="step-key">{s.key}</span>
                    {s.message && (
                      <Text type="danger" style={{ fontSize: 11 }}>
                        {s.message.slice(0, 40)}
                      </Text>
                    )}
                  </div>
                ))}
              </div>
            )}
          </Card>
        </Col>
      </Row>

      <Paragraph type="secondary" style={{ fontSize: 12 }}>
        「已就绪」表示上次执行成功且输入没有变化。任何参数改动都会让对应步骤回到待执行，
        不需要手工清理状态。
      </Paragraph>
    </Space>
  );
}
