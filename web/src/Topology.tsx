import { Empty, Tag, Tooltip, Typography } from "antd";
import { ArrowDownOutlined, ClusterOutlined, SwapOutlined } from "@ant-design/icons";

const { Text } = Typography;

/** 与后端 `ResolvedZone` 对应的最小类型。 */
export type Host = {
  name: string;
  role: "gateway" | "monitor" | "server" | "client";
  vmid: number | null;
  cores: number;
  memory: number;
  ipv4: string;
  ipv6_prefix: string | null;
  provisioner: string | null;
  mirror: "none" | "source" | "target";
};

export type Zone = {
  name: string;
  bridge: string;
  uplink: string | null;
  uplink_bridge: string | null;
  v4_cidr: string;
  v6_prefix: string | null;
  nat: boolean;
  hosts: Host[];
};

const ROLE_LABEL: Record<Host["role"], string> = {
  gateway: "网关",
  monitor: "监控",
  server: "服务主机",
  client: "终端主机",
};

export default function Topology({ zones }: { zones: Zone[] }) {
  if (!zones?.length) {
    return <Empty description="先点「预览计划」" image={Empty.PRESENTED_IMAGE_SIMPLE} />;
  }

  // 按 uplink 关系排序：wan 在前，其余按依赖深度
  const depth = (z: Zone, all: Zone[], guard = 0): number => {
    if (!z.uplink || guard > 8) return 0;
    const up = all.find((x) => x.name === z.uplink);
    return up ? 1 + depth(up, all, guard + 1) : 0;
  };
  const sorted = [...zones].sort((a, b) => depth(a, zones) - depth(b, zones));

  return (
    <div>
      {sorted.map((z, idx) => (
        <div key={z.name}>
          <div className="topology-zone">
            <div
              style={{
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
                marginBottom: 8,
              }}
            >
              <div>
                <ClusterOutlined style={{ marginRight: 6 }} />
                <Text strong>{z.name}</Text>
                <Tag style={{ marginLeft: 8 }} className="mono">
                  {z.bridge}
                </Tag>
                <Tag color="default" className="mono">
                  {z.v4_cidr}
                </Tag>
                {z.v6_prefix && (
                  <Tag color="purple" className="mono">
                    {z.v6_prefix}
                  </Tag>
                )}
                {z.nat && <Tag color="orange">NAT44</Tag>}
              </div>
              <Text type="secondary" style={{ fontSize: 12 }}>
                {z.uplink ? `上联 → ${z.uplink_bridge}` : "上联 → WAN"}
              </Text>
            </div>

            <div
              style={{
                display: "grid",
                gridTemplateColumns: "repeat(auto-fill, minmax(200px, 1fr))",
                gap: 8,
              }}
            >
              {z.hosts.map((h) => (
                <Tooltip
                  key={h.name}
                  title={
                    <span className="mono">
                      vmid {h.vmid ?? "?"} · {h.cores}C / {h.memory}MB
                      {h.provisioner ? ` · ${h.provisioner}` : ""}
                    </span>
                  }
                >
                  <div className={`host-chip ${h.role}`}>
                    <span>
                      <Text strong style={{ fontSize: 12 }}>
                        {h.name}
                      </Text>
                      <Text type="secondary" style={{ marginLeft: 6, fontSize: 11 }}>
                        {ROLE_LABEL[h.role]}
                      </Text>
                    </span>
                    <span className="mono" style={{ color: "#64748b" }}>
                      {h.ipv4.split("/")[0]}
                      {h.mirror !== "none" && (
                        <SwapOutlined
                          style={{ marginLeft: 6, color: h.mirror === "source" ? "#16a34a" : "#d97706" }}
                        />
                      )}
                    </span>
                  </div>
                </Tooltip>
              ))}
            </div>
          </div>

          {idx < sorted.length - 1 && (
            <div style={{ textAlign: "center", padding: "6px 0", color: "#94a3b8" }}>
              {sorted[idx + 1].uplink === z.name ? <ArrowDownOutlined /> : null}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
