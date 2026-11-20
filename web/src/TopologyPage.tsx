/** 拓扑页：一张图看完整环境，点节点直接改。
 *
 * 设计上只有一件事要守住：**图是入口，不是截图**。看到网关的"出网"是关的，
 * 就在这张图上打开；看到某台机停了，就在这里起来；想把某个网络的流量交给
 * 监控机，在网络节点上右键"开启整网流量监控"。改结构（加 zone、加主机）
 * 仍然要去「编排」，因为那是在改文件，不是在改运行态。
 *
 * 流量采集的入口收敛到**网络级**：整网监控由每个网络的开关控制（规格里的
 * `monitor:` 字段），主机级别的零散连线不再提供——那是老设计，和"一个网络
 * 一个抓包口、自动覆盖全网主机"的总体设计冲突。后台巡检每 5 分钟对一次账，
 * 规格里说该有的镜像实例漏了会自动补装。
 */
import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  App as AntApp,
  Button,
  Card,
  Descriptions,
  Drawer,
  Empty,
  Form,
  Input,
  InputNumber,
  Modal,
  Popconfirm,
  Select,
  Space,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
} from "antd";
import {
  ApartmentOutlined,
  DeleteOutlined,
  PlusOutlined,
  ReloadOutlined,
  ThunderboltOutlined,
} from "@ant-design/icons";
import TopologyGraph, { ROLE_LABEL } from "./TopologyGraph";
import { HostConfigEditor, type ConfigTarget } from "./HostConfig";
import { api, terminalUrl, vpnClientUrl, type GatewayStatus, type MirrorRow, type ReconcileInfo, type TopoEdge, type TopoNode } from "./api";

const { Text } = Typography;

function StatusTag({ n }: { n: TopoNode }) {
  if (n.retired) return <Tag color="orange">已废弃</Tag>;
  if (n.kind !== "host") return null;
  if (n.status === "running") return <Tag color="green">运行中</Tag>;
  if (n.status === "stopped") return <Tag>已停止</Tag>;
  return <Tag color="red">不存在</Tag>;
}

type MenuState = { x: number; y: number; node: TopoNode | null };

