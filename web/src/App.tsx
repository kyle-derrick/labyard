/** 应用骨架：登录态 + 顶部导航 + 页面路由（内存路由，不依赖 history API）。
 *
 * 页面顺序就是使用频率：编排（默认页）→ 拓扑 → 主机 → 运行历史 → 概览。
 * 规格文件的新建/复制/删除并进了编排页的「规格」抽屉——它和编排本来
 * 就是同一件事的两面，单独一页纯属浪费点击。
 */
import { Suspense, lazy, useCallback, useEffect, useState } from "react";
import {
  Layout,
  Menu,
  Button,
  Card,
  Dropdown,
  Form,
  Input,
  Modal,
  Space,
  Typography,
  App as AntApp,
} from "antd";
import {
  ApartmentOutlined,
  DashboardOutlined,
  ExperimentOutlined,
  HistoryOutlined,
  LogoutOutlined,
  CloudServerOutlined,
  KeyOutlined,
  UserOutlined,
} from "@ant-design/icons";
import Login from "./Login";
import Dashboard from "./Dashboard";
import Editor from "./Editor";
import Hosts from "./Hosts";
import Runs from "./Runs";
import { api } from "./api";

// 拓扑图把 react-flow + elkjs 一起拖进来（约 300 KB gzip）。它只在一个页面用到，
// 所以按需加载——首屏不该为一张可能不打开的图付代价。
const TopologyPage = lazy(() => import("./TopologyPage"));

const { Header, Content } = Layout;
const { Text } = Typography;

type Page = "editor" | "topology" | "hosts" | "runs" | "dashboard";

/** 改密码弹窗：旧密码必填（防走神），两次输入一致才放行。 */
function PasswordModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { message } = AntApp.useApp();
  const [busy, setBusy] = useState(false);
  const [form] = Form.useForm();

  const submit = async (v: { old: string; next: string; again: string }) => {
    if (v.next !== v.again) {
      message.error("两次输入的新密码不一致");
      return;
    }
    setBusy(true);
    try {
      await api.changePassword(v.old, v.next);
      message.success("密码已修改");
      form.resetFields();
      onClose();
    } catch (e) {
      message.error((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={<Space><KeyOutlined /> 修改管理员密码</Space>}
      open={open}
      onCancel={() => {
        form.resetFields();
        onClose();
      }}
      footer={null}
      width={420}
      destroyOnClose
    >
      <Form form={form} layout="vertical" onFinish={submit} style={{ marginTop: 12 }}>
        <Form.Item
          name="old"
          label="当前密码"
          rules={[{ required: true, message: "请输入当前密码" }]}
        >
          <Input.Password autoFocus />
        </Form.Item>
        <Form.Item
          name="next"
          label="新密码（至少 8 位）"
          rules={[
            { required: true, message: "请输入新密码" },
            { min: 8, message: "至少 8 位" },
          ]}
        >
          <Input.Password />
        </Form.Item>
        <Form.Item
          name="again"
          label="再输一遍"
          dependencies={["next"]}
          rules={[
            { required: true, message: "请再输入一遍" },
            ({ getFieldValue }) => ({
              validator: (_r, v) =>
                !v || v === getFieldValue("next")
                  ? Promise.resolve()
                  : Promise.reject(new Error("两次输入不一致")),
            }),
          ]}
        >
          <Input.Password onPressEnter={() => form.submit()} />
        </Form.Item>
        <Button type="primary" block loading={busy} onClick={() => form.submit()}>
          修改密码
        </Button>
      </Form>
    </Modal>
  );
}

export default function App() {
  const { message } = AntApp.useApp();
  const [authed, setAuthed] = useState<boolean | null>(null);
  const [page, setPage] = useState<Page>("editor");
  const [pwOpen, setPwOpen] = useState(false);

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
        <div style={{ display: "flex", alignItems: "center", gap: 24, flex: 1, minWidth: 0 }}>
          <Text strong style={{ fontSize: 16, letterSpacing: 0.3 }}>
            labyard
          </Text>
          <Menu
            mode="horizontal"
            disabledOverflow
            selectedKeys={[page]}
            onClick={(e) => setPage(e.key as Page)}
            style={{ flex: 1, minWidth: 0, borderBottom: "none", background: "transparent" }}
            items={[
              {
                key: "editor",
                icon: <ExperimentOutlined />,
                label: "编排",
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
        </div>
        <Space size={8}>
          <Dropdown
            menu={{
              items: [
                {
                  key: "password",
                  icon: <KeyOutlined />,
                  label: "修改密码",
                  onClick: () => setPwOpen(true),
                },
                { type: "divider" as const },
                {
                  key: "logout",
                  icon: <LogoutOutlined />,
                  label: "退出登录",
                  danger: true,
                  onClick: logout,
                },
              ],
            }}
          >
            <Button size="small" icon={<UserOutlined />} type="text">
              admin
            </Button>
          </Dropdown>
        </Space>
      </Header>

      <Content style={{ padding: 20, maxWidth: 1600, width: "100%", margin: "0 auto" }}>
        {page === "editor" && <Editor />}
        {page === "topology" && (
          <Suspense fallback={<Card loading style={{ minHeight: 320 }} />}>
            <TopologyPage />
          </Suspense>
        )}
        {page === "hosts" && <Hosts />}
        {page === "runs" && <Runs />}
        {page === "dashboard" && <Dashboard />}
      </Content>

      <PasswordModal open={pwOpen} onClose={() => setPwOpen(false)} />
    </Layout>
  );
}
