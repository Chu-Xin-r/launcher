import { defineConfig } from "vite";

// 前端构建产物输出到 dist，Tauri 以 frontendDist 引用
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "es2022",
    minify: "esbuild",
    sourcemap: false,
  },
});
