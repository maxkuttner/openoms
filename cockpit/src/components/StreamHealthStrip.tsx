import { useQuery } from "@tanstack/react-query";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Spinner } from "./common";
import { Pill, type Tone } from "./status";
import { api } from "../api/client";
import type { StreamHealth } from "../api/types";
import { ago, healthLabel } from "../streamHealth";

const PATH = "/admin/stream-health";

const STATE_TONE: Record<StreamHealth["state"], Tone> = {
  live: "buy",
  connecting: "warn",
  down: "sell",
};

/**
 * Live WebSocket connection health, filtered to one stream kind. Polls every 5s;
 * state is process-local to the OMS (empty when no streams of that kind run).
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
        {streams.map((s) => (
          <Tooltip key={`${s.broker_code}/${s.environment}`}>
            <TooltipTrigger asChild>
              <div className="flex items-center gap-2 rounded-md border bg-card px-2.5 py-1.5">
                <Pill tone={STATE_TONE[s.state]}>{s.state}</Pill>
                <span className="font-medium">{s.broker_code}</span>
                {s.transport === "fix" && <Pill tone="info">FIX</Pill>}
                <span className="text-muted-foreground">{s.environment}</span>
                <span className="num text-xs text-muted-foreground">{ago(s.last_event_at)}</span>
              </div>
            </TooltipTrigger>
            <TooltipContent>{healthLabel(s)}</TooltipContent>
          </Tooltip>
        ))}
      </div>
    </div>
  );
}
