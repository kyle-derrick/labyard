/** 拓扑页：一张图看完整环境，点节点直接改。
 *
 * 设计上只有一件事要守住：**图是入口，不是截图**。看到网关的"出网"是关的，
 * 就在这张图上打开；看到某台机停了，就在这里起来；想把几台主机的流量交给
 * 监控机，就框选然后右键。改结构（加 zone、加主机）仍然要去「编排」，
 * 因为那是在改文件，不是在改运行态。
 *
 * 镜像的添加入口从底部的表单挪到了右键菜单：底部表单只解决了"加一条"，
 * 而真正常见的是"这几台一起加"，框选比在下拉框里一个个挑快得多。
 * 底部表格保留，作为"现在到底抓了谁"的总账。
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
import { api, type GatewayStatus, type MirrorRow, type TopoNode } from "./api";

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

  const hosts = useMemo(() => nodes.filter((n) => n.kind === "host"), [nodes]);
  const monitors = useMemo(() => hosts.filter((n) => n.role === "monitor"), [hosts]);
  /** 网关面板：只有打开网络节点的抽屉时才查，避免每次进页面都打一次 pvesh */
  const pickedZone = picked?.kind === "network" && picked.zone ? picked.zone : null;
  const gw = useQuery({
    queryKey: ["gateway", pickedZone ?? ""],
    queryFn: () => api.gateway(pickedZone as string),
    enabled: !!pickedZone,
    retry: 0,
  });

  /** 当前在多选里的主机（排除监控机自身：它不能抓自己） */
  const selectedHosts = useMemo(
    () =>
      selected
        .map((id) => nodes.find((n) => n.id === id))
        .filter((n): n is TopoNode => !!n && n.kind === "host" && n.role !== "monitor"),
    [nodes, selected],
  );

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

  const addMirrors = async (target: TopoNode) => {
    if (selectedHosts.length === 0) return;
    setBusy(true);
    let ok = 0;
    const fail: string[] = [];
    for (const h of selectedHosts) {
      try {
        await api.addMirror(`${h.zone}:${h.label}`, `${target.zone}:${target.label}`, spec);
        ok += 1;
      } catch (e) {
        fail.push(`${h.label}: ${(e as Error).message}`);
      }
    }
    setBusy(false);
    setMenu(null);
    if (ok) afterChange(`已把 ${ok} 台主机的流量交给 ${target.label}`);
    if (fail.length) message.warning(fail.join("；"));
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
  const menuIsHost = menuNode?.kind === "host";
  const targets = menuIsHost && !selected.includes(menuNode!.id) ? [menuNode!] : selectedHosts;

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
          window.open(
            `/terminal/${encodeURIComponent(menuNode.zone!)}/${encodeURIComponent(menuNode.label)}/`,
            "_blank",
            "noopener",
          );
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
              {hosts.length} 主机 · 点节点看详情，悬停监控机看采集链路，框选后右键采集
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
            onSelect={setSelected}
            onContextMenu={(e, n) => setMenu({ x: e.clientX, y: e.clientY, node: n })}
          />
        )}

        <div className="topo-legend">
          <span className="lg">
            <b style={{ background: "#eef2ff", borderColor: "#8b5cf6", borderStyle: "dashed" }} />
            外网
          </span>
          <span className="lg">
            <b style={{ background: "#faf5ff", borderColor: "#a78bfa", borderStyle: "dashed" }} />
            内网（zone）
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
          {targets.length > 0 && monitors.length > 0 && (
            <>
              <div className="ctx-sep" />
              <div className="ctx-title">
                {targets.length > 1
                  ? `把 ${targets.length} 台的流量交给…`
                  : "把这台的流量交给…"}
              </div>
              {monitors.map((a) => (
                <div
                  key={a.id}
                  className="ctx-item"
                  onClick={() => addMirrors(a)}
                  title={`${a.zone} / ${a.label}`}
                >
                  → {a.zone} / {a.label}
                </div>
              ))}
            </>
          )}
        </div>
      )}

      {/* 详情抽屉：网络与主机两种 */}
      <Drawer
        open={!!detail}
        onClose={() => setPicked(null)}
        width={430}
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
            <Descriptions size="small" column={2} bordered>
              <Descriptions.Item label="Zone">{detail.zone ?? "—"}</Descriptions.Item>
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
                  window.open(
                    `/terminal/${encodeURIComponent(detail.zone!)}/${encodeURIComponent(detail.label)}/`,
                    "_blank",
                    "noopener",
                  )
                }
              >
                打开终端
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
              <Card size="small" title="抓到的流量">
                {edges.filter((e) => e.kind === "mirror" && e.to === detail.id).length === 0 ? (
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    还没有采集连线。框选几台主机 → 右键 → 把流量交给它；或在编排里打开整网监控。
                  </Text>
                ) : (
                  <Space direction="vertical" size={4}>
                    {edges
                      .filter((e) => e.kind === "mirror" && e.to === detail.id)
                      .map((e) => (
                        <div key={`${e.from}`}>
                          <Text className="mono">{e.from.replace(/^host:/, "")}</Text>
                          <Text type="secondary" className="mono">
                            {" "}
                            · {e.source_iface ?? "eth0"} → {e.target_iface ?? "?"}
                            {e.capture_bridge ? ` @ ${e.capture_bridge}` : ""}
                          </Text>
                        </div>
                      ))}
                  </Space>
                )}
              </Card>
            )}
          </Space>
        )}
      </Drawer>

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

      {/* 镜像总账：谁抓了谁。添加入口在右键菜单里。 */}
      <Card
        size="small"
        title={
          <Space size={8}>
            <span>流量镜像</span>
            <Text type="secondary" style={{ fontSize: 12, fontWeight: 400 }}>
              源主机接口的双向流量复制到监控机的抓包口
            </Text>
          </Space>
        }
        extra={
          <Button size="small" icon={<PlusOutlined />} onClick={() => setMenu(null)} disabled>
            在图里框选后右键添加
          </Button>
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
              width: 110,
              render: (v: string) => <Tag className="mono">{v}</Tag>,
            },
            {
              title: "源主机",
              dataIndex: "from",
              render: (v: string, r) => (
                <code>
                  {v}
                  {r.source_iface ? ` · ${r.source_iface}` : ""}
                </code>
              ),
            },
            {
              title: "监控机",
              dataIndex: "to",
              render: (v: string, r) => (
                <code>
                  {v}
                  {r.target_iface ? ` · ${r.target_iface}` : ""}
                </code>
              ),
            },
            {
              title: "抓包桥",
              dataIndex: "capture_bridge",
              width: 130,
              render: (v: string | undefined) =>
                v ? <span className="mono">{v}</span> : <span className="mono">{""}</span>,
            },
            {
              title: "",
              width: 50,
              render: (_: unknown, r: MirrorRow) => (
                <Popconfirm
                  title="删除这条镜像？"
                  description="规格里会一并删掉，监控机上的采集实例也会停。"
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
      </Card>
    </Space>
  );
}

/** 网络节点的抽屉：这个网段怎么配的、网关开着什么。 */
function NetworkDetail({
  node,
  gw,
  loading,
  busy,
  onEgress,
  onAddPub,
  onDelPub,
}: {
  node: TopoNode;
  gw?: GatewayStatus;
  loading: boolean;
  busy: boolean;
  onEgress: (on: boolean) => void;
  onAddPub: () => void;
  onDelPub: (idx: number) => void;
}) {
  const m = node.meta as Record<string, unknown>;
  const external = m.external === true;
  const pubs = gw?.pubs ?? [];

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
            <Descriptions.Item label="NAT">{m.nat ? "开" : "关"}</Descriptions.Item>
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
              关闭后这个网段的主机立刻断掉外部访问（规则插在 established 之前，不等超时）。
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
