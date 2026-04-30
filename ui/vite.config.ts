import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "path";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://localhost:8080",
        changeOrigin: true,
        ws: true,
        configure: (proxy) => {
          proxy.on("error", (err) => { console.warn("Proxy error:", err.message); });
          proxy.on("proxyReqWs", (_proxyReq, _req, socket) => {
            socket.on("error", (err) => { console.warn("WebSocket proxy error:", err.message); });
          });
        },
      },
      "/worker": {
        target: "http://localhost:8080",
        changeOrigin: true,
      },
    },
  },
  build: {
    sourcemap: 'hidden',
    outDir: "../crates/stroem-server/static",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        // Rollup 5 (Vite 8) tightened the TS shape for `manualChunks` — the
        // `{ [chunk]: string[] }` form no longer typechecks against the new
        // OutputOptions, so we use the equivalent function form instead.
        manualChunks(id) {
          if (id.includes("/@xyflow/react/") || id.includes("/@dagrejs/dagre/")) {
            return "vendor-xyflow";
          }
          if (
            id.includes("/node_modules/react/") ||
            id.includes("/node_modules/react-dom/") ||
            id.includes("/node_modules/react-router/")
          ) {
            return "vendor-react";
          }
        },
      },
    },
  },
});
