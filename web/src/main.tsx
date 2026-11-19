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
      // 页面间来回切不该每次都重打接口（hosts ~1s、doctor ~4s 都是慢接口），
      // 15 秒内复用缓存；需要实时的页面自己设 refetchInterval。
      staleTime: 15_000,
      gcTime: 5 * 60_000,
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
          Layout: { headerBg: "#ffffff", headerHeight: 56, headerPadding: "0 20px" },
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
