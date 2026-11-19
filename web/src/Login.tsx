import { useEffect, useState } from "react";
import { api } from "./api";
import {
  Alert,
  Button,
  Card,
  Form,
  Input,
  Spin,
  Typography,
  App as AntApp,
} from "antd";
import { LockOutlined } from "@ant-design/icons";

const { Title, Text } = Typography;

export default function Login({ onOk }: { onOk: () => void }) {
  const { message } = AntApp.useApp();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [checking, setChecking] = useState(true);

  // 已登录就直接进（刷新页面时省一次输入）
  useEffect(() => {
    api
      .me()
      .then(() => onOk())
      .catch(() => setChecking(false));
  }, [onOk]);

  if (checking) {
    return (
      <div className="login-wrap">
        <Spin />
      </div>
    );
  }

  const submit = async (v: { password: string }) => {
    setLoading(true);
    setError(null);
    try {
      await api.login(v.password);
      // 密码对不等于会话可用：cookie 可能被浏览器拒收（页面嵌在别的网站的
      // iframe 里、隐身模式拦第三方 cookie 等）。立刻探一次 me()，失败就
      // 给出可操作的原因，而不是进主界面后到处"未登录/文件不存在"。
      try {
        await api.me();
      } catch {
        setError(
          "密码正确，但浏览器没有保存会话 Cookie——常见于页面嵌在其他网站的框架里，或浏览器拦截了 Cookie。请直接在浏览器地址栏打开本面板地址访问。",
        );
        return;
      }
      message.success("已登录");
      onOk();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="login-wrap">
      <Card className="login-card" bordered={false}>
        <div style={{ textAlign: "center", marginBottom: 24 }}>
          <Title level={3} style={{ marginBottom: 4 }}>
            labyard
          </Title>
          <Text type="secondary">网络编排控制台</Text>
        </div>

        {error && (
          <Alert
            type="error"
            showIcon
            message={error}
            style={{ marginBottom: 16 }}
          />
        )}

        <Form layout="vertical" onFinish={submit}>
          <Form.Item
            name="password"
            label="管理员密码"
            rules={[{ required: true, message: "请输入密码" }]}
          >
            <Input.Password
              prefix={<LockOutlined />}
              placeholder="首次启动时终端打印的密码"
              autoFocus
              size="large"
            />
          </Form.Item>
          <Button
            type="primary"
            htmlType="submit"
            block
            size="large"
            loading={loading}
          >
            登录
          </Button>
        </Form>

        <Text
          type="secondary"
          style={{ display: "block", marginTop: 16, fontSize: 12 }}
        >
          密码只以 scrypt 哈希形式存储在本机，会话由 HMAC 签名 cookie 维持。
        </Text>
      </Card>
    </div>
  );
}
