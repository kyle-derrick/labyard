import { Suspense, lazy, useCallback, useEffect, useState } from "react";
import { Layout, Menu, Button, Card, Space, Typography, App as AntApp } from "antd";
import {
  ApartmentOutlined,
  DashboardOutlined,
  ExperimentOutlined,
  FileTextOutlined,
  HistoryOutlined,
  LogoutOutlined,
  CloudServerOutlined,
} from "@ant-design/icons";
import Login from "./Login";
import Dashboard from "./Dashboard";
import Editor from "./Editor";
import Hosts from "./Hosts";
import Runs from "./Runs";
import Specs from "./Specs";
import { api } from "./api";

// 拓扑图把 react-flow + dagre 一起拖进来（约 300 KB gzip）。它只在一个页面用到，
// 所以按需加载——首屏不该为一张可能不打开的图付代价。
const TopologyPage = lazy(() => import("./TopologyPage"));

const { Header, Content } = Layout;
const { Text } = Typography;

type Page = "dashboard" | "editor" | "specs" | "topology" | "hosts" | "runs";

export default function App() {
  const { message } = AntApp.useApp();
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [page, setPage] = useState<Page>("editor");

  // 启动时探一次登录态；401 就进登录页
  useEffect(() => {
    api
      .me()
      .then(() => setAuthed(true))
      .catch(() => setAuthed(false));
  }, []);

  const onLogin = useCallback(() => setAuthed(true), []);

  const logout = async () => {
    try {
      await api.logout();
    } finally {
      setAuthed(false);
      message.info("已退出");
    }
  };

  if (authed === null) {
    return null;
  }
  if (!authed) {
    return <Login onOk={onLogin} />;
  }

  return (
    <Layout style={{ minHeight: "100%" }}>
      <Header
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          borderBottom: "1px solid var(--border)",
          paddingInline: 20,
          height: 56,
        }}
      >
        <Space size={24}>
          <Text strong style={{ fontSize: 16, letterSpacing: 0.3 }}>
            labyard
          </Text>
          <Menu
            mode="horizontal"
            selectedKeys={[page]}
            onClick={(e) => setPage(e.key as Page)}
            style={{ borderBottom: "none", minWidth: 420 }}
            items={[
              {
                key: "editor",
                icon: <ExperimentOutlined />,
                label: "编排",
              },
              {
                key: "specs",
                icon: <FileTextOutlined />,
                label: "规格",
              },
              {
                key: "topology",
                icon: <ApartmentOutlined />,
                label: "拓扑",
              },
              {
                key: "hosts",
                icon: <CloudServerOutlined />,
                label: "主机",
              },
              {
                key: "runs",
                icon: <HistoryOutlined />,
                label: "运行历史",
              },
              {
                key: "dashboard",
                icon: <DashboardOutlined />,
                label: "概览",
              },
            ]}
          />
        </Space>
        <Button icon={<LogoutOutlined />} size="small" onClick={logout}>
          退出
        </Button>
      </Header>

      <Content style={{ padding: 20, maxWidth: 1600, width: "100%", margin: "0 auto" }}>
        {page === "editor" && <Editor />}
        {page === "specs" && <Specs />}
        {page === "topology" && (
          <Suspense fallback={<Card loading style={{ minHeight: 320 }} />}>
            <TopologyPage />
          </Suspense>
        )}
        {page === "hosts" && <Hosts />}
        {page === "runs" && <Runs />}
        {page === "dashboard" && <Dashboard />}
      </Content>
    </Layout>
  );
}
