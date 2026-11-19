/** 可视化编排：直接改结构，不用手写 YAML。
 *
 * 数据源是后端的 `/api/specs/{name}/model`（YAML 的 JSON 投影），保存也走它。
 * 这样前端完全不需要懂 YAML 的锚点、缩进和 `{ role: gateway }` 这类简写——
 * 解析规则只有一份，在后端。
 *
 * 模板与存储是下拉而不是输入框：数据来自 `/api/catalog`（PVE 现查，
 * 后端有 TTL 缓存），所以下拉里只有真的存在的选项，拼错名字这种事
 * 到不了保存那一步。
 *
 * 每个 zone 有一个「流量监控」开关（`zone.monitor`）：
 * - 开 = 这个网络的流量交给一台监控机采集（整网采集，按主机逐口镜像）；
 * - 新建一台（可指定挂在哪个网络做管理口）或复用别的 zone 里已有的监控机；
 * - 关 = 不采集。
 *
 * **刻意的取舍**：这里不是拖拽画布。拖一个方框到画布上，背后要回答"它属于
 * 哪个 zone、IP 从哪来、桥怎么接"，这些答案最后还是变成表单；而拖拽还会
 * 带来"位置要不要存进 YAML"这种没有意义的问题。所以做成"卡片 + 表格"：
 * 加 zone、加主机、改参数都是点出来的，改完保存就是一份合法规格。
 *
 * 一个必须说明的代价：**保存会重排 YAML 并丢掉注释**（YAML 库不保留注释）。
 * 需要保留注释就切到 YAML 模式手工改。
 */
