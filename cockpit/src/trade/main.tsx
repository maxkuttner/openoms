import React from "react";
import ReactDOM from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import { Notifications } from "@mantine/notifications";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";
import "@mantine/core/styles.css";
import "@mantine/notifications/styles.css";
import { TradeApp } from "./App";
import { theme } from "./theme";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
});

// BASE_URL is the shared ASSET base (/ui/), not this app's path. The trade
// shell is served at /trade/, so that is its router basename in production.
//
// In dev both apps are served by one vite server with no path split (each
// is its own top-level .html: index.html vs trade.html) — a basename of "/"
// here would let react-router rewrite the address bar down to a bare "/",
// losing "trade.html" from the URL. Reloading that bare "/" then hits
// vite's default entry (index.html, the ADMIN app), whose own router
// redirects "/" elsewhere — landing you in a different app entirely.
// Keeping "/trade.html" as the dev basename keeps it in the URL, so a
// reload re-resolves to this app's own entry file.
const basename = import.meta.env.DEV ? "/trade.html" : "/trade/";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <Notifications position="top-right" />
      <QueryClientProvider client={queryClient}>
        <BrowserRouter basename={basename}>
          <TradeApp />
        </BrowserRouter>
      </QueryClientProvider>
    </MantineProvider>
  </React.StrictMode>,
);
