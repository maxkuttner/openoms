import { useQuery } from "@tanstack/react-query";
import { API_BASE } from "../api/client";
import { useLive } from "../trade/api/liveUpdates";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

/// Reachability of the OMS, polled on its own. `/health` is unauthenticated,
/// so this answers "can I reach the server", not "am I still signed in".
/// Fetched directly because `/health` answers plain `OK`, not JSON.
export function ConnectionDot() {
  const health = useQuery({
    queryKey: ["/health"],
    queryFn: async () => {
      const res = await fetch(`${API_BASE}/health`);
      if (!res.ok) throw new Error(String(res.status));
      return true;
    },
    refetchInterval: 10_000,
    retry: false,
  });
  const live = useLive();

  const { color, label } = health.isError
    ? { color: "bg-sell", label: "No connection to the OMS" }
    : health.isFetching && !health.data
      ? { color: "bg-warn", label: "Checking the connection" }
      : { color: "bg-buy", label: live ? "Live updates" : "Connected" };

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="flex items-center gap-1.5 text-xs text-muted-foreground" aria-label={label}>
          <span className={`size-1.5 rounded-full ${color}`} />
          {health.isError ? "Offline" : live ? "Live" : "Connected"}
        </span>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}
