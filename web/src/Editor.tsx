import { useMemo, useRef, useState } from "react";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Col,
  Input,
  Row,
  Space,
  Switch,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import {
  CaretRightOutlined,
  CheckCircleOutlined,
  CloseCircleOutlined,
  EyeOutlined,
  StopOutlined,
  ReloadOutlined,
} from "@ant-design/icons";
import { api, streamRun, type PreviewStep, type RunEvent } from "./api";
import Topology, { type Zone } from "./Topology";

const { Text, Paragraph } = Typography;

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
    hosts:
      gateway:   { role: gateway }
      analysis:  { role: analysis, mirror: target, provisioner: apt-mirror }
      server1:   { role: server,   mirror: source }
      client1:   { role: client }

  # 嵌套：sub 的上联是 main 的网关
  sub:
    uplink: main
    v4: 10.20.1.0/24
    hosts:
      gateway: { role: gateway }
      client1: { role: client }
`;

type LogLine = { cls: string; text: string };

const PHASE_ORDER = [
  "bridge",
  "template",
  "container",
  "net",
  "bootstrap",
  "mirror",
  "snapshot",
];

export default function Editor() {
  const { message } = AntApp.useApp();
  const [yaml, setYaml] = useState(SAMPLE);
  const [steps, setSteps] = useState<PreviewStep[]>([]);
  const [zones, setZones] = useState<unknown[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [runId, setRunId] = useState<string | null>(null);
  const [logs, setLogs] = useState<LogLine[]>([]);
  const [snapshot, setSnapshot] = useState(false);
  const stopRef = useRef<null | (() => void)>(null);
  const logRef = useRef<HTMLDivElement>(null);

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

  const onPreview = async () => {
    setErr(null);
    setBusy(true);
    try {
      const r = await api.preview(yaml, snapshot);
      setSteps(r.steps);
      setZones(r.zones);
      message.success(`计划 ${r.steps.length} 步`);
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
    try {
      const r = await api.apply(yaml, snapshot);
      setRunId(r.run_id);
      message.info(`已启动 ${r.run_id}`);

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
          } else if (ev.kind === "failed") {
            pushLog({ cls: "ln-fail", text: `✗ ${ev.key}: ${ev.error}` });
          } else if (ev.kind === "started") {
            pushLog({ cls: "ln-skip", text: `▶ ${ev.command}` });
          } else if (ev.kind === "finished" && !ev.key.startsWith("run:")) {
            pushLog({
              cls: ev.code === 0 ? "ln-ok" : "ln-fail",
              text: `■ ${ev.key} 退出码 ${ev.code}（${ev.duration_ms} ms）`,
            });
          }
        },
        () => {
          setBusy(false);
          stopRef.current = null;
          message.success("执行结束");
        },
      );
      stopRef.current = stop;
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

  const grouped = useMemo(() => {
    const g = new Map<string, PreviewStep[]>();
    for (const s of steps) {
      const arr = g.get(s.phase) ?? [];
      arr.push(s);
      g.set(s.phase, arr);
    }
    return PHASE_ORDER.filter((p) => g.has(p)).map((p) => [p, g.get(p)!] as const);
  }, [steps]);

  return (
    <Row gutter={[16, 16]}>
      <Col xs={24} xl={10}>
        <Card
          title="规格（lab.yaml）"
          size="small"
          extra={
            <Space>
              <Tooltip title="执行后为每台机器打快照">
                <Space size={4}>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    快照
                  </Text>
                  <Switch size="small" checked={snapshot} onChange={setSnapshot} />
                </Space>
              </Tooltip>
              <Button
                size="small"
                icon={<ReloadOutlined />}
                onClick={() => setYaml(SAMPLE)}
              >
                重置
              </Button>
            </Space>
          }
        >
          <Input.TextArea
            value={yaml}
            onChange={(e) => setYaml(e.target.value)}
            autoSize={{ minRows: 22, maxRows: 32 }}
            style={{ fontFamily: "var(--mono)", fontSize: 12 }}
            spellCheck={false}
          />
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
                    <Text type="secondary" className="mono">
                      {s.input_hash}
                    </Text>
                  </div>
                ))}
              </div>
            ))}
          </Card>
        )}
      </Col>

      <Col xs={24} xl={14}>
        {zones.length > 0 && (
          <Card title="拓扑预览" size="small" style={{ marginBottom: 16 }}>
            <Topology zones={zones as Zone[]} />
          </Card>
        )}

        <Card
          title="执行日志"
          size="small"
          extra={
            runId ? (
              <Text type="secondary" className="mono">
                {runId}
              </Text>
            ) : (
              <Text type="secondary" style={{ fontSize: 12 }}>
                尚未执行
              </Text>
            )
          }
        >
          <div className="log-view" ref={logRef}>
            {logs.length === 0 ? (
              <span style={{ color: "#5b6b84" }}>
                点击「执行」后，每一步的输出会实时出现在这里。
                {"\n"}已完成且参数未变的步骤会被跳过，并标注原因。
              </span>
            ) : (
              logs.map((l, i) => (
                <div key={i} className={l.cls}>
                  {l.text}
                </div>
              ))
            )}
          </div>
          {runId && (
            <Paragraph type="secondary" style={{ marginTop: 8, marginBottom: 0, fontSize: 12 }}>
              <CheckCircleOutlined /> 日志超过 3000 行会丢弃较早部分；
              <CloseCircleOutlined /> 超过 500 行的瞬时输出会丢中间行并标注。
            </Paragraph>
          )}
        </Card>
      </Col>
    </Row>
  );
}