export default function TopologyPage() {
  const { message } = AntApp.useApp();
  const qc = useQueryClient();
  const [spec, setSpec] = useState<string | undefined>(undefined);
  const [picked, setPicked] = useState<TopoNode | null>(null);
  /** 正在编辑配置的主机（null = 弹窗关闭） */
  const [cfg, setCfg] = useState<ConfigTarget | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [busy, setBusy] = useState(false);
  const [pubOpen, setPubOpen] = useState(false);
  const [pubForm] = Form.useForm();

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
  const reconcile = mirrors.data?.reconcile;

  const hosts = useMemo(() => nodes.filter((n) => n.kind === "host"), [nodes]);
  const monitors = useMemo(() => hosts.filter((n) => n.role === "monitor"), [hosts]);

  /** 总账分组：监控机 → 被采集网络 → 逐口行。 */
  /** 镜像总账的树形数据源：监控机 → 网络 → 逐口行。antd Table 直接吃。 */
  type LedgerRow = {
    key: string;
    name: React.ReactNode;
    bridge?: React.ReactNode;
    iface?: React.ReactNode;
    spec?: string;
    active?: string;
    row?: MirrorRow;
    children?: LedgerRow[];
  };
  const ledger = useMemo<LedgerRow[]>(() => {
    const byMon = new Map<string, MirrorRow[]>();
    for (const r of rows) {
      (byMon.get(r.to) ?? byMon.set(r.to, []).get(r.to)!).push(r);
    }
    return [...byMon.entries()].map(([mon, rs]) => {
      const byZone = new Map<string, MirrorRow[]>();
      for (const r of rs) {
        const zone = r.from.split(":")[0];
        (byZone.get(zone) ?? byZone.set(zone, []).get(zone)!).push(r);
      }
      return {
        key: `mon:${mon}`,
        name: (
          <Space size={8}>
            <Tag color="blue" style={{ marginInlineEnd: 0 }}>
              监控机 {rs[0]?.to_alias ?? mon}
            </Tag>
            {rs[0]?.to_alias && rs[0]?.to_alias !== mon && (
              <Text type="secondary" style={{ fontSize: 12 }}>
                实际建在 {mon}
              </Text>
            )}
            <Text type="secondary" style={{ fontSize: 12 }}>
              {rs.length} 条连线 · {byZone.size} 个网络
            </Text>
          </Space>
        ),
        children: [...byZone.entries()].map(([zone, zrs]) => {
          const netNode = nodes.find((n) => n.id === `net:${zone}`);
          const nm = netNode?.meta as Record<string, unknown> | undefined;
          return {
            key: `net:${mon}:${zone}`,
            name: (
              <Space size={6}>
                <Text strong style={{ fontSize: 12 }}>
                  {netNode?.label ?? `${zone} 内网`}
                </Text>
                <Text type="secondary" className="mono" style={{ fontSize: 11 }}>
                  {String(nm?.cidr ?? "")}
                </Text>
              </Space>
            ),
            bridge: (
              <span className="mono" style={{ fontSize: 11 }}>
                {zrs[0]?.capture_bridge ?? "—"}
              </span>
            ),
            children: zrs.map((r) => {
              const host = r.from.replace(/^host:[^:]+:/, "");
              return {
                key: `row:${mon}:${r.from}`,
                name: (
                  <span className="mono" style={{ fontSize: 12 }}>
                    {host}
                  </span>
                ),
                iface: (
                  <span className="mono" style={{ fontSize: 12 }}>
                    {r.source_iface ?? "eth0"} → {r.target_iface ?? "?"}
                  </span>
                ),
                spec: r.spec,
                active: r.active,
                row: r,
              };
            }),
          };
        }),
      };
    });
  }, [rows, nodes]);
  const networks = useMemo(
    () => nodes.filter((n) => n.kind === "network" && !(n.meta as Record<string, unknown>).external),
    [nodes],
  );
  /** 网关面板：只有打开网络节点的抽屉时才查，避免每次进页面都打一次 pvesh */
  const pickedZone = picked?.kind === "network" && picked.zone ? picked.zone : null;
  const gw = useQuery({
    queryKey: ["gateway", pickedZone ?? ""],
    queryFn: () => api.gateway(pickedZone as string),
    enabled: !!pickedZone,
    retry: 0,
  });

  // 右键菜单：点别处关掉
  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    window.addEventListener("click", close);
    window.addEventListener("scroll", close, true);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("scroll", close, true);
    };
  }, [menu]);

  const refresh = () => {
    topo.refetch();
    mirrors.refetch();
    if (pickedZone) gw.refetch();
  };

  const afterChange = (msg: string) => {
    message.success(msg);
    // 网关/电源是异步起 run 的，给它一点时间再刷图
    setTimeout(refresh, 1200);
    qc.invalidateQueries({ queryKey: ["hosts"] });
  };

  /** 右键开关某网络的整网流量监控（写规格 + 立刻落地抓包口） */
  const toggleMonitor = async (node: TopoNode, on: boolean) => {
    if (!node.zone) return;
    setBusy(true);
    try {
      await api.setMonitor(node.zone, on);
      afterChange(on ? "已开启整网流量监控（抓包口安装中）" : "已关闭流量监控");
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

  const power = async (action: "start" | "stop" | "reboot") => {
    if (!picked?.zone) return;
    setBusy(true);
    try {
      await api.power(picked.zone, picked.label, action);
      afterChange(`已${action === "start" ? "启动" : action === "stop" ? "停止" : "重启"} ${picked.label}`);
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const retire = async (revive: boolean) => {
    if (!picked?.zone) return;
    setBusy(true);
    try {
      if (revive) await api.reviveHost(picked.zone, picked.label);
      else await api.retireHost(picked.zone, picked.label);
      afterChange(revive ? "已恢复" : "已废弃（停机、保留 IP 与磁盘）");
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const toggleEgress = async (on: boolean) => {
    if (!pickedZone) return;
    setBusy(true);
    try {
      await api.setEgress(pickedZone, on);
      afterChange(on ? "出网已开启" : "出网已关闭");
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const addPub = async (v: { proto: string; port: number; to: string }) => {
    if (!pickedZone) return;
    setBusy(true);
    try {
      await api.addPub(pickedZone, v);
      setPubOpen(false);
      pubForm.resetFields();
      afterChange("已添加端口映射");
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  // ─────────────────────────── 右键菜单 ───────────────────────────
  const menuNode = menu?.node ?? null;

  const menuItems: { key: string; label: React.ReactNode; danger?: boolean; onClick: () => void }[] = [];

  if (menuNode) {
    menuItems.push({
      key: "detail",
      label: "打开详情",
      onClick: () => {
        setPicked(menuNode);
        setMenu(null);
      },
    });
    if (menuNode.kind === "host" && menuNode.zone) {
      menuItems.push({
        key: "term",
        label: "打开终端",
        onClick: () => {
          window.open(terminalUrl(menuNode.zone!, menuNode.label), "_blank", "noopener");
          setMenu(null);
        },
      });
      menuItems.push({
        key: "power",
        label: menuNode.status === "running" ? "停止" : "启动",
        onClick: async () => {
          setMenu(null);
          setBusy(true);
          try {
            await api.power(
              menuNode.zone!,
              menuNode.label,
              menuNode.status === "running" ? "stop" : "start",
            );
            afterChange(menuNode.status === "running" ? "已发送停止" : "已发送启动");
          } catch (e) {
            message.error((e as Error).message);
          } finally {
            setBusy(false);
          }
        },
      });
    }
    if (menuNode.kind === "network" && menuNode.zone && !menuNode.meta.external) {
      menuItems.push({
        key: "egress",
        label: menuNode.meta.egress ? "关闭出网" : "开启出网",
        onClick: async () => {
          setMenu(null);
          setBusy(true);
          try {
            await api.setEgress(menuNode.zone!, !menuNode.meta.egress);
            afterChange(menuNode.meta.egress ? "出网已关闭" : "出网已开启");
          } catch (e) {
            message.error((e as Error).message);
          } finally {
            setBusy(false);
          }
        },
      });
      menuItems.push({
        key: "monitor",
        label: menuNode.meta.monitor ? "关闭整网流量监控" : "开启整网流量监控",
        onClick: async () => {
          const on = !menuNode.meta.monitor;
          setMenu(null);
          await toggleMonitor(menuNode, on);
        },
      });
    }
    if (menuNode.kind === "network" && menuNode.meta.cidr) {
      menuItems.push({
        key: "copy",
        label: "复制网段",
        onClick: () => {
          navigator.clipboard?.writeText(String(menuNode.meta.cidr));
          message.success("已复制 " + menuNode.meta.cidr);
          setMenu(null);
        },
      });
    }
  } else {
    menuItems.push({
      key: "fit",
      label: "刷新拓扑",
      onClick: () => {
        setMenu(null);
        refresh();
      },
    });
  }

  const detail = picked;

  return (
    <Space direction="vertical" size={16} style={{ width: "100%" }}>
      <Card
        size="small"
        title={
          <Space size={8}>
            <ApartmentOutlined />
            <span>全局拓扑</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              {nodes.filter((n) => n.kind === "network").length} 网络 /{" "}
              {hosts.length} 主机 · 点节点看详情，悬停监控机看采集链路；整网监控在网络节点上右键开关
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
              options={(specs.data?.specs ?? []).map((s) => ({ value: s.name, label: s.name }))}
            />
            <Tooltip title="重新解析规格并读取容器状态">
              <Button
                size="small"
                icon={<ReloadOutlined />}
                loading={topo.isFetching}
                onClick={refresh}
              >
                刷新
              </Button>
            </Tooltip>
          </Space>
        }
      >
        {topo.isError ? (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={(topo.error as Error).message} />
        ) : nodes.length === 0 && !topo.isLoading ? (
          <Empty
            image={Empty.PRESENTED_IMAGE_SIMPLE}
            description="还没有规格——先去「编排」页建一份"
          />
        ) : (
          <TopologyGraph
            nodes={nodes}
            edges={edges}
            selected={selected}
            onPick={(n) => setPicked(n)}
            onSelect={(ids) =>
              // 内容没变就不更新：React Flow 在节点对象被整体替换后会重新
              // 触发 onSelectionChange，这里再无条件 setState 就是死循环
              setSelected((prev) =>
                prev.length === ids.length && prev.every((v, i) => v === ids[i]) ? prev : ids,
              )
            }
            onContextMenu={(e, n) => setMenu({ x: e.clientX, y: e.clientY, node: n })}
          />
        )}

        <div className="topo-legend">
          <span className="lg">
            <b style={{ background: "#eef2ff", borderColor: "#8b5cf6", borderStyle: "dashed" }} />
            外网
          </span>
          <span className="lg">
            <b style={{ background: "#faf5ff", borderColor: "#a78bfa" }} />
            内网（zone）
          </span>
          <span className="lg">
            <b style={{ background: "#fafbfd", borderColor: "#d7dee9", borderStyle: "dashed" }} />
            网段框（一层=一个网段）
          </span>
          <span className="lg">
            <b style={{ background: "#f0fdf4", borderColor: "#16a34a" }} />
            网关
          </span>
          <span className="lg">
            <b style={{ background: "#eff6ff", borderColor: "#2563eb" }} />
            流量监控
          </span>
          <span className="lg">
            <b style={{ background: "#ffffff" }} />
            主机
          </span>
          <span className="lg">
            <i />
            上联口
          </span>
          <span className="lg">
            <i className="thin" />
            内网口
          </span>
          <span className="lg">
            <i className="dash" />
            流量镜像
          </span>
        </div>
      </Card>

      {/* 右键菜单：自绘，位置跟鼠标走 */}
      {menu && (
        <div
          className="ctx-menu"
          style={{ left: menu.x, top: menu.y }}
          onClick={(e) => e.stopPropagation()}
        >
          {menuItems.map((it) => (
            <div key={it.key} className="ctx-item" onClick={it.onClick}>
              {it.label}
            </div>
          ))}
        </div>
      )}

      {/* 详情抽屉：网络与主机两种 */}
      <Drawer
        open={!!detail}
        onClose={() => setPicked(null)}
        width={560}
        title={
          detail && (
            <Space>
              <span>{detail.label}</span>
              <StatusTag n={detail} />
              {detail.role && <Tag>{ROLE_LABEL[detail.role] ?? detail.role}</Tag>}
            </Space>
          )
        }
      >
        {detail?.kind === "network" && (
          <NetworkDetail
            node={detail}
            gw={gw.data}
            loading={gw.isLoading}
            busy={busy}
            monitors={monitors}
            mirrorRows={rows}
            onAfterChange={afterChange}
            onRefreshMirrors={() => mirrors.refetch()}
            onEgress={toggleEgress}
            onAddPub={() => setPubOpen(true)}
            onDelPub={async (idx) => {
              setBusy(true);
              try {
                await api.delPub(detail.zone!, idx);
                afterChange("已删除映射");
              } catch (e) {
                message.error((e as Error).message);
              } finally {
                setBusy(false);
              }
            }}
          />
        )}

        {detail?.kind === "host" && (
          <Space direction="vertical" size={14} style={{ width: "100%" }}>
            {/* 标签列不换行：430px 宽时 "角色"/"状态" 会被折成竖排，
                值 "running" 也会断成 "runni/ng" */}
            <Descriptions
              size="small"
              column={2}
              bordered
              labelStyle={{ whiteSpace: "nowrap", width: 76 }}
              contentStyle={{ wordBreak: "keep-all" }}
            >
              <Descriptions.Item label="Zone">
                {detail.zone ?? "—"}
                {typeof detail.meta.declared === "string" && (
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {" "}（规格里声明为 {String(detail.meta.declared)}，管理口在 {detail.zone}）
                  </Text>
                )}
              </Descriptions.Item>
              <Descriptions.Item label="角色">
                {ROLE_LABEL[detail.role ?? ""] ?? detail.role ?? "—"}
              </Descriptions.Item>
              <Descriptions.Item label="CT">{detail.vmid ?? "—"}</Descriptions.Item>
              <Descriptions.Item label="状态">{detail.status ?? "—"}</Descriptions.Item>
              <Descriptions.Item label="IPv4" span={2}>
                <span className="mono">{detail.ipv4 ?? "—"}</span>
              </Descriptions.Item>
              <Descriptions.Item label="配置" span={2}>
                {String(detail.meta.cores ?? "?")}C / {String(detail.meta.memory ?? "?")}M /{" "}
                {String(detail.meta.disk ?? "?")}G
              </Descriptions.Item>
              <Descriptions.Item label="规格" span={2}>
                {String(detail.meta.spec ?? "—")}
              </Descriptions.Item>
            </Descriptions>

            <Space wrap>
              <Button
                size="small"
                icon={<ThunderboltOutlined />}
                disabled={busy || !detail.vmid}
                onClick={() => power("start")}
              >
                启动
              </Button>
              <Button size="small" disabled={busy || !detail.vmid} onClick={() => power("stop")}>
                停止
              </Button>
              <Button size="small" disabled={busy || !detail.vmid} onClick={() => power("reboot")}>
                重启
              </Button>
              <Button
                size="small"
                onClick={() =>
                  window.open(terminalUrl(detail.zone!, detail.label), "_blank", "noopener")
                }
              >
                打开终端
              </Button>
              <Button
                size="small"
                onClick={() =>
                  setCfg({
                    spec: String((detail.meta as Record<string, unknown>).spec ?? ""),
                    zone: detail.zone!,
                    host: detail.label,
                    cores: Number((detail.meta as Record<string, unknown>).cores ?? 0) || undefined,
                    memory: Number((detail.meta as Record<string, unknown>).memory ?? 0) || undefined,
                    disk: Number((detail.meta as Record<string, unknown>).disk ?? 0) || undefined,
                  })
                }
                disabled={!detail.vmid}
              >
                改配置
              </Button>
            </Space>

            <Space wrap>
              {detail.retired ? (
                <Button size="small" loading={busy} onClick={() => retire(true)}>
                  恢复（revive）
                </Button>
              ) : (
                <Popconfirm
                  title="废弃这台主机？"
                  description="停机但保留 IP、磁盘与配置，随时可以恢复。"
                  okText="废弃"
                  onConfirm={() => retire(false)}
                >
                  <Button size="small" danger loading={busy}>
                    废弃（retire）
                  </Button>
                </Popconfirm>
              )}
            </Space>

            {detail.role === "monitor" && (
              <MonitorNetworks
                monitor={detail}
                networks={networks}
                edges={edges}
                mirrorRows={rows}
                onRefresh={() => mirrors.refetch()}
              />
            )}
          </Space>
        )}
      </Drawer>

      <HostConfigEditor target={cfg} onClose={() => setCfg(null)} />

      <Modal
        open={pubOpen}
        title={`给 ${pickedZone ?? ""} 添加端口映射`}
        onCancel={() => setPubOpen(false)}
        onOk={() => pubForm.submit()}
        confirmLoading={busy}
        okText="添加"
        destroyOnClose
      >
        <Form form={pubForm} layout="vertical" onFinish={addPub} preserve={false}>
          <Form.Item name="proto" label="协议" initialValue="tcp">
            <Select options={[{ value: "tcp" }, { value: "udp" }]} />
          </Form.Item>
          <Form.Item name="port" label="宿主端口" rules={[{ required: true }]}>
            <InputNumber min={1} max={65535} style={{ width: "100%" }} />
          </Form.Item>
          <Form.Item
            name="to"
            label="转到（IP:端口）"
            rules={[{ required: true }]}
            extra="可以只写 IP，端口默认跟宿主端口一样"
          >
            <Input placeholder="10.77.0.20:7681" />
          </Form.Item>
        </Form>
      </Modal>

      {/* 镜像总账：谁抓了谁。入口在网络右键"整网监控"与监控机抽屉里。 */}
      <Card
        size="small"
        title={
          <Space size={8}>
            <span>流量镜像</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              源主机网口的双向流量复制到监控机的抓包口
            </Text>
            {reconcile && <ReconcileBadge info={reconcile} />}
          </Space>
        }
        extra={
          <Tooltip title="规格改动后巡检 5 分钟内自动接线；点刷新立即重读">
            <Button size="small" icon={<ReloadOutlined />} onClick={refresh}>
              刷新
            </Button>
          </Tooltip>
        }
      >
        {rows.length === 0 && !mirrors.isLoading ? (
          <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有镜像连线" />
        ) : (
          <Table<LedgerRow>
            rowKey="key"
            size="small"
            dataSource={ledger}
            pagination={false}
            defaultExpandAllRows
            indentSize={18}
            columns={[
              { title: "监控机 / 网络 / 主机", dataIndex: "name" },
              {
                title: "抓包桥",
                dataIndex: "bridge",
                width: 130,
                render: (v?: React.ReactNode) => v ?? "",
              },
              { title: "网口", dataIndex: "iface", width: 140 },
              {
                title: "规格",
                dataIndex: "spec",
                width: 110,
                render: (v?: string) =>
                  v ? <Tag className="mono">{v}</Tag> : "",
              },
              {
                title: "状态",
                dataIndex: "active",
                width: 90,
                render: (v?: string) =>
                  v === undefined ? (
                    ""
                  ) : (
                    <Tooltip
                      title={
                        v === "active"
                          ? "宿主上采集单元运行中"
                          : v === "failed"
                            ? "采集单元已安装但运行失败，巡检/重启后会自动重试"
                            : v === "missing"
                              ? "宿主上还没有这个实例（容器未建或等巡检补装）"
                              : "单元已安装但未运行"
                      }
                    >
                      <Tag color={v === "active" ? "green" : v === "failed" ? "red" : "default"}>
                        {v === "active"
                          ? "采集中"
                          : v === "failed"
                            ? "失败"
                            : v === "missing"
                              ? "未安装"
                              : "停止"}
                      </Tag>
                    </Tooltip>
                  ),
              },
              {
                title: "",
                width: 50,
                render: (_: unknown, r) =>
                  r.row ? (
                    <Popconfirm
                      title="删除这条镜像？"
                      description="规格里会一并删掉，监控机上的采集实例也会停。"
                      okText="删除"
                      okButtonProps={{ danger: true }}
                      onConfirm={() => delMirror(r.row!)}
                    >
                      <Button size="small" danger type="text" icon={<DeleteOutlined />} />
                    </Popconfirm>
                  ) : (
                    ""
                  ),
              },
            ]}
          />
        )}
      </Card>
    </Space>
  );
}

/** 后台巡检状态的小徽标：上次对账时间与结果。 */
function ReconcileBadge({ info }: { info: ReconcileInfo }) {
  if (info.error) {
    return (
      <Tooltip title={info.error}>
        <Tag color="red">巡检出错</Tag>
      </Tooltip>
    );
  }
  if (info.skipped) {
    return (
      <Tooltip title={info.skipped}>
        <Tag>巡检跳过</Tag>
      </Tooltip>
    );
  }
  if (!info.at) return null;
  const t = info.at.replace(/^.*T/, "").replace(/:\d\d$/, "");
  const delta =
    info.applied || info.removed
      ? `（补 ${info.applied} / 清 ${info.removed}）`
      : "";
  return (
    <Tooltip title={`后台每 5 分钟对账一次：规格说该有的镜像漏了自动补装，孤儿单元自动清理`}>
      <Tag color="default">
        巡检 {t}
        {delta}
      </Tag>
    </Tooltip>
  );
}

/** 监控机的采集清单（**只读**）：列出它正在采集哪些网络、每个网络的逐口现状。
 *
 * 开关/编辑不在这里做——那是"网络"的事，在网络节点的抽屉里操作（开/关/
 * 换绑监控机）。这里只回答"这台监控机眼下在看什么、看得见吗"。
 */
function MonitorNetworks({
  monitor,
  networks,
  edges,
  mirrorRows,
  onRefresh,
}: {
  monitor: TopoNode;
  networks: TopoNode[];
  edges: TopoEdge[];
  /** 全部镜像连线（带运行状态）：展开显示每个网络里"哪些口在抓、哪些没抓" */
  mirrorRows: MirrorRow[];
  /** 重新拉取镜像现状（宿主侧 systemctl 状态） */
  onRefresh: () => void;
}) {
  /** 这个网络上现在有几台被抓 + 落在哪块抓包口 */
  const captureInfo = (net: TopoNode) => {
    const lines = edges.filter(
      (e) => e.kind === "mirror" && e.to === monitor.id && e.from.startsWith(`host:${net.zone}:`),
    );
    if (lines.length === 0) return null;
    return {
      count: lines.length,
      iface: lines[0].target_iface ?? "?",
      bridge: lines[0].capture_bridge ?? "",
    };
  };

  return (
    <Card
      size="small"
      title="采集的网络"
      extra={
        <Button
          size="small"
          icon={<ReloadOutlined />}
          onClick={onRefresh}
          title="重新对账宿主上各采集实例的运行状态"
        >
          刷新
        </Button>
      }
    >
      {networks.length === 0 ? (
        <Text type="secondary" style={{ fontSize: 12 }}>
          还没有任何内网网络。
        </Text>
      ) : (
        <div className="mon-list">
          {networks.map((net) => {
            const m = net.meta as Record<string, unknown>;
            const monitored = typeof m.monitor === "string" && m.monitor !== "";
            const mine = m.monitor === monitor.label;
            const info = captureInfo(net);
            // 这台监控机在这个网络里逐主机的采集现状（来自 /api/mirrors 的
            // 宿主侧状态，不是图上的意图；行 from 是 `zone:host` 格式）
            const hostRows = mine
              ? mirrorRows.filter(
                  (r) => r.to === `${monitor.zone}:${monitor.label}` && r.from.startsWith(`${net.zone}:`),
                )
              : [];
            return (
              <div
                key={net.id}
                className={`mon-card${mine ? " mon-card-mine" : ""}${monitored && !mine ? " mon-card-other" : ""}`}
              >
                <div className="mon-head">
                  <span className="mono mon-name">{net.label}</span>
                  <span className="mono mon-cidr">{String(m.cidr ?? "—")}</span>
                  <span className="mon-flex" />
                  {monitored ? (
                    mine ? (
                      <Tag color="blue" style={{ marginInlineEnd: 0 }}>
                        本机采集
                      </Tag>
                    ) : (
                      <Tag style={{ marginInlineEnd: 0 }}>归 {String(m.monitor)}</Tag>
                    )
                  ) : (
                    <Tag style={{ marginInlineEnd: 0 }}>未采集</Tag>
                  )}
                </div>
                <div className="mon-meta mono">
                  {info ? (
                    <>
                      抓包口 {info.iface}
                      {info.bridge ? ` · ${info.bridge}` : ""} · 已抓 {info.count} 台
                    </>
                  ) : monitored && !mine ? (
                    "由另一台监控机采集"
                  ) : (
                    "没有镜像连线"
                  )}
                </div>
                {hostRows.length > 0 && (
                  <div className="mon-hosts">
                    {hostRows.map((r) => {
                      // r.from 形如 `lab:gw`；只取主机名，别把 zone 前缀带上
                      const host = r.from.split(":").slice(-1)[0];
                      const ok = r.active === "active";
                      const word = ok
                        ? "采集中"
                        : r.active === "failed"
                          ? "失败"
                          : r.active === "missing"
                            ? "未安装"
                            : "停止";
                      return (
                        <div className="mon-host" key={r.from}>
                          <span className={`mon-dot mon-dot-${ok ? "on" : r.active ?? "off"}`} />
                          <span className="mono mon-host-name">{host}</span>
                          <span className="mono mon-host-link">
                            {r.source_iface ?? "eth0"} → {r.target_iface ?? "?"}
                          </span>
                          <span className={`mon-host-state${ok ? "" : " mon-host-bad"}`}>{word}</span>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })}
          <Text type="secondary" style={{ fontSize: 11 }}>
            这份清单是只读的。要开/关某个网络的采集、换绑监控机，去点对应的
            网络节点，在它的抽屉里操作；新加入网络的主机由后台巡检自动接线。
          </Text>
        </div>
      )}
    </Card>
  );
}

/** 网络节点的抽屉：网段配置、网关出网/映射/VPN、流量监控的开关与现状。 */
function NetworkDetail({
  node,
  gw,
  loading,
  busy,
  monitors,
  mirrorRows,
  onAfterChange,
  onRefreshMirrors,
  onEgress,
  onAddPub,
  onDelPub,
}: {
  node: TopoNode;
  gw?: GatewayStatus;
  loading: boolean;
  busy: boolean;
  /** 图上的全部监控机：开启/换绑用 */
  monitors: TopoNode[];
  /** 全部镜像连线（带宿主上的运行状态），用于本网络的逐口对账 */
  mirrorRows: MirrorRow[];
  /** 改了规格类配置（监控/VPN）后刷新图与总账 */
  onAfterChange: (msg: string) => void;
  onRefreshMirrors: () => void;
  onEgress: (on: boolean) => void;
  onAddPub: () => void;
  onDelPub: (idx: number) => void;
}) {
  const [monPending, setMonPending] = useState(false);
  const [vpnPending, setVpnPending] = useState<string | null>(null);
  const { message } = AntApp.useApp();
  const m = node.meta as Record<string, unknown>;
  const external = m.external === true;
  const pubs = gw?.pubs ?? [];
  const zone = node.zone ?? "";

  /** 本网络各主机的镜像现状：源主机 → 实例状态。规格说该抓（有边）但
   * 宿主上单元不在/挂了，就标红——"哪些口镜像了哪些没有"一眼可见。
   * 注意 /api/mirrors 行的 from 是 `zone:host`（拓扑边才是 host:zone:host）。 */
  const zoneMirrors = mirrorRows.filter((r) => r.from.startsWith(`${zone}:`));
  const monitorName = typeof m.monitor === "string" ? m.monitor : "";

  /** 开/关/换绑整网监控。开启默认选第一台监控机，也可以从下拉里换。 */
  const switchMonitor = async (on: boolean, use?: string) => {
    setMonPending(true);
    try {
      await api.setMonitor(zone, on, use ? { use } : undefined);
      onAfterChange(
        on
          ? `已开启 ${node.label} 的整网流量监控${use ? `（→ ${use}）` : ""}，抓包口安装中`
          : `已关闭 ${node.label} 的流量监控`,
      );
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setMonPending(false);
    }
  };

  /** VPN 开关。写规格 + 走 gateway 变更流程，和编排页是同一条路。 */
  const toggleVpn = async (proto: "wireguard" | "openvpn", on: boolean) => {
    setVpnPending(proto);
    try {
      await api.setVpn(zone, proto, on ? "on" : "off");
      onAfterChange(`${proto === "wireguard" ? "WireGuard" : "OpenVPN"} 已${on ? "开启" : "关闭"}`);
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setVpnPending(null);
    }
  };

  const monitorOptions = monitors
    .filter((x) => !(x.zone === node.zone && x.label === monitorName))
    .map((x) => ({ value: `${x.zone}:${x.label}`, label: `${x.zone} / ${x.label}` }));

  return (
    <Space direction="vertical" size={14} style={{ width: "100%" }}>
      <Descriptions size="small" column={1} bordered>
        <Descriptions.Item label="网段">
          <span className="mono">{String(m.cidr ?? "—")}</span>
        </Descriptions.Item>
        <Descriptions.Item label="网桥">
          <span className="mono">{String(m.bridge ?? "—")}</span>
        </Descriptions.Item>
        {!external && (
          <>
            <Descriptions.Item label="上联">
              {m.uplink ? String(m.uplink) : "外网"}
              {m.uplink_bridge ? (
                <Text type="secondary" className="mono">
                  {" "}
                  · {String(m.uplink_bridge)}
                </Text>
              ) : null}
            </Descriptions.Item>
            <Descriptions.Item label="网关主机">
              {String(m.gateway_host ?? "—")}
            </Descriptions.Item>
            <Descriptions.Item label="宿主地址">
              <span className="mono">{String(m.mgmt_ip ?? "—")}</span>
            </Descriptions.Item>
          </>
        )}
      </Descriptions>

      {external ? (
        <Text type="secondary" style={{ fontSize: 12 }}>
          外网桥由 PVE 提供，平台只往上接网关，不改它的配置。
        </Text>
      ) : (
        <>
          <Card size="small" title="VPN">
            <Space direction="vertical" size={8} style={{ width: "100%" }}>
              {(["wireguard", "openvpn"] as const).map((proto) => {
                const v = gw?.vpn;
                const enabled = proto === "wireguard" ? !!v?.wireguard : !!v?.openvpn;
                const active = proto === "wireguard" ? !!v?.wireguard_active : !!v?.openvpn_active;
                const label = proto === "wireguard" ? "WireGuard" : "OpenVPN";
                return (
                  <div key={proto} className="mon-net-row">
                    <Space size={6}>
                      <span style={{ fontSize: 13 }}>{label}</span>
                      {enabled ? (
                        <Tag color={active ? "green" : "red"}>
                          {active ? "运行中" : "已启用 · 未运行"}
                        </Tag>
                      ) : (
                        <Tag>未开启</Tag>
                      )}
                    </Space>
                    <Space size={6}>
                      {enabled && (
                        <a href={vpnClientUrl(zone, proto)} target="_blank" rel="noreferrer">
                          <Button size="small">下载配置</Button>
                        </a>
                      )}
                      <Button
                        size="small"
                        danger={enabled}
                        loading={vpnPending === proto}
                        disabled={busy}
                        onClick={() => toggleVpn(proto, !enabled)}
                      >
                        {enabled ? "关闭" : "开启"}
                      </Button>
                    </Space>
                  </div>
                );
              })}
              {!!gw?.vpn?.clients?.length && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  WireGuard 客户端 {gw.vpn.clients.length} 个，配置在网关的抽屉里也可逐个下载。
                </Text>
              )}
              <Text type="secondary" style={{ fontSize: 11 }}>
                开关写回规格并立即下发；隧道密钥都存这台网关自己的容器里。
              </Text>
            </Space>
          </Card>

          <Card
            size="small"
            title="流量监控"
            extra={
              monitorName ? (
                <Button
                  size="small"
                  icon={<ReloadOutlined />}
                  onClick={onRefreshMirrors}
                  loading={busy}
                  title="重新对账：规格意图 vs 宿主上真实在跑的采集实例"
                >
                  刷新
                </Button>
              ) : undefined
            }
          >
            <Space direction="vertical" size={6} style={{ width: "100%" }}>
              <div className="mon-net-row">
                <Space size={8}>
                  <Switch
                    size="small"
                    checked={!!monitorName}
                    loading={monPending}
                    disabled={busy}
                    onChange={(on) => switchMonitor(on)}
                  />
                  {monitorName ? (
                    <Tag color="blue">监控机 {monitorName}</Tag>
                  ) : (
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      未开启——开启后本网段全部主机的接口都镜像给监控机
                    </Text>
                  )}
                </Space>
              </div>

              {monitorName && monitorOptions.length > 0 && (
                <div className="mon-net-row">
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    换一台监控机
                  </Text>
                  <Select
                    size="small"
                    style={{ width: 200 }}
                    placeholder="选择其他监控机"
                    options={monitorOptions}
                    onChange={(v) => switchMonitor(true, v)}
                  />
                </div>
              )}

              {monitorName && zoneMirrors.length > 0 && (
                <>
                  {zoneMirrors.map((r) => {
                    const host = r.from.replace(/^host:[^:]+:/, "");
                    const ok = r.active === "active";
                    return (
                      <div key={`${r.from}->${r.to}`} className="mon-net-row">
                        <span className="mono" style={{ fontSize: 12 }}>
                          {host} · {r.source_iface ?? "eth0"} → {r.target_iface ?? "?"}
                        </span>
                        <Tag color={ok ? "green" : r.active === "failed" ? "red" : "default"}>
                          {ok ? "采集中" : r.active === "failed" ? "失败" : "未安装"}
                        </Tag>
                      </div>
                    );
                  })}
                </>
              )}

              {!monitorName && monitors.length === 0 && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  环境里还没有监控机：去「编排」页给任意网络打开"流量监控-新建"创建一台，
                  然后回到这里开启。
                </Text>
              )}
              <Text type="secondary" style={{ fontSize: 11 }}>
                后台巡检每 5 分钟自动对账：缺的补装、挂的拉起、孤儿清理。逐台挑主机抓取在「编排」页配置。
              </Text>
            </Space>
          </Card>

          <Card
            size="small"
            title="出网"
            extra={
              <Space size={6}>
                <Tag color={m.egress ? "green" : "default"}>{m.egress ? "已开启" : "已关闭"}</Tag>
                <Button size="small" loading={busy} onClick={() => onEgress(!m.egress)}>
                  {m.egress ? "关闭" : "开启"}
                </Button>
              </Space>
            }
          >
            <Text type="secondary" style={{ fontSize: 12 }}>
              开 = 网关转发 + NAT，本网段主机可访问上游/外网；关 = 立即断掉外部访问
              （规则插在 established 之前，不等超时）。
            </Text>
          </Card>

          <Card
            size="small"
            title="端口映射"
            extra={
              <Button size="small" icon={<PlusOutlined />} onClick={onAddPub} loading={loading}>
                添加
              </Button>
            }
          >
            {pubs.length === 0 ? (
              <Text type="secondary" style={{ fontSize: 12 }}>
                没有映射。宿主端口要进来，就在这里加一条。
              </Text>
            ) : (
              <Table
                size="small"
                rowKey={(r) => `${r.proto}:${r.port}`}
                dataSource={pubs}
                pagination={false}
                columns={[
                  {
                    title: "映射",
                    render: (_, r) => (
                      <span className="mono">
                        {r.proto}/{r.port} → {r.to}
                      </span>
                    ),
                  },
                  {
                    title: "",
                    width: 40,
                    render: (_, r) => {
                      const idx = pubs.findIndex(
                        (p) => p.proto === r.proto && p.port === r.port && p.to === r.to,
                      );
                      return (
                        <Button
                          size="small"
                          danger
                          type="text"
                          icon={<DeleteOutlined />}
                          onClick={() => onDelPub(idx)}
                        />
                      );
                    },
                  },
                ]}
              />
            )}
          </Card>
        </>
      )}
    </Space>
  );
}
