import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev: proxy the admin/order API to the running OMS so there is no CORS to deal with.
// The OMS listens on :3001 (see src/main.rs). Override with OMS_URL if needed.
const target = process.env.OMS_URL ?? "http://localhost:3001";

export default defineConfig(({ command }) => ({
  plugins: [react()],
  // Dev serves at "/"; the production bundle is served by the OMS under "/cockpit/"
  // (tower-http ServeDir), so assets must resolve there.
  base: command === "build" ? "/cockpit/" : "/",
  server: {
    port: 5173,
    proxy: {
      "/api": { target, changeOrigin: true, rewrite: (p) => p.replace(/^\/api/, "") },
    },
  },
}));
