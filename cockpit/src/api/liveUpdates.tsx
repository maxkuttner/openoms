import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "./client";
import { reconnectDelayMs, staleFor, updatesUrl, WATCHDOG_MS, type UiNotice } from "./liveUpdatesCore";
import { createRefresher } from "./refresh";

const LiveContext = createContext(false);

/// True while the OMS's update socket is open. Screens poll only while it isn't.
export function useLive(): boolean {
  return useContext(LiveContext);
}

/// Holds one socket to `/updates` for the signed-in trade app. Each notice
/// refreshes the matching queries; every (re)connect refreshes everything once,
/// so whatever changed while the socket was down is picked up. Reconnects with
/// backoff — screens fall back to polling meanwhile — and treats a socket that
/// has gone silent past the watchdog as dead.
export function LiveUpdatesProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const [live, setLive] = useState(false);

  useEffect(() => {
    const refresher = createRefresher(queryClient);
    let socket: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let watchdog: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    let stopped = false;

    const armWatchdog = (s: WebSocket) => {
      clearTimeout(watchdog);
      watchdog = setTimeout(() => s.close(), WATCHDOG_MS);
    };

    const connect = () => {
      const s = new WebSocket(updatesUrl(window.location, API_BASE));
      socket = s;
      s.onopen = () => {
        attempt = 0;
        setLive(true);
        armWatchdog(s);
        refresher.refresh(() => true);
      };
      s.onmessage = (e) => {
        armWatchdog(s);
        try {
          refresher.refresh(staleFor(JSON.parse(e.data) as UiNotice));
        } catch {
          // a frame this client can't read changes nothing it knows how to refresh
        }
      };
      s.onclose = () => {
        clearTimeout(watchdog);
        setLive(false);
        if (!stopped) retry = setTimeout(connect, reconnectDelayMs(attempt++));
      };
    };
    connect();

    return () => {
      stopped = true;
      clearTimeout(retry);
      clearTimeout(watchdog);
      socket?.close();
      refresher.stop();
    };
  }, [queryClient]);

  return <LiveContext.Provider value={live}>{children}</LiveContext.Provider>;
}
