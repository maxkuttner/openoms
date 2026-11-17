import { useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { notify } from "./notify";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { api, ApiError } from "../api/client";
import { IbkrImport } from "./IbkrImport";
import { openTaskCenter, type UniverseStatus as Status } from "../api/jobs";

const SYMBOL: Record<Status["state"], { sym: string; color: string; label: string }> = {
  not_loaded: { sym: "○", color: "text-muted-foreground", label: "Not loaded" },
  loading: { sym: "◐", color: "text-warn", label: "Loading" },
  loaded: { sym: "●", color: "text-buy", label: "Loaded" },
  partial: { sym: "◑", color: "text-warn", label: "Partial" },
  failed: { sym: "✕", color: "text-sell", label: "Failed" },
};

const ACTION: Partial<Record<Status["state"], string>> = {
  not_loaded: "Load",
  loaded: "Reload",
  partial: "Load again",
  failed: "Retry",
};

/** Start (or restart) a connection's universe load. A 409 means one is already running. */
export async function loadUniverse(code: string): Promise<boolean> {
  try {
    await api.post(`/admin/broker-connections/${code}/universe/load`, {});
    toast.success(`Loading the instrument universe for ${code} in the background.`, {
      action: { label: "Open Jobs", onClick: openTaskCenter },
    });
    return true;
  } catch (err) {
    if (err instanceof ApiError && err.status === 409) {
      notify({ color: "yellow", message: "Already loading." });
      openTaskCenter();
    } else {
      notify({ color: "red", message: err instanceof ApiError ? err.message : String(err) });
    }
    return false;
  }
}

/** The "Instrument universe" cell for one broker connection. */
export function UniverseStatus({ code, broker, environment }: { code: string; broker?: string; environment?: string }) {
  const qc = useQueryClient();
  const [importing, setImporting] = useState(false);
  // IBKR has no list-everything call, so its "load" is the CSV import.
  const isIbkr = broker === "IBKR_TWS" && !!environment;
  const { data } = useQuery<Status>({
    queryKey: ["/admin/broker-connections", code, "universe"],
    queryFn: () => api.get<Status>(`/admin/broker-connections/${code}/universe`),
    refetchInterval: (q) => (q.state.data?.state === "loading" ? 2000 : 15000),
  });
  // Load needs usable credentials; the server refuses without them (422), the
  // button says why before anyone clicks.
  const { data: creds } = useQuery<{ state: string }>({
    queryKey: [`/admin/broker-connections/${code}/credentials`],
    queryFn: () => api.get<{ state: string }>(`/admin/broker-connections/${code}/credentials`),
  });
  if (!data) return <span className="text-muted-foreground">…</span>;

  const s = SYMBOL[data.state];
  const job = data.last_job;
  const total = job?.progress_total ?? 0;
  const done = job?.progress_done ?? 0;
  const action = ACTION[data.state];
  const configured = creds?.state === "configured";

  async function run() {
    if (await loadUniverse(code)) {
      qc.invalidateQueries({ queryKey: ["/admin/broker-connections", code, "universe"] });
      qc.invalidateQueries({ queryKey: ["/admin/jobs"] });
    }
  }

  const button = (
    <Button size="xs" variant="secondary" onClick={isIbkr ? () => setImporting(true) : run} disabled={!configured}>
      {isIbkr ? "Import instruments" : action}
    </Button>
  );

  return (
    <div className="flex items-center justify-between gap-3">
      <div className="flex flex-col gap-0.5">
        <span>
          <span className={s.color}>{s.sym} {s.label}</span>
          {data.state === "loading" && total > 0 && <span className="num"> — {done.toLocaleString()} of {total.toLocaleString()}</span>}
          {data.count > 0 && data.state !== "loading" && <span className="num"> — {data.count.toLocaleString()} instruments</span>}
        </span>
        {data.state === "loading" && total > 0 && (
          <div className="h-1 w-44 overflow-hidden rounded bg-accent">
            <div className="h-full bg-buy" style={{ width: `${(done / total) * 100}%` }} />
          </div>
        )}
        {data.state === "loaded" && job?.finished_at && (
          <span className="num text-xs text-muted-foreground">loaded {new Date(job.finished_at).toLocaleString()}</span>
        )}
        {data.reason && (
          <span className={cn("line-clamp-2 max-w-sm text-xs whitespace-normal", data.state === "failed" ? "text-sell" : "text-muted-foreground")}>
            {data.reason}
          </span>
        )}
      </div>
      {(isIbkr || action) &&
        (configured ? (
          button
        ) : (
          <Tooltip>
            <TooltipTrigger asChild><span tabIndex={0}>{button}</span></TooltipTrigger>
            <TooltipContent>Add credentials first</TooltipContent>
          </Tooltip>
        ))}
      {importing && isIbkr && (
        <IbkrImport
          envs={[environment!]}
          onClose={() => setImporting(false)}
          intro={
            <p>
              IBKR has no way to list every instrument, so the OMS adds the ones you trade. Upload a CSV
              with the columns symbol, exchange and currency, or conid (exchange as an IBKR code or a
              MIC, up to 1000 rows). Each row is looked up at IBKR and pinned by its conId; if a row
              matches several contracts you pick one. You can also search for single instruments.{" "}
              <a href="https://openoms.dev/integrations/interactive-brokers/#instruments" target="_blank" rel="noreferrer"
                className="text-primary hover:underline">
                Read the docs
              </a>
            </p>
          }
        />
      )}
    </div>
  );
}
