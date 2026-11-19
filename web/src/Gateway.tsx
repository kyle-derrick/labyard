/** 网关面板：出网开关、端口映射、VPN。
 *
 * 所有改动都写回规格文件（后端负责改 YAML 并立刻重跑该 zone 的 gateway 步骤），
 * 这里只做"显示 + 触发"。没有另存一份运行态，页面刷新后看到的仍是真相。
 */
import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  App as AntApp,
  Button,
  Drawer,
  Empty,
  Form,
  Input,
  InputNumber,
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
  ApiOutlined,
  CloudDownloadOutlined,
  DeleteOutlined,
  LockOutlined,
  PlusOutlined,
  SafetyOutlined,
} from "@ant-design/icons";
import { api, vpnClientUrl, type GatewayStatus } from "./api";

const { Text } = Typography;

export default function GatewayPanel({
  zone,
  onClose,
}: {
  zone: string | null;
  onClose: () => void;
}) {
  const { message } = AntApp.useApp();
  const qc = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [form] = Form.useForm();

  const freshRef = useRef(false);
  const q = useQuery({
    queryKey: ["gateway", zone],
    queryFn: () => api.gateway(zone!, freshRef.current),
    enabled: !!zone,
    // 改动后要重跑 gateway 步骤，状态会变；面板开着时轮询比"让用户点刷新"好。
    // fresh 只在"点刷新/刚改完"的那一次置位，平时吃服务端 8 秒缓存——
    // run 占着 exec 闸时也不会让面板空白干等。
    refetchInterval: 6000,
  });
  const refetchFresh = () => {
    freshRef.current = true;
    q.refetch().finally(() => {
      freshRef.current = false;
    });
  };

  useEffect(() => {
    if (zone) form.resetFields();
  }, [zone, form]);

  const run = async (fn: () => Promise<unknown>, ok: string) => {
    setBusy(true);
    try {
      await fn();
      message.success(ok);
      // 立刻刷一次（步骤可能还在跑），并让主机/拓扑跟着更新
      setTimeout(() => refetchFresh(), 1200);
      qc.invalidateQueries({ queryKey: ["hosts"] });
      qc.invalidateQueries({ queryKey: ["topology"] });
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const g: GatewayStatus | undefined = q.data;

  const addPub = async (v: { proto: string; port: number; to: string }) => {
    if (!zone) return;
    await run(() => api.addPub(zone, v), "端口映射已写入规格并下发");
    form.resetFields();
  };

  return (
    <Drawer
      open={!!zone}
      onClose={onClose}
      width={640}
      destroyOnClose
      title={
        <Space>
          <SafetyOutlined />
          <span>网关策略 · {zone}</span>
          {g && <Tag className="mono">{g.gateway.host}</Tag>}
          {g && <Tag className="mono">{g.gateway.ipv4}</Tag>}
        </Space>
      }
      extra={
        <Button size="small" onClick={refetchFresh} loading={q.isFetching}>
          刷新
        </Button>
      }
    >
      {q.isError && (
        <Alert
          type="error"
          showIcon
          message="读取网关状态失败"
          description={<span className="mono">{(q.error as Error).message}</span>}
        />
      )}

      {g && (
        <Space direction="vertical" size={20} style={{ width: "100%" }}>
          <section>
            <Space align="center" size={12}>
              <Text strong>内网出网</Text>
              <Switch
                checked={g.egress}
                loading={busy}
                onChange={(on) =>
                  run(() => api.setEgress(zone!, on), on ? "已放开出网" : "已禁止出网")
                }
              />
              <Text type="secondary">
                {g.egress
                  ? "内网可以经网关访问外网（NAT 生效）"
                  : "转发链丢弃 内网→外网 的新连接，已建立的连接不受影响"}
              </Text>
            </Space>
            <div style={{ marginTop: 6 }}>
              <Text type="secondary" className="mono" style={{ fontSize: 12 }}>
                {g.v4_cidr} · NAT {g.nat ? "开" : "关"} · 上联 eth1
              </Text>
            </div>
          </section>

          <section>
            <Space style={{ marginBottom: 8 }}>
              <LockOutlined />
              <Text strong>端口映射</Text>
              <Text type="secondary" style={{ fontSize: 12 }}>
                外网 → 网关 → 内网主机（DNAT，nftables）
              </Text>
            </Space>
            <Table
              size="small"
              rowKey={(r) => r.spec}
              dataSource={g.pubs}
              pagination={false}
              locale={{
                emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有映射" />,
              }}
              columns={[
                {
                  title: "协议",
                  dataIndex: "proto",
                  width: 70,
                  render: (v: string) => <Tag className="mono">{v}</Tag>,
                },
                {
                  title: "对外端口",
                  dataIndex: "port",
                  width: 90,
                  render: (v: number) => <code>{v}</code>,
                },
                {
                  title: "内网目标",
                  dataIndex: "to",
                  render: (v: string) => <code>{v}</code>,
                },
                {
                  title: "",
                  width: 50,
                  render: (_: unknown, _r, i) => (
                    <Popconfirm
                      title="删除这条映射？"
                      onConfirm={() => run(() => api.delPub(zone!, i), "已删除")}
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
              style={{ marginTop: 10 }}
              initialValues={{ proto: "tcp" }}
              onFinish={addPub}
            >
              <Form.Item name="proto" style={{ marginBottom: 6 }}>
                <Select
                  style={{ width: 84 }}
                  options={[
                    { value: "tcp", label: "tcp" },
                    { value: "udp", label: "udp" },
                  ]}
                />
              </Form.Item>
              <Form.Item
                name="port"
                rules={[{ required: true, message: "对外端口" }]}
                style={{ marginBottom: 6 }}
              >
                <InputNumber placeholder="对外端口" min={1} max={65535} style={{ width: 110 }} />
              </Form.Item>
              <Form.Item
                name="to"
                rules={[{ required: true, message: "内网 ip:port" }]}
                style={{ marginBottom: 6 }}
              >
                <Input placeholder="10.10.1.20:22" style={{ width: 190 }} className="mono" />
              </Form.Item>
              <Form.Item style={{ marginBottom: 6 }}>
                <Button type="primary" htmlType="submit" icon={<PlusOutlined />} loading={busy}>
                  添加
                </Button>
              </Form.Item>
            </Form>
          </section>

          <section>
            <Space style={{ marginBottom: 8 }}>
              <ApiOutlined />
              <Text strong>VPN</Text>
              <Text type="secondary" style={{ fontSize: 12 }}>
                内核 wireguard / openvpn，客户端配置现读现拼
              </Text>
            </Space>

            <Space direction="vertical" size={10} style={{ width: "100%" }}>
              <Space size={12} wrap>
                <Text style={{ width: 90, display: "inline-block" }}>WireGuard</Text>
                <Switch
                  checked={g.vpn.wireguard}
                  loading={busy}
                  onChange={(on) =>
                    run(
                      () => api.setVpn(zone!, "wireguard", on ? "on" : "off"),
                      on ? "WireGuard 已开启（首次会生成密钥）" : "WireGuard 已关闭",
                    )
                  }
                />
                {g.vpn.wireguard && (
                  <Tag color={g.vpn.wireguard_active ? "green" : "orange"}>
                    {g.vpn.wireguard_active ? "wg0 运行中" : "wg0 未运行"}
                  </Tag>
                )}
                {g.vpn.wireguard && (
                  <Space size={4} wrap>
                    {g.vpn.clients.map((c) => (
                      <Tooltip key={c} title="下载客户端 .conf">
                        <Button
                          size="small"
                          icon={<CloudDownloadOutlined />}
                          href={vpnClientUrl(zone!, "wireguard", c)}
                        >
                          {c}.conf
                        </Button>
                      </Tooltip>
                    ))}
                  </Space>
                )}
              </Space>

              <Space size={12} wrap>
                <Text style={{ width: 90, display: "inline-block" }}>OpenVPN</Text>
                <Switch
                  checked={g.vpn.openvpn}
                  loading={busy}
                  onChange={(on) =>
                    run(
                      () => api.setVpn(zone!, "openvpn", on ? "on" : "off"),
                      on ? "OpenVPN 已开启（首次会生成 CA 与证书）" : "OpenVPN 已关闭",
                    )
                  }
                />
                {g.vpn.openvpn && (
                  <Tag color={g.vpn.openvpn_active ? "green" : "orange"}>
                    {g.vpn.openvpn_active ? "server 运行中" : "server 未运行"}
                  </Tag>
                )}
                {g.vpn.openvpn && (
                  <Space size={4} wrap>
                    {g.vpn.clients.map((c) => (
                      <Tooltip key={c} title="下载客户端 .ovpn">
                        <Button
                          size="small"
                          icon={<CloudDownloadOutlined />}
                          href={vpnClientUrl(zone!, "openvpn", c)}
                        >
                          {c}.ovpn
                        </Button>
                      </Tooltip>
                    ))}
                  </Space>
                )}
              </Space>

              <Text type="secondary" style={{ fontSize: 12 }}>
                客户端配置里的 Endpoint 是网关在上联口的地址。VPN 需要容器有
                <code> /dev/net/tun </code>，开启时平台会自动补上并重启一次容器。
              </Text>
            </Space>
          </section>

          {g.retired_hosts.length > 0 && (
            <Alert
              type="warning"
              showIcon
              message={`已废弃的主机：${g.retired_hosts.join("、")}`}
              description="它们仍占着 IP 与 vmid，但容器已停机。在「主机」页点「恢复」可原样回来。"
            />
          )}
        </Space>
      )}
    </Drawer>
  );
}
