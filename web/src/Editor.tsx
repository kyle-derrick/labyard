import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Col,
  Drawer,
  Empty,
  Form,
  Input,
  Modal,
  Popconfirm,
  Progress,
  Row,
  Segmented,
  Select,
  Space,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import type { ColumnsType } from "antd/es/table";
import {
  CaretRightOutlined,
  CheckCircleOutlined,
  CloseCircleOutlined,
  CopyOutlined,
  EyeOutlined,
  FileAddOutlined,
  StopOutlined,
  ReloadOutlined,
  SaveOutlined,
  UnorderedListOutlined,
} from "@ant-design/icons";
import { api, streamRun, type PreviewStep, type RunEvent, type SpecMeta } from "./api";
import Topology, { type Zone } from "./Topology";
import SpecVisual from "./SpecVisual";

const { Text } = Typography;

const SAMPLE = `defaults:
  cores: 2
  memory: 1024
  disk: 8
  template: debian-13-standard
  storage: local-lvm

zones:
  main:
    uplink: wan
    v4: 10.10.1.0/24
    v6: auto
    # 网关策略：出网开关 + 端口映射 + VPN（全在网关容器里用 nft / wg 落地）
    gateway:
      egress: true
      pub:
        - { proto: tcp, port: 2222, to: "10.10.1.20:22" }
      vpn: { wireguard: true, clients: [alice] }
    # 流量监控：本网络每台主机的接口流量复制到一台监控机
    monitor: { name: probe, mgmt: wan }
    hosts:
      gateway:   { role: gateway }
      web1:      { role: host }
      host2:     { role: host }

  # 嵌套：sub 的上联是 main 的网关
  sub:
    uplink: main
    v4: 10.20.1.0/24
    # 复用 main 里新建的那台监控机
    monitor: { use: "main:probe" }
    hosts:
      gateway: { role: gateway }
      win1:    { role: host }

# 精确连线：某台主机的流量复制到某台监控机（可跨网络）
mirrors:
  - { from: "sub:win1", to: "main:probe" }
`;

type LogLine = { cls: string; text: string; key?: string };

/** 毫秒 → 1m23s / 45s，执行进度里显示用时 */
function fmtDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  const m = Math.floor(s / 60);
  return m > 0 ? `${m}m${String(s % 60).padStart(2, "0")}s` : `${s}s`;
}

/** 规格文件管理（原独立「规格」页的核心部分，并进编排页的抽屉）。
 *
 * 文件级 CRUD：新建（可复制现有规格做变体）、删除、切换。zone 级编辑
 * 由可视化编排承担——同一份规格在两处打开只会互相覆盖，没必要。 */
