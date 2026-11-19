import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Col,
  Empty,
  Popconfirm,
  Row,
  Segmented,
  Space,
  Spin,
  Statistic,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import {
  CheckCircleFilled,
  CloseCircleFilled,
  ReloadOutlined,
} from "@ant-design/icons";
import { api, type DoctorItem, type StepState } from "./api";

const { Text, Paragraph } = Typography;

/** 步骤状态是给人看的，别直接摊英文 */
const STATE_META: Record<string, { label: string; color: string }> = {
  done: { label: "已完成", color: "green" },
  failed: { label: "失败", color: "red" },
  running: { label: "运行中", color: "blue" },
  skipped: { label: "已跳过", color: "default" },
  pending: { label: "待执行", color: "default" },
};

/** 从体检详情里抠出版本号（形如 `9.2.2（需要 ≥ 9.0）`） */
function pveVersion(items: DoctorItem[]): string {
  const it = items.find((i) => i.name === "PVE 版本");
  if (!it) return "未知";
  const m = it.detail.match(/\d+\.\d+(?:\.\d+)?/);
  return m ? m[0] : it.ok ? "已就绪" : "未知";
}

export default function Dashboard() {
  const { message } = AntApp.useApp();
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

  const [onlyFailed, setOnlyFailed] = useState(false);

  const steps: StepState[] = state.data?.steps ?? [];
  const failedSteps = useMemo(
    () => steps.filter((s) => s.state === "failed"),
    [steps],
  );
  const byState = steps.reduce<Record<string, number>>((acc, s) => {
    acc[s.state] = (acc[s.state] ?? 0) + 1;
    return acc;
  }, {});

  const items: DoctorItem[] = doctor.data?.items ?? [];
  const failed = items.filter((i) => !i.ok);
  const orphans = state.data?.orphans ?? 0;
  const visibleSteps = onlyFailed ? failedSteps : steps;

  const pruneOrphans = async () => {
    try {
      const r = await api.pruneState();
      message.success(
        r.removed > 0 ? `已清理 ${r.removed} 条历史记录` : "没有可清理的记录",
      );
      await state.refetch();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

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

      {orphans > 0 && (
        <Alert
          type="info"
          showIcon
          message={`库里有 ${orphans} 条记录属于已删除的规格`}
          description="它们不计入下面的统计，也不代表当前环境有问题。确认不再需要后可以清掉。"
          action={
            <Popconfirm
              title="清理历史记录？"
              description="只删「规格里已经没有」的步骤记录，不会碰现存环境。"
              okText="清理"
              cancelText="取消"
              onConfirm={pruneOrphans}
            >
              <Button size="small">清理</Button>
            </Popconfirm>
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
          <Tooltip
            title={
              failedSteps.length > 0
                ? "点击在右侧只看失败步骤"
                : "上次执行没有失败的步骤"
            }
          >
            <Card
              size="small"
              hoverable={failedSteps.length > 0}
              onClick={() => {
                if (failedSteps.length > 0) setOnlyFailed(true);
              }}
              style={
                failedSteps.length > 0
                  ? { cursor: "pointer", borderColor: "#fecaca" }
                  : undefined
              }
            >
              <Statistic
                title="失败步骤"
                value={byState["failed"] ?? 0}
                valueStyle={{
                  color: (byState["failed"] ?? 0) > 0 ? "#dc2626" : undefined,
                }}
                suffix={
                  failedSteps.length > 0 ? (
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      查看明细
                    </Text>
                  ) : undefined
                }
              />
            </Card>
          </Tooltip>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic title="环境检查通过" value={items.length - failed.length} suffix={`/ ${items.length}`} />
          </Card>
        </Col>
        <Col xs={12} md={6}>
          <Card size="small">
            <Statistic
              title="PVE 版本"
              value={pveVersion(items)}
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
              failedSteps.length > 0 ? (
                <Segmented
                  size="small"
                  value={onlyFailed ? "failed" : "all"}
                  onChange={(v) => setOnlyFailed(v === "failed")}
                  options={[
                    { label: `全部 ${steps.length}`, value: "all" },
                    { label: `只看失败 ${failedSteps.length}`, value: "failed" },
                  ]}
                />
              ) : (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  每 10 秒刷新
                </Text>
              )
            }
          >
            {steps.length === 0 ? (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description="还没有执行过任何规格"
              />
            ) : visibleSteps.length === 0 ? (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description="没有失败的步骤"
              />
            ) : (
              <div style={{ maxHeight: 420, overflow: "auto" }}>
                {visibleSteps.map((s) => {
                  const meta = STATE_META[s.state] ?? {
                    label: s.state,
                    color: "default",
                  };
                  return (
                    <div className="step-row" key={s.key}>
                      <Tag color={meta.color}>{meta.label}</Tag>
                      <span className="step-key">{s.key}</span>
                      {s.message && (
                        <Tooltip title={s.message}>
                          <Text type="danger" style={{ fontSize: 11 }}>
                            {s.message.slice(0, 40)}
                            {s.message.length > 40 ? "…" : ""}
                          </Text>
                        </Tooltip>
                      )}
                    </div>
                  );
                })}
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
