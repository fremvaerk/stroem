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
        manualChunks: {
          "vendor-xyflow": ["@xyflow/react", "@dagrejs/dagre"],
          "vendor-react": ["react", "react-dom", "react-router"],
        },
      },
    },
  },
});
