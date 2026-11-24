import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";
import "./styles/globals.css";
import { Toaster } from "@/components/ui/sonner";
import { TooltipProvider } from "@/components/ui/tooltip";
import { App } from "./App";
import { useTheme } from "./theme";

function ThemedToaster() {
  return <Toaster position="top-right" theme={useTheme()} />;
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
});

// BASE_URL is the shared ASSET base (/ui/), not the app's path. The shell is
// served at /app/, so that is the router basename.
const basename = import.meta.env.DEV ? "/" : "/app/";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <TooltipProvider delayDuration={300}>
        <BrowserRouter basename={basename}>
          <App />
        </BrowserRouter>
        <ThemedToaster />
      </TooltipProvider>
    </QueryClientProvider>
  </React.StrictMode>,
);
