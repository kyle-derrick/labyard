import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // 单页应用 + 嵌入式资源，不需要 sourcemap（会让二进制变大）
    sourcemap: false,
    chunkSizeWarningLimit: 1200,
  },
  server: {
    port: 5173,
    proxy: {
      // 开发时把 API 转给宿主上的 labyard
      "/api": {
        target: "http://127.0.0.1:8443",
        changeOrigin: true,
        secure: false,
      },
    },
  },
});
