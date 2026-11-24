import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// Dev: proxy the admin/order API to the running OMS so there is no CORS to deal with.
// The OMS listens on :3001 (see src/main.rs). Override with OMS_URL if needed.
const target = process.env.OMS_URL ?? "http://localhost:3001";

// A session-authenticated write is rejected unless its Origin header exactly
// equals oms.toml's public_base_url (src/sessions.rs origin_is_allowed) — a
// deliberate CSRF defense, not something to relax server-side. In dev the
// browser's real origin is vite's own (localhost:5173), which never matches.
// Rewriting the header on the way out, dev-proxy-only, makes the OMS see the
// same origin it would if served same-origin in production; nothing here
// runs in a production build.
function rewriteOriginHeader(proxy: import("http-proxy").default) {
  proxy.on("proxyReq", (proxyReq) => {
    proxyReq.setHeader("origin", target);
  });
  // the live-updates WebSocket upgrade is checked against the same origin
  proxy.on("proxyReqWs", (proxyReq) => {
    proxyReq.setHeader("origin", target);
  });
}

export default defineConfig(({ command }) => ({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": path.resolve(__dirname, "src") } },
  // Assets live under /ui/; the app's shell is served at /app/ by
  // src/cockpit.rs, so the asset base cannot be the app's path.
  base: command === "build" ? "/ui/" : "/",
  build: {
    rollupOptions: { input: { app: "index.html" } },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target,
        changeOrigin: true,
        ws: true,
        rewrite: (p) => p.replace(/^\/api/, ""),
        configure: rewriteOriginHeader,
      },
      // The SSO buttons navigate to `/auth/oidc/login` and `/auth/invite/…`
      // relative to whatever origin they run on. Without this, that lands
      // on vite's own dev server, which has no such route.
      "/auth": { target, changeOrigin: true, configure: rewriteOriginHeader },
    },
  },
}));
