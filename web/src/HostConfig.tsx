/** 主机配置编辑器：改 CPU / 内存 / 系统盘，写回规格文件并可选立即执行。
 *
 * 为什么不用一次弹窗直接 `pct set`：面板的语义是"规格是唯一事实"，
 * 直接改 PVE 会让下次 apply 按旧规格把它改回去（改完就"自己变回来"）。
 * 所以流程固定为：改规格 → apply 按规格落地（ensure_ct 里做 set / resize）。
 *
 * 磁盘只能扩容：PVE 不支持缩盘，缩小会静默无效（界面上会写明）。
 */
import { useEffect, useState } from "react";
import { Alert, Button, Form, InputNumber, Modal, Space, Typography, message } from "antd";
import { api } from "./api";

const { Text } = Typography;

export type ConfigTarget = {
  spec: string;
  zone: string;
  host: string;
  /** 当前值（拓扑节点 meta 里就有，用来预填） */
  cores?: number;
  memory?: number;
  disk?: number;
};

type Model = {
  defaults?: { cores?: number; memory?: number; disk?: number };
  zones?: Record<string, { hosts?: Record<string, Record<string, unknown>> }>;
};

export function HostConfigEditor({
  target,
  onClose,
}: {
  target: ConfigTarget | null;
  onClose: () => void;
}) {
  const [form] = Form.useForm<{ cores: number; memory: number; disk: number }>();
  const [model, setModel] = useState<Model | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!target) return;
    let alive = true;
    api
      .specModel(target.spec)
      .then((r) => {
        if (!alive) return;
        const m = r.model as Model;
        setModel(m);
        const d = m.defaults ?? {};
        const h = (m.zones?.[target.zone]?.hosts?.[target.host] ?? {}) as Record<string, unknown>;
        form.setFieldsValue({
          cores: (h.cores as number) ?? d.cores ?? target.cores ?? 1,
          memory: (h.memory as number) ?? d.memory ?? target.memory ?? 512,
          disk: (h.disk as number) ?? d.disk ?? target.disk ?? 2,
        });
      })
      .catch(() => message.error("读规格失败"));
    return () => {
      alive = false;
    };
  }, [target, form]);

  const save = async (andApply: boolean) => {
    if (!target || !model) return;
    const v = await form.validateFields();
    setBusy(true);
    try {
      // 行级编辑：只改这三行，YAML 的注释与排版原样保留
      // （走整份序列化会把用户手写的注释全部抹掉）
      await api.patchHostConfig(target.spec, target.zone, target.host, {
        cores: v.cores,
        memory: v.memory,
        disk: v.disk,
      });
      if (andApply) {
        await api.applySpec(target.spec);
        message.success("已写入规格并开始执行，进度看「运行历史」");
      } else {
        message.success("已写入规格，点「执行」后生效");
      }
      onClose();
    } catch (e) {
      message.error((e as Error).message || "保存失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={!!target}
      title={
        <Space>
          <span>改配置</span>
          {target && (
            <Text type="secondary" style={{ fontSize: 12 }}>
              {target.spec} / {target.zone} / {target.host}
            </Text>
          )}
        </Space>
      }
      onCancel={onClose}
      destroyOnClose
      footer={
        <Space>
          <Button onClick={onClose} disabled={busy}>
            取消
          </Button>
          <Button onClick={() => save(false)} loading={busy}>
            只存规格
          </Button>
          <Button type="primary" onClick={() => save(true)} loading={busy}>
            保存并执行
          </Button>
        </Space>
      }
    >
      <Form form={form} layout="vertical" preserve={false}>
        <Form.Item label="CPU 核数" name="cores" rules={[{ required: true }]}>
          <InputNumber min={1} max={64} style={{ width: "100%" }} />
        </Form.Item>
        <Form.Item label="内存（MB）" name="memory" rules={[{ required: true }]}>
          <InputNumber min={128} max={65536} step={128} style={{ width: "100%" }} />
        </Form.Item>
        <Form.Item
          label="系统盘（GB）"
          name="disk"
          rules={[{ required: true }]}
          extra="只能扩容：PVE 不支持缩盘，填比现在小的值不会生效。"
        >
          <InputNumber min={1} max={512} style={{ width: "100%" }} />
        </Form.Item>
      </Form>
      <Alert
        type="info"
        showIcon
        style={{ marginTop: 4 }}
        message="改了网段桥/网口这类网络字段请去「编排」页改规格——这里只动计算资源。"
      />
      <Alert
        type="warning"
        showIcon
        style={{ marginTop: 8 }}
        message="CPU/内存对运行中的容器可能需重启才生效（PVE 会标 pending）；磁盘扩容立即生效。"
      />
    </Modal>
  );
}