import { useEffect, useMemo, useState } from "react";
import {
  App as AntApp,
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Form,
  Input,
  InputNumber,
  Modal,
  Popconfirm,
  Radio,
  Row,
  Select,
  Space,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import { DeleteOutlined, PlusOutlined, SaveOutlined } from "@ant-design/icons";
import { api, type Catalog } from "./api";

const { Text } = Typography;

type HostM = {
  role?: string;
  /** 规格里的 `ip` 是**主机号**（最后一段），不是完整地址——完整地址由
   *  zone 的网段 + 这个数字得出。这里照着原样写，免得用户以为要填 10.77.0.20。 */
  ip?: number;
  vmid?: number;
  cores?: number;
  memory?: number;
  disk?: number;
  provisioner?: string;
  mirror?: string;
  /** rootfs 之外的数据盘，按顺序落成 mp0、mp1…… */
  disks?: { size: number; mount: string; storage?: string }[];
};

type MonitorM = {
  /** 复用已有监控机时是 `zone:host` 或唯一的 `host`；新建时不存在 */
  use?: string;
  /** 新建监控机的名字，默认 monitor */
  name?: string;
  /** 新建时管理口挂哪：不写 = 本 zone 内网；wan = 外网桥（DHCP）；或别的 zone */
  mgmt?: string;
  /** 自定义 IP：内网 = 主机位；外网 = 完整地址（留空 = DHCP/自动分配） */
  ip?: string;
  /** 自定义 CT 号；留空自动分配 */
  vmid?: number;
  /** 只抓这些主机；不写 = 本网络全部主机 */
  targets?: string[];
  cores?: number;
  memory?: number;
  disk?: number;
  template?: string;
  storage?: string;
};

type ZoneM = {
  uplink?: string;
  v4?: string;
  v6?: string | boolean;
  nat?: boolean;
  bridge?: string;
  gateway?: { egress?: boolean; pub?: unknown[]; vpn?: Record<string, unknown> };
  monitor?: MonitorM;
  hosts?: Record<string, HostM>;
};

type Model = {
  defaults?: Record<string, unknown>;
  zones?: Record<string, ZoneM>;
  mirrors?: { from: string; to: string }[];
};

const ROLES = [
  { value: "gateway", label: "网关" },
  { value: "monitor", label: "流量监控" },
  { value: "host", label: "主机" },
];

/** 角色下拉的选项：YAML 里可能还写着老的 server / client——后端按别名
 * 接受。这里直接显示成"主机"，不暴露"旧写法"这种内部概念：对用户来说
 * 它就是主机，值保留原样即可。 */
function roleOptions(cur?: string) {
  if (cur && (cur === "server" || cur === "client")) {
    return ROLES.map((r) => (r.value === "host" ? { value: cur, label: "主机" } : r));
  }
  return ROLES;
}

function Field({ label, children, hint }: { label: string; children: React.ReactNode; hint?: string }) {
  return (
    <div>
      <div style={{ fontSize: 11, color: "#64748b", marginBottom: 2 }}>{label}</div>
      {hint ? (
        <Tooltip title={hint}>
          <div>{children}</div>
        </Tooltip>
      ) : (
        children
      )}
    </div>
  );
}

export default function SpecVisual({
  name,
  onSaved,
}: {
  name: string;
  onSaved: (yaml: string) => void;
}) {
  const { message } = AntApp.useApp();
  const [model, setModel] = useState<Model | null>(null);
  const [catalog, setCatalog] = useState<Catalog | null>(null);
  const [exists, setExists] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [zoneOpen, setZoneOpen] = useState(false);
  const [hostFor, setHostFor] = useState<string | null>(null);
  /** 数据盘编辑器：正在给哪台主机配盘 */
  const [diskFor, setDiskFor] = useState<{ zn: string; host: string } | null>(null);
  const [diskDraft, setDiskDraft] = useState<
    { size: number; mount: string; storage?: string }[]
  >([]);
  const [zoneForm] = Form.useForm();
  const [hostForm] = Form.useForm();
  /** 草稿恢复提示：上次没保存成功的编辑还留在 localStorage 里 */
  const [draftHint, setDraftHint] = useState(false);

  const draftKey = `labyard-draft-${name}`;

  useEffect(() => {
    let alive = true;
    setBusy(true);
    api
      .specModel(name)
      .then((r) => {
        if (!alive) return;
        // 有草稿就恢复草稿——保存失败/误关页面不再白干。恢复与否都给提示。
        const draft = localStorage.getItem(draftKey);
        if (draft) {
          try {
            const parsed = JSON.parse(draft) as Model;
            setModel(parsed);
            setDirty(true);
            setDraftHint(true);
          } catch {
            localStorage.removeItem(draftKey);
            setModel((r.model ?? {}) as Model);
          }
        } else {
          setModel((r.model ?? {}) as Model);
        }
        setExists(r.exists);
        setErr(null);
      })
      .catch((e) => alive && setErr((e as Error).message))
      .finally(() => alive && setBusy(false));
    // 目录（模板/存储）失败不阻塞编辑——下拉退化为手填
    api
      .catalog()
      .then((c) => alive && setCatalog(c))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [name, draftKey]);

  const zones = model?.zones ?? {};
  const zoneNames = Object.keys(zones);

  const patch = (fn: (m: Model) => Model) => {
    setModel((prev) => {
      const next = fn(prev ?? {});
      // 每次编辑都暂存草稿：保存失败、误关标签页都能恢复
      try {
        localStorage.setItem(draftKey, JSON.stringify(next));
      } catch {
        // 存储满了之类的小事故不值得打断编辑
      }
      return next;
    });
    setDirty(true);
  };

  const discardDraft = () => {
    localStorage.removeItem(draftKey);
    setDraftHint(false);
    setDirty(false);
    // 重新拉磁盘上的版本
    setBusy(true);
    api
      .specModel(name)
      .then((r) => setModel((r.model ?? {}) as Model))
      .finally(() => setBusy(false));
  };

  const patchZone = (zn: string, p: Partial<ZoneM>) =>
    patch((m) => ({
      ...m,
      zones: { ...(m.zones ?? {}), [zn]: { ...(m.zones?.[zn] ?? {}), ...p } },
    }));

  const patchHost = (zn: string, hn: string, p: Partial<HostM>) =>
    patch((m) => {
      const z = m.zones?.[zn] ?? {};
      return {
        ...m,
        zones: {
          ...(m.zones ?? {}),
          [zn]: {
            ...z,
            hosts: { ...(z.hosts ?? {}), [hn]: { ...(z.hosts?.[hn] ?? {}), ...p } },
          },
        },
      };
    });

  const delHost = (zn: string, hn: string) =>
    patch((m) => {
      const z = m.zones?.[zn] ?? {};
      const hosts = { ...(z.hosts ?? {}) };
      delete hosts[hn];
      return { ...m, zones: { ...(m.zones ?? {}), [zn]: { ...z, hosts } } };
    });

  const delZone = (zn: string) =>
    patch((m) => {
      const zs = { ...(m.zones ?? {}) };
      delete zs[zn];
      return { ...m, zones: zs };
    });

  const save = async () => {
    if (!model) return;
    setBusy(true);
    setErr(null);
    try {
      const r = await api.putSpecModel(name, model);
      localStorage.removeItem(draftKey);
      setDraftHint(false);
      setDirty(false);
      setExists(true);
      message.success(`已保存 ${name}.yaml`);
      onSaved(r.yaml);
    } catch (e) {
      // 保存失败不清 model：草稿还在 localStorage，编辑成果都在
      const msg = (e as Error).message;
      setErr(msg);
      message.error({
        content: `保存失败（内容未丢失，草稿已暂存）：${msg}`,
        duration: 6,
      });
    } finally {
      setBusy(false);
    }
  };

  const totalHosts = useMemo(
    () => zoneNames.reduce((n, z) => n + Object.keys(zones[z].hosts ?? {}).length, 0),
    [zones, zoneNames],
  );

  const roleOf = (z: ZoneM) =>
    Object.values(z.hosts ?? {}).find((h) => h.role === "gateway") ?? null;

  /** 全部规格里 role=monitor 的主机 + 各 zone 的新建声明，供"复用已有"下拉用。
   * 声明可以跨 zone 引用（后端认 `zone:名字`，监控机实际落点由声明的 mgmt 决定）；
   * 但本 zone 自己的声明不能选——use 指向自己等于什么都没指。 */
  const monitorChoices = useMemo(() => {
    const out: { value: string; label: string }[] = [];
    for (const zn of zoneNames) {
      for (const [hn, h] of Object.entries(zones[zn].hosts ?? {})) {
        if (h.role === "monitor") out.push({ value: `${zn}:${hn}`, label: `${zn} / ${hn}` });
      }
      const m = zones[zn].monitor;
      if (m?.name && !m.use) {
        const v = `${zn}:${m.name}`;
        if (!out.some((o) => o.value === v))
          out.push({
            value: v,
            // 声明名与落点不是一回事：`lab/monitor` 管理口挂 wan 时，容器
            // 实际建在 wan 网络里。这里提前说清楚，免得用户以为会多一台。
            label: `${zn}/${m.name}（${
              m.mgmt && m.mgmt !== zn
                ? `管理口在 ${m.mgmt}，实际建在 ${m.mgmt}`
                : "建在本网络"
            }）`,
          });
      }
    }
    return out;
  }, [zones, zoneNames]);

  /** 某个 zone 选"复用已有"时的候选：排除该 zone 自己的任何条目 */
  const reuseChoices = (zn: string) => monitorChoices.filter((c) => !c.value.startsWith(`${zn}:`));

  // ── 下拉数据源 ──
  const templateOptions = (catalog?.templates ?? []).map((t) => ({
    value: t.label,
    label: t.label,
  }));
  const diskStorageOptions = (catalog?.disk_storages ?? []).map((s) => ({
    value: s.storage,
    label: `${s.storage}（${s.kind}）`,
  }));
  // 能放模板的存储（dir 类）：模板是文件，lvmthin 放不了
  const tplStorageOptions = (catalog?.template_storages ?? []).map((s) => ({
    value: s.storage,
    label: `${s.storage}（${s.kind}）`,
  }));
  const withFallback = (opts: { value: string; label: string }[], cur?: string) =>
    cur && !opts.some((o) => o.value === cur)
      ? [...opts, { value: cur, label: `${cur}（不在目录里）` }]
      : opts;

  /** 监控机管理口可挂的网络：本 zone / 外网 / 其他 zone */
  const mgmtOptions = (zn: string) => [
    { value: "", label: "本 zone 内网（推荐）" },
    { value: "wan", label: "外网（wan，DHCP）" },
    ...zoneNames
      .filter((o) => o !== zn)
      .map((o) => ({ value: o, label: `zone ${o}` })),
  ];

  return (
    <Space direction="vertical" size={12} style={{ width: "100%" }}>
      {/* 出错时不知道文件到底存不存在，别再叠加"还不存在"误导人 */}
      {!exists && !err && (
        <Alert
          type="info"
          showIcon
          message={`${name}.yaml 还不存在`}
          description="下面填完点保存就会创建。也可以切到 YAML 模式直接粘一份。"
        />
      )}
      {err && <Alert type="error" showIcon message={err} />}
      {draftHint && dirty && (
        <Alert
          type="warning"
          showIcon
          message="检测到上次未保存成功的编辑，已自动恢复草稿"
          description={
            <Space size={8}>
              <Text type="secondary" style={{ fontSize: 12 }}>
                确认无误后点「保存」写回；也可以丢弃草稿回到磁盘上的版本。
              </Text>
              <Button size="small" danger onClick={discardDraft}>
                丢弃草稿
              </Button>
            </Space>
          }
        />
      )}
      {dirty && !draftHint && (
        <Alert
          type="warning"
          showIcon
          message="有未保存的修改（已自动暂存草稿，误关页面也能恢复）"
        />
      )}

      <Card
        size="small"
        title={
          <Space size={8}>
            <span>默认配置</span>
            <Text type="secondary" style={{ fontSize: 11 }}>
              没单独写的字段都继承这里
            </Text>
          </Space>
        }
      >
        <Row gutter={10}>
          {[
            ["cores", "CPU 核数"],
            ["memory", "内存 MB"],
            ["disk", "磁盘 GB"],
          ].map(([k, label]) => (
            <Col span={8} key={k}>
              <Field label={label}>
                <InputNumber
                  size="small"
                  min={1}
                  style={{ width: "100%" }}
                  value={(model?.defaults?.[k] as number) ?? undefined}
                  onChange={(v) =>
                    patch((m) => ({
                      ...m,
                      defaults: { ...(m.defaults ?? {}), [k]: v ?? undefined },
                    }))
                  }
                />
              </Field>
            </Col>
          ))}
          <Col span={12} style={{ marginTop: 8 }}>
            <Field label="模板">
              <Select
                size="small"
                showSearch
                style={{ width: "100%" }}
                placeholder={catalog ? "从列表选择" : "debian-13-standard"}
                value={(model?.defaults?.template as string) ?? undefined}
                options={withFallback(templateOptions, model?.defaults?.template as string | undefined)}
                onChange={(v) =>
                  patch((m) => ({
                    ...m,
                    defaults: { ...(m.defaults ?? {}), template: v },
                  }))
                }
              />
            </Field>
          </Col>
          <Col span={12} style={{ marginTop: 8 }}>
            <Field label="系统盘存储">
              <Select
                size="small"
                showSearch
                style={{ width: "100%" }}
                placeholder={catalog ? "从列表选择" : "local-lvm"}
                value={(model?.defaults?.storage as string) ?? undefined}
                options={withFallback(diskStorageOptions, model?.defaults?.storage as string | undefined)}
                onChange={(v) =>
                  patch((m) => ({
                    ...m,
                    defaults: { ...(m.defaults ?? {}), storage: v },
                  }))
                }
              />
            </Field>
          </Col>
          <Col span={12} style={{ marginTop: 8 }}>
            <Field
              label="模板存储"
              hint="模板文件（tar.zst）放在哪个存储。模板是文件，只有 dir 类存储能放；不填跟随系统盘存储"
            >
              <Select
                size="small"
                showSearch
                allowClear
                style={{ width: "100%" }}
                placeholder="跟随系统盘"
                value={(model?.defaults?.template_storage as string) ?? undefined}
                options={withFallback(tplStorageOptions, model?.defaults?.template_storage as string | undefined)}
                onChange={(v) =>
                  patch((m) => {
                    const d = { ...(m.defaults ?? {}) };
                    if (v) d.template_storage = v;
                    else delete d.template_storage;
                    return { ...m, defaults: d };
                  })
                }
              />
            </Field>
          </Col>
        </Row>
      </Card>

      {zoneNames.length === 0 && (
        <Empty
          image={Empty.PRESENTED_IMAGE_SIMPLE}
          description="还没有网络——一个网络就是一段内网 + 一台网关"
        />
      )}

      {zoneNames.map((zn) => {
        const z = zones[zn];
        const gw = roleOf(z);
        const hostEntries = Object.entries(z.hosts ?? {});
        const hostRows = hostEntries.map(([hn, h]) => ({ name: hn, ...h }));
        const mon = z.monitor;
        const monOn = !!mon;
        const monMode = mon?.use ? "use" : "new";
        return (
          <Card
            key={zn}
            size="small"
            title={
              <Space size={8}>
                <Text strong>{zn}</Text>
                <Tag className="mono">{z.v4 ?? "无网段"}</Tag>
                {gw ? (
                  <Tag color="green">网关 {hostEntries.find(([, h]) => h.role === "gateway")?.[0]}</Tag>
                ) : (
                  <Tag color="red">没有网关</Tag>
                )}
                {monOn && (
                  <Tag color="blue">
                    流量监控 {monMode === "use" ? `← ${mon!.use}` : `← 新建 ${mon!.name || "monitor"}`}
                  </Tag>
                )}
              </Space>
            }
            extra={
              <Popconfirm
                title={`从规格里删掉网络 ${zn}？`}
                description="只改文件，PVE 上的容器不动。"
                okText="删除"
                okButtonProps={{ danger: true }}
                onConfirm={() => delZone(zn)}
              >
                <Button size="small" type="text" danger icon={<DeleteOutlined />} />
              </Popconfirm>
            }
          >
            <Row gutter={10}>
              <Col span={8}>
                <Field label="上联">
                  <Select
                    size="small"
                    style={{ width: "100%" }}
                    value={z.uplink ?? "wan"}
                    onChange={(v) => patchZone(zn, { uplink: v })}
                    options={[
                      { value: "wan", label: "外网（wan）" },
                      ...zoneNames
                        .filter((o) => o !== zn)
                        .map((o) => ({ value: o, label: `网络 ${o}` })),
                    ]}
                  />
                </Field>
              </Col>
              <Col span={9}>
                <Field label="网段（CIDR）">
                  <Input
                    size="small"
                    className="mono"
                    placeholder="10.10.1.0/24"
                    value={z.v4 ?? ""}
                    onChange={(e) => patchZone(zn, { v4: e.target.value })}
                  />
                </Field>
              </Col>
              <Col span={7}>
                <Field label="出网">
                  <Tooltip title="本网段的主机能否访问上游/外网。开 = 网关转发 + NAT 一起生效；关 = 立即断外网。高级场景（上游已手动路由整段、只想去掉 NAT）请改 YAML 里的 nat 字段。">
                    <Switch
                      size="small"
                      checked={z.nat !== false && z.gateway?.egress !== false}
                      onChange={(v) =>
                        patchZone(zn, {
                          nat: v,
                          gateway: { ...(z.gateway ?? {}), egress: v },
                        })
                      }
                    />
                  </Tooltip>
                </Field>
              </Col>
            </Row>

            {/* ── 流量监控开关 ── */}
            <div
              style={{
                marginTop: 10,
                padding: "8px 10px",
                border: "1px solid var(--border, #e2e8f0)",
                borderRadius: 6,
                background: monOn ? "rgba(22,119,255,0.04)" : "transparent",
              }}
            >
              <Space size={10} align="center" wrap>
                <Text strong>流量监控</Text>
                <Switch
                  size="small"
                  checked={monOn}
                  onChange={(v) =>
                    patchZone(zn, {
                      monitor: v
                        ? { name: "monitor" }
                        : undefined,
                    })
                  }
                />
                {monOn ? (
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    开启后，本网络每台主机的接口流量会被复制到监控机对应网口
                  </Text>
                ) : (
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    关闭——需要采集时打开，单台主机的精确连线请到「拓扑」页加
                  </Text>
                )}
              </Space>
              {monOn && (
                <div style={{ marginTop: 8 }}>
                  <Radio.Group
                    size="small"
                    value={monMode}
                    onChange={(e) => {
                      const v = e.target.value as "new" | "use";
                      if (v === "new") {
                        patchZone(zn, {
                          monitor: { name: mon?.name || "monitor", mgmt: mon?.mgmt, cores: mon?.cores, memory: mon?.memory, disk: mon?.disk },
                        });
                      } else {
                        const c = reuseChoices(zn)[0];
                        if (!c) {
                          message.warning("当前没有别的监控机可复用——请先在其它 zone 新建，或改选「新建一台」");
                          return;
                        }
                        patchZone(zn, { monitor: { use: c.value } });
                      }
                    }}
                    options={[
                      { value: "new", label: "新建一台" },
                      { value: "use", label: "复用已有" },
                    ]}
                    style={{ marginBottom: 8 }}
                  />
                  {monMode === "new" ? (
                    <Row gutter={8}>
                      <Col span={5}>
                        <Field label="名字">
                          <Input
                            size="small"
                            className="mono"
                            value={mon?.name ?? "monitor"}
                            onChange={(e) =>
                              patchZone(zn, { monitor: { ...mon!, name: e.target.value || undefined } })
                            }
                          />
                        </Field>
                      </Col>
                      <Col span={7}>
                        <Field label="管理口挂在" hint="监控机自己的管理网卡接哪：默认本网段，也可以挂到外网或别的网络（边界采集）。监控机自身的内网口也属于镜像范围（内网口 → 抓包口没有回环）。">
                          <Select
                            size="small"
                            style={{ width: "100%" }}
                            value={mon?.mgmt ?? ""}
                            options={mgmtOptions(zn)}
                            onChange={(v) =>
                              patchZone(zn, { monitor: { ...mon!, mgmt: v || undefined } })
                            }
                          />
                        </Field>
                      </Col>
                      {(["cores", "memory", "disk"] as const).map((k) => (
                        <Col span={4} key={k}>
                          <Field
                            label={k === "cores" ? "CPU" : k === "memory" ? "内存 MB" : "磁盘 GB"}
                          >
                            <InputNumber
                              size="small"
                              min={1}
                              style={{ width: "100%" }}
                              placeholder={String(model?.defaults?.[k] ?? "")}
                              value={mon?.[k]}
                              onChange={(nv) =>
                                patchZone(zn, { monitor: { ...mon!, [k]: nv ?? undefined } })
                              }
                            />
                          </Field>
                        </Col>
                      ))}
                      <Col span={24} style={{ marginTop: 8 }}>
                        <Row gutter={8}>
                          <Col span={8}>
                            <Field
                              label="IP（可选）"
                              hint={
                                (mon?.mgmt ?? "") === "wan"
                                  ? "挂外网：填完整地址（如 10.77.0.55/24）就是静态，留空 = DHCP"
                                  : "内网：填主机位（如 10，即网段的 .10）；留空按角色默认位自动分配"
                              }
                            >
                              <Input
                                size="small"
                                className="mono"
                                placeholder={
                                  (mon?.mgmt ?? "") === "wan" ? "留空 = DHCP" : "留空 = 自动分配"
                                }
                                value={mon?.ip ?? ""}
                                onChange={(e) =>
                                  patchZone(zn, {
                                    monitor: { ...mon!, ip: e.target.value || undefined },
                                  })
                                }
                              />
                            </Field>
                          </Col>
                          <Col span={8}>
                            <Field label="CT 号（可选）" hint="指定容器编号；留空自动分配">
                              <InputNumber
                                size="small"
                                min={100}
                                max={999999999}
                                style={{ width: "100%" }}
                                placeholder="自动分配"
                                value={mon?.vmid}
                                onChange={(nv) =>
                                  patchZone(zn, { monitor: { ...mon!, vmid: nv ?? undefined } })
                                }
                              />
                            </Field>
                          </Col>
                        </Row>
                      </Col>
                    </Row>
                  ) : (
                    <Field
                      label="复用哪台监控机"
                      hint={
                        reuseChoices(zn).length === 0
                          ? "当前没有其它 zone 的监控机可复用——先在别的 zone 新建一台，或改选「新建一台」"
                          : undefined
                      }
                    >
                      <Select
                        size="small"
                        style={{ width: "100%", maxWidth: 360 }}
                        placeholder="选择已有的流量监控机"
                        value={mon?.use}
                        options={reuseChoices(zn)}
                        onChange={(v) => patchZone(zn, { monitor: { use: v } })}
                        notFoundContent="没有可复用的监控机（本 zone 的新建声明不能选给自己）"
                      />
                    </Field>
                  )}
                </div>
              )}
            </div>

            <Table
              size="small"
              style={{ marginTop: 10 }}
              rowKey="name"
              dataSource={hostRows}
              pagination={false}
              locale={{ emptyText: "这个网络还没有主机" }}
              columns={[
                {
                  title: "主机",
                  dataIndex: "name",
                  width: 110,
                  render: (v: string) => <Text strong>{v}</Text>,
                },
                {
                  title: "角色",
                  dataIndex: "role",
                  width: 130,
                  render: (v: string | undefined, r) => (
                    <Select
                      size="small"
                      style={{ width: "100%" }}
                      value={v ?? "host"}
                      onChange={(nv) => patchHost(zn, r.name, { role: nv })}
                      options={roleOptions(v)}
                    />
                  ),
                },
                {
                  title: "IP 末位",
                  dataIndex: "ip",
                  width: 84,
                  render: (v: number | undefined, r) => (
                    <Tooltip title="主机号（网段的最后一段）。留空按角色自动分配：网关 .1、流量监控 .10、主机 .20">
                      <InputNumber
                        size="small"
                        min={1}
                        max={254}
                        placeholder="自动"
                        style={{ width: "100%" }}
                        value={v}
                        onChange={(nv) => patchHost(zn, r.name, { ip: nv ?? undefined })}
                      />
                    </Tooltip>
                  ),
                },
                {
                  title: "vmid",
                  dataIndex: "vmid",
                  width: 88,
                  render: (v: number | undefined, r) => (
                    <InputNumber
                      size="small"
                      style={{ width: "100%" }}
                      placeholder="自动"
                      value={v}
                      onChange={(nv) => patchHost(zn, r.name, { vmid: nv ?? undefined })}
                    />
                  ),
                },
                // 三个数字原来挤在一列（标题 "C/M/G"），既看不懂也没法写单位。
                // 拆成三列，表头直接用中文，单位放 tooltip。
                ...(
                  [
                    { key: "cores", label: "CPU", unit: "核", w: 62 },
                    { key: "memory", label: "内存", unit: "MB", w: 72 },
                    { key: "disk", label: "磁盘", unit: "GB", w: 72 },
                  ] as const
                ).map((c) => ({
                  title: (
                    <Tooltip title={`${c.label}（${c.unit}）。留空跟随默认配置`}>
                      <span>{c.label}</span>
                    </Tooltip>
                  ),
                  width: c.w,
                  render: (_: unknown, r: HostM & { name: string }) => (
                    <InputNumber
                      size="small"
                      style={{ width: "100%" }}
                      value={r[c.key]}
                      placeholder={String(model?.defaults?.[c.key] ?? "")}
                      onChange={(nv) => patchHost(zn, r.name, { [c.key]: nv ?? undefined })}
                    />
                  ),
                })),
                {
                  title: "数据盘",
                  width: 78,
                  render: (_, r) => (
                    <Button
                      size="small"
                      type="link"
                      style={{ padding: 0 }}
                      onClick={() => {
                        const cur = (model?.zones?.[zn]?.hosts?.[r.name]?.disks ?? []).map((d) => ({
                          ...d,
                        }));
                        setDiskDraft(cur.length ? cur : [{ size: 10, mount: "/data" }]);
                        setDiskFor({ zn, host: r.name });
                      }}
                    >
                      {r.disks?.length ? `${r.disks.length} 块` : "添加"}
                    </Button>
                  ),
                },
                {
                  title: "",
                  width: 40,
                  render: (_, r) => (
                    <Button
                      size="small"
                      type="text"
                      danger
                      icon={<DeleteOutlined />}
                      onClick={() => delHost(zn, r.name)}
                    />
                  ),
                },
              ]}
            />
            <Button
              size="small"
              type="dashed"
              icon={<PlusOutlined />}
              style={{ marginTop: 8, width: "100%" }}
              onClick={() => {
                hostForm.resetFields();
                setHostFor(zn);
              }}
            >
              添加主机
            </Button>
          </Card>
        );
      })}

      <Space>
        <Button size="small" icon={<PlusOutlined />} onClick={() => setZoneOpen(true)}>
          添加网络
        </Button>
        <Button
          size="small"
          type="primary"
          icon={<SaveOutlined />}
          loading={busy}
          disabled={!dirty}
          onClick={save}
        >
          保存到 {name}.yaml
        </Button>
        <Text type="secondary" style={{ fontSize: 11 }}>
          可视化保存会重排 YAML、丢注释；要留注释请切到 YAML 模式。
        </Text>
      </Space>

      <Text type="secondary" style={{ fontSize: 12 }}>
        {zoneNames.length} 个网络 / {totalHosts} 台主机
        {(model?.mirrors?.length ?? 0) > 0
          ? ` · ${model!.mirrors!.length} 条镜像连线（在「拓扑」页增删）`
          : ""}
      </Text>

      <Modal
        open={!!diskFor}
        title={diskFor ? `${diskFor.zn}:${diskFor.host} 的数据盘` : "数据盘"}
        onCancel={() => setDiskFor(null)}
        onOk={() => {
          if (!diskFor) return;
          // 空挂载点/0 容量在这里就拦住，别等 apply 才报
          for (const d of diskDraft) {
            if (!/^\/.+/.test(d.mount.trim())) {
              message.error(`挂载点要写绝对路径（如 /data），当前是 \`${d.mount}\``);
              return;
            }
            if (!d.size || d.size < 1) {
              message.error(`${d.mount} 的大小必须 ≥ 1 GB`);
              return;
            }
          }
          patchHost(diskFor.zn, diskFor.host, {
            disks: diskDraft.length
              ? diskDraft.map((d) => ({
                  size: d.size,
                  mount: d.mount.trim(),
                  ...(d.storage ? { storage: d.storage } : {}),
                }))
              : undefined,
          });
          setDiskFor(null);
        }}
        okText="保存到规格"
        width={640}
        destroyOnClose
      >
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 10 }}
          message="数据盘挂在 rootfs 之外，按顺序落成 mp0、mp1……"
          description="只能加和扩容，不能在这里缩容或删除（缩盘等于删数据）。改完要 apply 才生效。"
        />
        <Table
          size="small"
          pagination={false}
          rowKey={(_, i) => String(i)}
          dataSource={diskDraft}
          locale={{ emptyText: "还没有数据盘" }}
          columns={[
            {
              title: "挂载点",
              render: (_, r, i) => (
                <Input
                  size="small"
                  className="mono"
                  value={r.mount}
                  onChange={(e) => {
                    const n = [...diskDraft];
                    n[i] = { ...n[i], mount: e.target.value };
                    setDiskDraft(n);
                  }}
                />
              ),
            },
            {
              title: "容量 GB",
              width: 110,
              render: (_, r, i) => (
                <InputNumber
                  size="small"
                  min={1}
                  style={{ width: "100%" }}
                  value={r.size}
                  onChange={(v) => {
                    const n = [...diskDraft];
                    n[i] = { ...n[i], size: Number(v ?? 0) };
                    setDiskDraft(n);
                  }}
                />
              ),
            },
            {
              title: "存储",
              width: 170,
              render: (_, r, i) => (
                <Select
                  size="small"
                  allowClear
                  style={{ width: "100%" }}
                  placeholder="跟随 rootfs"
                  value={r.storage}
                  options={withFallback(
                    diskStorageOptions,
                    r.storage,
                  )}
                  onChange={(v) => {
                    const n = [...diskDraft];
                    n[i] = { ...n[i], storage: v || undefined };
                    setDiskDraft(n);
                  }}
                />
              ),
            },
            {
              title: "",
              width: 40,
              render: (_, __, i) => (
                <Button
                  size="small"
                  type="text"
                  danger
                  icon={<DeleteOutlined />}
                  onClick={() => setDiskDraft(diskDraft.filter((_, k) => k !== i))}
                />
              ),
            },
          ]}
        />
        <Button
          size="small"
          type="dashed"
          icon={<PlusOutlined />}
          style={{ marginTop: 8, width: "100%" }}
          onClick={() =>
            setDiskDraft([...diskDraft, { size: 10, mount: `/data${diskDraft.length + 1}` }])
          }
        >
          再加一块
        </Button>
      </Modal>

      <Modal
        open={zoneOpen}
        title="添加网络"
        onCancel={() => setZoneOpen(false)}
        onOk={() => zoneForm.submit()}
        okText="添加"
        destroyOnClose
      >
        <Form
          form={zoneForm}
          layout="vertical"
          preserve={false}
          onFinish={(v: { name: string; uplink: string; v4: string }) => {
            if (zones[v.name]) {
              message.error(`网络 ${v.name} 已存在`);
              return;
            }
            patch((m) => ({
              ...m,
              zones: {
                ...(m.zones ?? {}),
                [v.name]: {
                  uplink: v.uplink,
                  v4: v.v4,
                  hosts: { gateway: { role: "gateway" } },
                },
              },
            }));
            setZoneOpen(false);
          }}
        >
          <Form.Item
            name="name"
            label="名字"
            rules={[
              { required: true },
              {
                pattern: /^[a-z][a-z0-9-]*$/,
                message: "小写字母开头，只用小写字母、数字、连字符",
              },
              { max: 10, message: "不超过 10 个字符（桥名有长度限制）" },
            ]}
          >
            <Input placeholder="lab2" className="mono" />
          </Form.Item>
          <Form.Item name="uplink" label="上联" initialValue="wan">
            <Select
              options={[
                { value: "wan", label: "外网（wan）" },
                ...zoneNames.map((o) => ({ value: o, label: `网络 ${o}` })),
              ]}
            />
          </Form.Item>
          <Form.Item
            name="v4"
            label="网段"
            rules={[{ required: true, message: "写一个 CIDR，例如 10.30.1.0/24" }]}
          >
            <Input placeholder="10.30.1.0/24" className="mono" />
          </Form.Item>
          <Text type="secondary" style={{ fontSize: 12 }}>
            会自动加一台 <code>gateway</code> 主机；其余主机在里面加。
          </Text>
        </Form>
      </Modal>

      <Modal
        open={!!hostFor}
        title={`给 ${hostFor ?? ""} 添加主机`}
        onCancel={() => setHostFor(null)}
        onOk={() => hostForm.submit()}
        okText="添加"
        destroyOnClose
      >
        <Form
          form={hostForm}
          layout="vertical"
          preserve={false}
          onFinish={(v: { name: string; role: string }) => {
            const zn = hostFor!;
            if (zones[zn]?.hosts?.[v.name]) {
              message.error(`主机 ${v.name} 已存在`);
              return;
            }
            patchHost(zn, v.name, { role: v.role });
            setHostFor(null);
          }}
        >
          <Form.Item
            name="name"
            label="名字"
            rules={[
              { required: true },
              {
                pattern: /^[a-z][a-z0-9-]*$/,
                message: "小写字母开头，只用小写字母、数字、连字符",
              },
            ]}
          >
            <Input placeholder="web1" className="mono" />
          </Form.Item>
          <Form.Item name="role" label="角色" initialValue="host">
            <Select options={ROLES} />
          </Form.Item>
        </Form>
      </Modal>
    </Space>
  );
}
