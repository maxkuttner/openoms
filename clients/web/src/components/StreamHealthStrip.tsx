import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { Spinner } from "./common";
import { Pill, type Tone } from "./status";
import { api } from "../api/client";
import type { StreamHealth } from "../api/types";
import { ago, describeStream, lastEventLabel } from "../streamHealth";

const PATH = "/admin/stream-health";

const STATE_TONE: Record<StreamHealth["state"], Tone> = {
  live: "buy",
  connecting: "warn",
  down: "sell",
  stopped: "sell",
};

const STATE_TEXT: Record<StreamHealth["state"], string> = {
  live: "text-muted-foreground",
  connecting: "text-warn",
  down: "text-sell",
  stopped: "text-sell font-medium",
};

/** The current time, re-read every `ms`: relative times tick between fetches. */
function useNow(ms = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

/**
 * Connection health of every stream of one kind (WebSocket, FIX, IBKR TWS),
 * one status set for all: live, connecting, down (retrying), stopped (needs the
 * user). Polls every 5s; the relative times tick locally each second. State is
 * process-local to the OMS (empty when no streams of that kind run).
 * `kind` = "execution" for broker order/fill streams, "feed" for market data.
 */
export function StreamHealthStrip({
  kind,
  title = "Live connections",
  emptyText = "No live streams in this process.",
}: {
  kind: StreamHealth["kind"];
  title?: string;
  emptyText?: string;
}) {
  const now = useNow();
  const { data, isLoading, isError } = useQuery<StreamHealth[]>({
    queryKey: [PATH],
    queryFn: () => api.get<StreamHealth[]>(PATH),
    refetchInterval: 5000,
  });

  if (isLoading) return <Spinner />;
  if (isError) return null;
  const streams = (data ?? []).filter((s) => s.kind === kind);
  if (streams.length === 0) return <p className="text-muted-foreground">{emptyText}</p>;

  return (
    <div className="flex flex-col gap-2">
      <h2 className="text-sm font-semibold">{title}</h2>
      <div className="flex flex-wrap gap-2">
        {streams.map((s) => {
          const event = lastEventLabel(s, now);
          return (
            <Tooltip key={`${s.broker_code}/${s.environment}`}>
              <TooltipTrigger asChild>
                <div
                  className={cn(
                    "flex max-w-full flex-col gap-1 rounded-md border bg-card px-2.5 py-1.5",
                    s.state === "stopped" && "border-sell/60 ring-1 ring-sell/40",
                  )}
                >
                  <div className="flex items-center gap-2">
                    <Pill tone={STATE_TONE[s.state]} className={cn(s.state === "stopped" && "font-semibold uppercase")}>
                      {s.state}
                    </Pill>
                    <span className="font-medium">{s.broker_code}</span>
                    {s.transport === "fix" && <Pill tone="info">FIX</Pill>}
                    <span className="text-muted-foreground">{s.environment}</span>
                  </div>
                  <span className={cn("num max-w-[42rem] truncate text-xs", STATE_TEXT[s.state])}>{describeStream(s, now)}</span>
                </div>
              </TooltipTrigger>
              <TooltipContent className="max-w-md">
                <div className="flex flex-col gap-0.5">
                  <span>{describeStream(s, now)}</span>
                  {s.connected_since && <span>connected {ago(s.connected_since, now)}</span>}
                  <span>heartbeat every {s.heartbeat_secs}s</span>
                  {event && <span>{event}</span>}
                </div>
              </TooltipContent>
            </Tooltip>
          );
        })}
      </div>
    </div>
  );
}
