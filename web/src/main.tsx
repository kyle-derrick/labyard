import React from "react";
import ReactDOM from "react-dom/client";
import { ConfigProvider, App as AntApp, theme } from "antd";
import zhCN from "antd/locale/zh_CN";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import App from "./App";
import "./styles.css";

const qc = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: false,
      // 拓扑/状态轮询交给页面控制，全局不自动轮询
      staleTime: 5_000,
    },
  },
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <ConfigProvider
      locale={zhCN}
      theme={{
        algorithm: theme.defaultAlgorithm,
        token: {
          colorPrimary: "#2f6feb",
          borderRadius: 6,
          fontFamily:
            '-apple-system, "PingFang SC", "Microsoft YaHei", system-ui, sans-serif',
        },
        components: {
          Layout: { headerBg: "#ffffff", siderBg: "#ffffff" },
        },
      }}
    >
      <AntApp>
        <QueryClientProvider client={qc}>
          <App />
        </QueryClientProvider>
      </AntApp>
    </ConfigProvider>
  </React.StrictMode>,
);
