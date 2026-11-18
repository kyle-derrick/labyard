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
          <Text type="secondary">实验网络编排控制台</Text>
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