function SpecManager({
  open,
  onClose,
  current,
  onPick,
  specs,
  onChanged,
}: {
  open: boolean;
  onClose: () => void;
  current: string;
  onPick: (name: string) => void;
  specs: SpecMeta[];
  onChanged: () => void;
}) {
  const { message } = AntApp.useApp();
  const [busy, setBusy] = useState(false);
  const [newOpen, setNewOpen] = useState(false);
  const [form] = Form.useForm();

  const create = async (v: { name: string; copy_of?: string }) => {
    setBusy(true);
    try {
      await api.createSpec({
        name: v.name,
        copy_of: v.copy_of || undefined,
      });
      message.success(`已创建 ${v.name}.yaml`);
      setNewOpen(false);
      form.resetFields();
      onChanged();
      onPick(v.name);
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const del = async (name: string) => {
    try {
      await api.deleteSpec(name);
      message.success(`已删除 ${name}.yaml`);
      onChanged();
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  const columns: ColumnsType<SpecMeta> = [
    {
      title: "规格",
      dataIndex: "name",
      render: (v: string, r) => (
        <Space size={6}>
          <a onClick={() => { onPick(v); onClose(); }}>
            <Text strong={v === current}>{v}.yaml</Text>
          </a>
          {v === current && <Tag color="blue" style={{ marginInlineEnd: 0 }}>当前</Tag>}
          {!r.valid && (
            <Tooltip title={r.error ?? "解析失败"}>
              <Tag color="red" style={{ marginInlineEnd: 0 }}>解析失败</Tag>
            </Tooltip>
          )}
        </Space>
      ),
      sorter: (a, b) => a.name.localeCompare(b.name),
    },
    { title: "网络", dataIndex: "zones", width: 70, sorter: (a, b) => a.zones - b.zones },
    { title: "主机", dataIndex: "hosts", width: 70, sorter: (a, b) => a.hosts - b.hosts },
    {
      title: "修改时间",
      dataIndex: "modified",
      width: 160,
      render: (v: string | null) => (
        <Text type="secondary" style={{ fontSize: 12 }}>
          {v ?? "—"}
        </Text>
      ),
      sorter: (a, b) => (a.modified ?? "").localeCompare(b.modified ?? ""),
    },
    {
      title: "操作",
      width: 150,
      render: (_: unknown, r) => (
        <Space size={4}>
          <Tooltip title="复制一份做变体">
            <Button
              size="small"
              icon={<CopyOutlined />}
              onClick={() => {
                setNewOpen(true);
                form.setFieldsValue({ copy_of: r.name });
              }}
            />
          </Tooltip>
          <Popconfirm
            title={`删除 ${r.name}.yaml？`}
            description="只删规格文件，不影响已经在跑的容器。"
            okText="删除"
            okButtonProps={{ danger: true }}
            disabled={r.name === current}
            onConfirm={() => del(r.name)}
          >
            <Button size="small" danger type="text" disabled={r.name === current} icon={<CloseCircleOutlined />} />
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <Drawer
      title="规格文件"
      width={640}
      open={open}
      onClose={onClose}
      extra={
        <Button type="primary" size="small" icon={<FileAddOutlined />} onClick={() => setNewOpen(true)}>
          新建
        </Button>
      }
    >
      <Table
        rowKey="name"
        size="small"
        dataSource={specs}
        columns={columns}
        pagination={false}
        loading={busy}
        locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有规格文件" /> }}
      />
      <Modal
        title="新建规格"
        open={newOpen}
        onCancel={() => setNewOpen(false)}
        onOk={() => form.submit()}
        confirmLoading={busy}
        okText="创建"
        width={420}
        destroyOnClose
      >
        <Form form={form} layout="vertical" onFinish={create} style={{ marginTop: 12 }}>
          <Form.Item
            name="name"
            label="规格名"
            rules={[
              { required: true, message: "请输入名字" },
              { pattern: /^[a-z][a-z0-9-]*$/, message: "小写字母开头，只含小写字母数字与 -" },
            ]}
          >
            <Input placeholder="如 lab2" />
          </Form.Item>
          <Form.Item
            name="copy_of"
            label="从现有规格复制（可选）"
            extra="选一份现有规格作为起点，改改就能用"
          >
            <Select
              allowClear
              placeholder="空白规格"
              options={specs.map((s) => ({ value: s.name, label: `${s.name}.yaml` }))}
            />
          </Form.Item>
        </Form>
      </Modal>
    </Drawer>
  );
}


const PHASE_ORDER = [
  "bridge",
  "template",
  "container",
  "net",
  "bootstrap",
  "gateway",
  "mirror",
  "snapshot",
];

export default function Editor() {
  const { message } = AntApp.useApp();
  const [specName, setSpecName] = useState("lab");
  // 默认可视化：改结构不该先学 YAML。YAML 是"高级选项"，不是主路径。
  const [mode, setMode] = useState<"visual" | "yaml">("visual");
  const [yaml, setYaml] = useState(SAMPLE);
  const [steps, setSteps] = useState<PreviewStep[]>([]);
  const [zones, setZones] = useState<unknown[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [runId, setRunId] = useState<string | null>(null);
  const [logs, setLogs] = useState<LogLine[]>([]);
  /** 进度：后端每完成一步推一次 {done,total} */
  const [prog, setProg] = useState<{ done: number; total: number } | null>(null);
  /** 当前正在跑的步骤 */
  const [cur, setCur] = useState<string | null>(null);
  /** 本次运行的计数与失败清单，结束后给结论 */
  const [tally, setTally] = useState<{ ok: number; skip: number; fail: number; errors: string[] }>({
    ok: 0,
    skip: 0,
    fail: 0,
    errors: [],
  });
  /** 日志过滤：all=全部，error=只看失败/错误 */
  const [logFilter, setLogFilter] = useState<"all" | "error">("all");
  const [startedAt, setStartedAt] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const [snapshot, setSnapshot] = useState(false);
  // 服务端保存的那一份。null = 从未保存过（此时以内置样例为基准判断是否改动）
  const [saved, setSaved] = useState<string | null>(null);
  const [specMgrOpen, setSpecMgrOpen] = useState(false);
  const stopRef = useRef<null | (() => void)>(null);
  const pollRef = useRef<number | null>(null);
  const logRef = useRef<HTMLDivElement>(null);

  const dirty = yaml !== (saved ?? SAMPLE);

  // 规格下拉：让"编排"页也能切文件，不必回规格页
  const specs = useQuery({ queryKey: ["specs"], queryFn: api.specs });
  const names = (specs.data?.specs ?? []).map((s) => s.name);

  // 切换规格时把那份读回来。读不到（还没建）就保持当前文本，保存时才创建。
  useEffect(() => {
    let alive = true;
    api
      .specDetail(specName)
      .then((r) => {
        if (!alive) return;
        if (r.exists && r.yaml != null) {
          setYaml(r.yaml);
          setSaved(r.yaml);
        } else {
          setSaved(null);
        }
        setErr(null);
      })
      .catch((e) => {
        // 读取失败要露出来——静默吞掉会把"未登录/服务不可达"伪装成"文件没建过"。
        if (alive) setErr(`读取 ${specName}.yaml 失败：${(e as Error).message}`);
      });
    return () => {
      alive = false;
    };
  }, [specName]);

  const onSave = async () => {
    setErr(null);
    try {
      await api.putSpecNamed(specName, yaml);
      setSaved(yaml);
      message.success(`已保存到 ${specName}.yaml`);
    } catch (e) {
      setErr((e as Error).message);
    }
  };

  const pushLog = (l: LogLine) => {
    setLogs((prev) => {
      // 日志上限 3000 行：超出丢最老的，避免浏览器内存被拖垮
      const next = prev.length > 3000 ? prev.slice(-2000) : prev;
      return [...next, l];
    });
    queueMicrotask(() => {
      const el = logRef.current;
      if (el) el.scrollTop = el.scrollHeight;
    });
  };

  /** 预览/执行前若还有未保存的改动，先落盘——所见即所跑。
   *  可视化模式下用户经常忘记"先保存"，这里替他做掉。 */
  const ensureSaved = async () => {
    if (!dirty) return;
    await api.putSpecNamed(specName, yaml);
    setSaved(yaml);
    message.info(`已自动保存 ${specName}.yaml`);
  };

  const onPreview = async () => {
    setErr(null);
    setBusy(true);
    try {
      await ensureSaved();
      const r = await api.preview(yaml, snapshot);
      setSteps(r.steps);
      setZones(r.zones);
      const skip = r.steps.filter((s) => s.action === "skip").length;
      const create = r.steps.filter((s) => s.action === "create").length;
      message.success(
        `计划 ${r.steps.length} 步：新建 ${create} · 更新 ${r.steps.length - skip - create} · 跳过 ${skip}`,
      );
    } catch (e) {
      setErr((e as Error).message);
      setSteps([]);
    } finally {
      setBusy(false);
    }
  };

  const onApply = async () => {
    setErr(null);
    setBusy(true);
    setLogs([]);
    setProg(null);
    setCur(null);
    setTally({ ok: 0, skip: 0, fail: 0, errors: [] });
    setStartedAt(Date.now());
    setNow(Date.now());
    // 记下这一刻提交的文本：执行期间用户可能继续改编辑器，
    // 自动保存要存的是"跑的那一份"，不是改过的新文本。
    const sent = yaml;
    try {
      await ensureSaved();
      const r = await api.apply(sent, snapshot);
      setRunId(r.run_id);
      message.info(`已启动 ${r.run_id}`);

      // 跑成功就把这份规格落盘：下次刷新页面能拿回来。
      // 失败只提示不打断——apply 已经开始了，保存不成功不该影响它。
      api
        .putSpecNamed(specName, sent)
        .then(() => setSaved(sent))
        .catch((e) => message.warning(`规格未能保存：${(e as Error).message}`));

      const stop = streamRun(
        r.run_id,
        (ev: RunEvent) => {
          if (ev.kind === "line") {
            pushLog({
              cls: ev.stream === "stderr" ? "ln-stderr" : "",
              text: ev.line,
            });
          } else if (ev.kind === "skipped") {
            pushLog({ cls: "ln-skip", text: `[已省略 ${ev.lines} 行]` });
          } else if (ev.kind === "upstream_skipped") {
            pushLog({ cls: "ln-skip", text: `⊘ ${ev.key}: ${ev.reason}` });
          } else if (ev.kind === "failed") {
            pushLog({ cls: "ln-fail", text: `✗ ${ev.key}: ${ev.error}`, key: ev.key });
            setTally((t) => ({ ...t, fail: t.fail + 1, errors: [...t.errors, `${ev.key}: ${ev.error}`] }));
          } else if (ev.kind === "started") {
            setCur(ev.key);
            pushLog({ cls: "ln-skip", text: `▶ ${ev.command}`, key: ev.key });
          } else if (ev.kind === "progress") {
            setProg({ done: ev.done, total: ev.total });
          } else if (ev.kind === "finished" && !ev.key.startsWith("run:")) {
            pushLog({
              cls: ev.code === 0 ? "ln-ok" : "ln-fail",
              text: `■ ${ev.key} 退出码 ${ev.code}（${ev.duration_ms} ms）`,
              key: ev.key,
            });
            setTally((t) => ({ ...t, ok: t.ok + (ev.code === 0 ? 1 : 0) }));
          }
        },
        () => {
          setBusy(false);
          stopRef.current = null;
          setCur(null);
        },
      );
      stopRef.current = stop;

      // 兜底：SSE 断流、切页回来、或浏览器把流掐了的时候，靠轮询 run 状态收尾。
      // 没有这条，"进行中"会一直挂着，用户永远等不到结论。
      if (pollRef.current) window.clearInterval(pollRef.current);
      pollRef.current = window.setInterval(async () => {
        try {
          const list = await api.runs();
          const row = list.runs.find((x) => x.id === r.run_id);
          if (row && row.status !== "running") {
            if (pollRef.current) window.clearInterval(pollRef.current);
            pollRef.current = null;
            stopRef.current?.();
            stopRef.current = null;
            setBusy(false);
            setCur(null);
          }
        } catch {
          /* 网络抖动忽略，下一轮再试 */
        }
      }, 5000);
    } catch (e) {
      setErr((e as Error).message);
      setBusy(false);
    }
  };

  const onCancel = async () => {
    if (!runId) return;
    try {
      await api.cancel(runId);
      message.warning("已请求取消");
    } catch (e) {
      message.error((e as Error).message);
    }
  };

  // 秒表：跑的时候每秒刷一次"已用时"
  useEffect(() => {
    if (!busy) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [busy]);

  // 卸载时收干净：轮询定时器与 SSE 连接都不该留在后台
  useEffect(
    () => () => {
      if (pollRef.current) window.clearInterval(pollRef.current);
      stopRef.current?.();
    },
    [],
  );

  const grouped = useMemo(() => {    const g = new Map<string, PreviewStep[]>();
    for (const s of steps) {
      const arr = g.get(s.phase) ?? [];
      arr.push(s);
      g.set(s.phase, arr);
    }
    return PHASE_ORDER.filter((p) => g.has(p)).map((p) => [p, g.get(p)!] as const);
  }, [steps]);

  return (
    <Row gutter={[16, 16]}>
      <Col xs={24} xl={13}>
        <Card
          title={
            <Space size={8}>
              <Select
                size="small"
                style={{ width: 132 }}
                value={specName}
                onChange={setSpecName}
                options={
                  names.includes(specName)
                    ? names.map((n) => ({ value: n, label: `${n}.yaml` }))
                    : [...names.map((n) => ({ value: n, label: `${n}.yaml` })), {
                        value: specName,
                        label: `${specName}.yaml（未建）`,
                      }]
                }
              />
              <Segmented
                size="small"
                value={mode}
                onChange={(v) => setMode(v as "visual" | "yaml")}
                options={[
                  { value: "visual", label: "可视化" },
                  { value: "yaml", label: "YAML" },
                ]}
              />
              {dirty ? (
                <Tag color="orange" style={{ marginInlineEnd: 0 }}>
                  未保存
                </Tag>
              ) : saved !== null ? (
                <Tag color="green" style={{ marginInlineEnd: 0 }}>
                  已保存
                </Tag>
              ) : null}
            </Space>
          }
          size="small"
          extra={
            <Space>
              <Button
                size="small"
                icon={<UnorderedListOutlined />}
                onClick={() => setSpecMgrOpen(true)}
              >
                规格管理
              </Button>
              <Tooltip title="执行后为每台机器打快照">
                <Space size={4}>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    快照
                  </Text>
                  <Switch size="small" checked={snapshot} onChange={setSnapshot} />
                </Space>
              </Tooltip>
              {mode === "yaml" && (
                <Tooltip title="恢复到内置样例（不覆盖已保存的那份）">
                  <Button
                    size="small"
                    icon={<ReloadOutlined />}
                    onClick={() => setYaml(SAMPLE)}
                  >
                    重置
                  </Button>
                </Tooltip>
              )}
            </Space>
          }
        >
          {mode === "yaml" ? (
            <Input.TextArea
              value={yaml}
              onChange={(e) => setYaml(e.target.value)}
              autoSize={{ minRows: 22, maxRows: 32 }}
              style={{ fontFamily: "var(--mono)", fontSize: 12 }}
              spellCheck={false}
            />
          ) : (
            <SpecVisual
              name={specName}
              onSaved={(y) => {
                setYaml(y);
                setSaved(y);
                setErr(null);
              }}
            />
          )}
          {mode === "visual" && (
            <Text type="secondary" style={{ fontSize: 12, display: "block", marginTop: 10 }}>
              预览或执行时会自动保存当前改动——所见即所跑。
            </Text>
          )}
          <Space style={{ marginTop: 12 }} wrap>
            <Button
              icon={<EyeOutlined />}
              onClick={onPreview}
              loading={busy && !runId}
            >
              预览计划
            </Button>
            <Button
              type="primary"
              icon={<CaretRightOutlined />}
              onClick={onApply}
              loading={busy && !!runId}
            >
              执行
            </Button>
            <Button
              danger
              icon={<StopOutlined />}
              onClick={onCancel}
              disabled={!runId || !busy}
            >
              取消
            </Button>
            <Tooltip title="保存到服务端，刷新页面后自动读回">
              <Button icon={<SaveOutlined />} onClick={onSave} disabled={!dirty}>
                保存
              </Button>
            </Tooltip>
          </Space>

          {err && (
            <Alert
              type="error"
              showIcon
              message="解析或执行失败"
              description={<span className="mono">{err}</span>}
              style={{ marginTop: 12 }}
            />
          )}
        </Card>

        {steps.length > 0 && (
          <Card title={`计划（${steps.length} 步）`} size="small" style={{ marginTop: 16 }}>
            {grouped.map(([phase, list]) => (
              <div key={phase} style={{ marginBottom: 10 }}>
                <Tag color="blue" style={{ marginBottom: 6 }}>
                  {phase}
                </Tag>
                {list.map((s) => (
                  <div className="step-row" key={s.key}>
                    <span className="step-key">{s.key}</span>
                    <Space size={6}>
                      {s.action === "create" && (
                        <Tag color="green" style={{ marginInlineEnd: 0 }}>
                          新建
                        </Tag>
                      )}
                      {s.action === "update" && (
                        <Tooltip title={s.reason}>
                          <Tag color="orange" style={{ marginInlineEnd: 0 }}>
                            更新
                          </Tag>
                        </Tooltip>
                      )}
                      {s.action === "skip" && (
                        <Tag style={{ marginInlineEnd: 0 }}>跳过</Tag>
                      )}
                      <Text type="secondary" className="mono">
                        {s.input_hash}
                      </Text>
                    </Space>
                  </div>
                ))}
              </div>
            ))}
          </Card>
        )}
      </Col>

      <Col xs={24} xl={11}>
        {zones.length > 0 && (
          <Card title="拓扑预览" size="small" style={{ marginBottom: 16 }}>
            <Topology zones={zones as Zone[]} />
          </Card>
        )}

        <Card
          title={
            <Space size={8}>
              <span>执行</span>
              {busy && <Tag color="processing">进行中</Tag>}
              {!busy && prog && prog.done > 0 && (
                <Tag color={tally.fail > 0 ? "red" : "green"}>
                  {tally.fail > 0 ? "有失败" : "完成"}
                </Tag>
              )}
            </Space>
          }
          size="small"
          extra={
            runId ? (
              <Text type="secondary" className="mono" style={{ fontSize: 12 }}>
                {runId}
              </Text>
            ) : (
              <Text type="secondary" style={{ fontSize: 12 }}>
                尚未执行
              </Text>
            )
          }
        >
          {(prog || busy) && (
            <div style={{ marginBottom: 10 }}>
              <Progress
                percent={prog && prog.total > 0 ? Math.round((prog.done / prog.total) * 100) : 0}
                size="small"
                status={busy ? "active" : tally.fail > 0 ? "exception" : "success"}
                format={() =>
                  prog ? `${prog.done}/${prog.total} 步` : busy ? "排队中…" : ""
                }
              />
              <Space size={14} wrap style={{ fontSize: 12, marginTop: 4 }}>
                {busy && cur && (
                  <Text>
                    正在执行 <code>{cur}</code>
                  </Text>
                )}
                <Text type="secondary">
                  成功 {tally.ok} · 失败 {tally.fail}
                </Text>
                {startedAt && (
                  <Text type="secondary">
                    {busy ? "已用时" : "总用时"} {fmtDuration(now - startedAt)}
                  </Text>
                )}
              </Space>
            </div>
          )}

          {!busy && tally.errors.length > 0 && (
            <Alert
              type="error"
              showIcon
              style={{ marginBottom: 10 }}
              message={`${tally.errors.length} 个步骤失败`}
              description={
                <div className="mono" style={{ fontSize: 12, maxHeight: 160, overflow: "auto" }}>
                  {tally.errors.map((e, i) => (
                    <div key={i}>✗ {e}</div>
                  ))}
                </div>
              }
            />
          )}

          <div className="log-view" ref={logRef}>
            {logs.length === 0 ? (
              <span style={{ color: "#5b6b84" }}>
                点击「执行」后，每一步的输出会实时出现在这里。
                {"\n"}已完成且参数未变的步骤会被跳过，并标注原因。
              </span>
            ) : (
              (logFilter === "error"
                ? logs.filter((l) => l.cls.includes("fail") || l.cls === "ln-stderr")
                : logs
              ).map((l, i) => (
                <div key={i} className={l.cls}>
                  {l.text}
                </div>
              ))
            )}
          </div>
          <Space style={{ marginTop: 8 }} size={12} wrap>
            <Segmented
              size="small"
              value={logFilter}
              onChange={(v) => setLogFilter(v as "all" | "error")}
              options={[
                { value: "all", label: "全部" },
                { value: "error", label: "只看错误" },
              ]}
            />
            {runId && (
              <Text type="secondary" style={{ fontSize: 12 }}>
                <CheckCircleOutlined /> 日志超 3000 行丢弃较早部分；瞬时超 500 行会丢中间行并标注
              </Text>
            )}
          </Space>
        </Card>
      </Col>

      <SpecManager
        open={specMgrOpen}
        onClose={() => setSpecMgrOpen(false)}
        current={specName}
        onPick={setSpecName}
        specs={specs.data?.specs ?? []}
        onChanged={() => specs.refetch()}
      />
    </Row>
  );
}
