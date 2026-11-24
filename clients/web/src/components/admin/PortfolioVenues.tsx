import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { api } from "../../api/client";
import { useApiMutation } from "../../api/hooks";
import { missingRefs, needsRef, venueGrants, type Grant, type GrantConnection } from "../../grants";

export type GrantsResponse = { grants: Grant[]; connections: GrantConnection[] };
type Draft = { picked: Set<string>; refs: Record<string, string> };

export const grantsPath = (portfolioId: string) => `/admin/portfolios/${portfolioId}/grants`;

/** The venue connections a portfolio holds, from its saved grants. */
export function savedVenues(data: GrantsResponse | undefined): string[] {
  return (data?.grants ?? []).filter((g) => g.kind === "venue").map((g) => g.params.connection).sort();
}

function draftFrom(data: GrantsResponse): Draft {
  const picked = new Set<string>();
  const refs: Record<string, string> = {};
  for (const g of data.grants) {
    if (g.kind !== "venue") continue;
    picked.add(g.params.connection);
    if (g.params.account_ref) refs[g.params.connection] = g.params.account_ref;
  }
  return { picked, refs };
}

/** Venues tab: which broker connections this portfolio may trade on (IBKR also names the account). */
export function PortfolioVenues({ portfolioId, onDirty }: { portfolioId: string; onDirty: (dirty: boolean) => void }) {
  const path = grantsPath(portfolioId);
  const query = useQuery<GrantsResponse>({ queryKey: [path], queryFn: () => api.get<GrantsResponse>(path) });
  // Not dirty: the draft is the server's data, so a refetch shows through.
  const [edited, setEdited] = useState<Draft | null>(null);
  useEffect(() => { onDirty(edited !== null); }, [edited, onDirty]);
  const save = useApiMutation((grants: Grant[]) => api.put(path, grants), {
    invalidate: [path], success: "Venues saved", onDone: () => setEdited(null),
  });

  if (query.error) return <p className="text-destructive">Could not load venues: {query.error instanceof Error ? query.error.message : String(query.error)}</p>;
  const data = query.data;
  if (!data) return <p className="text-muted-foreground">Loading…</p>;
  const draft = edited ?? draftFrom(data);
  const bad = missingRefs(data.connections, draft.picked, draft.refs).length > 0;
  return (
    <div className="flex flex-col gap-2">
      <p className="text-muted-foreground">Where this portfolio may trade. No venue = no trading.</p>
      {data.connections.length === 0 && <p className="text-muted-foreground">No broker connections yet.</p>}
      {data.connections.map((c) => {
        const on = draft.picked.has(c.code);
        const ref = draft.refs[c.code] ?? "";
        return (
          <div key={c.code} className="flex min-h-10 items-center gap-2 border-t">
            <label className="flex flex-1 items-center gap-2">
              <Checkbox checked={on} onCheckedChange={(v) => {
                const picked = new Set(draft.picked);
                if (v === true) picked.add(c.code); else picked.delete(c.code);
                setEdited({ ...draft, picked });
              }} />
              <span>{c.code} <span className="text-muted-foreground">· {c.environment}</span></span>
            </label>
            {on && needsRef(c.broker_code) && (
              <Input className="h-8 w-44" aria-invalid={!ref.trim()} title={!ref.trim() ? "IBKR account required" : undefined}
                placeholder="IBKR account required" aria-label={`IBKR account for ${c.code}`} value={ref}
                onChange={(e) => setEdited({ ...draft, refs: { ...draft.refs, [c.code]: e.target.value } })} />
            )}
          </div>
        );
      })}
      <div className="mt-2 flex justify-end">
        <Button size="sm" disabled={edited === null || save.isPending || bad}
          onClick={() => save.mutate(venueGrants(draft.picked, draft.refs, Object.fromEntries(data.connections.map((c) => [c.code, c.broker_code]))))}>
          Save
        </Button>
      </div>
    </div>
  );
}
