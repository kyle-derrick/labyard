import { useCallback, useEffect, useState } from "react";
import { Layout, Menu, Button, Space, Typography, App as AntApp } from "antd";
import {
  DashboardOutlined,
  ExperimentOutlined,
  HistoryOutlined,
  LogoutOutlined,
  CloudServerOutlined,
} from "@ant-design/icons";
import Login from "./Login";
import Dashboard from "./Dashboard";
import Editor from "./Editor";
import Hosts from "./Hosts";
import Runs from "./Runs";
import { api } from "./api";

const { Header, Content } = Layout;
const { Text } = Typography;

type Page = "dashboard" | "editor" | "hosts" | "runs";

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
            style={{ borderBottom: "none", minWidth: 260 }}
            items={[
              {
                key: "editor",
                icon: <ExperimentOutlined />,
                label: "编排",
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
        {page === "hosts" && <Hosts />}
        {page === "runs" && <Runs />}
        {page === "dashboard" && <Dashboard />}
      </Content>
    </Layout>
  );
}
