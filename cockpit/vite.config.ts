import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev: proxy the admin/order API to the running OMS so there is no CORS to deal with.
// The OMS listens on :3001 (see src/main.rs). Override with OMS_URL if needed.
const target = process.env.OMS_URL ?? "http://localhost:3001";

export default defineConfig(({ command }) => ({
  plugins: [react()],
  // Both bundles share one asset tree; each app's SHELL is served at its own
  // path (/cockpit/, /trade/) by src/cockpit.rs. Vite's base is global per
  // build, so it cannot be either app's path.
  base: command === "build" ? "/ui/" : "/",
  build: {
    rollupOptions: { input: { cockpit: "index.html", trade: "trade.html" } },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target, changeOrigin: true, rewrite: (p) => p.replace(/^\/api/, "") },
      // The trade app's own client-side redirect to a missing session
      // (cockpit/src/trade/api/client.ts) navigates to `/auth/login`
      // relative to whatever origin it's running on. Without this, that
      // lands on vite's own dev server, which has no such route.
      "/auth": { target, changeOrigin: true },
    },
  },
}));
