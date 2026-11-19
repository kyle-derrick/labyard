import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Card,
  Col,
  Input,
  Row,
  Segmented,
  Select,
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
  SaveOutlined,
} from "@ant-design/icons";
import { api, streamRun, type PreviewStep, type RunEvent } from "./api";
import Topology, { type Zone } from "./Topology";
import SpecVisual from "./SpecVisual";

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
      server1:   { role: server }
      client1:   { role: client }

  # 嵌套：sub 的上联是 main 的网关
  sub:
    uplink: main
    v4: 10.20.1.0/24
    # 复用 main 里新建的那台监控机
    monitor: { use: "main:probe" }
    hosts:
      gateway: { role: gateway }
      client1: { role: client }

# 精确连线：某台主机的流量复制到某台监控机（可跨网络）
mirrors:
  - { from: "sub:client1", to: "main:probe" }
`;

type LogLine = { cls: string; text: string };

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
  const [snapshot, setSnapshot] = useState(false);
  // 服务端保存的那一份。null = 从未保存过（此时以内置样例为基准判断是否改动）
  const [saved, setSaved] = useState<string | null>(null);
  const stopRef = useRef<null | (() => void)>(null);
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
      .catch(() => {});
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
