import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "./client";
import { reconnectDelayMs, staleFor, updatesUrl, type UiNotice } from "./liveUpdatesCore";

const LiveContext = createContext(false);

/// True while the OMS's update socket is open. Screens poll only while it isn't.
export function useLive(): boolean {
  return useContext(LiveContext);
}

/// Holds one socket to `/updates` for the signed-in trade app. Each notice marks
/// the matching queries stale; every (re)connect marks everything stale once, so
/// whatever changed while the socket was down is picked up. Reconnects with
/// backoff — screens fall back to polling meanwhile.
export function LiveUpdatesProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const [live, setLive] = useState(false);

  useEffect(() => {
    let socket: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let attempt = 0;
    let stopped = false;

    const connect = () => {
      socket = new WebSocket(updatesUrl(window.location, API_BASE));
      socket.onopen = () => {
        attempt = 0;
        setLive(true);
        queryClient.invalidateQueries();
      };
      socket.onmessage = (e) => {
        const stale = staleFor(JSON.parse(e.data) as UiNotice);
        queryClient.invalidateQueries({ predicate: (q) => stale(q.queryKey) });
      };
      socket.onclose = () => {
        setLive(false);
        if (!stopped) retry = setTimeout(connect, reconnectDelayMs(attempt++));
      };
    };
    connect();

    return () => {
      stopped = true;
      clearTimeout(retry);
      socket?.close();
    };
  }, [queryClient]);

  return <LiveContext.Provider value={live}>{children}</LiveContext.Provider>;
}
