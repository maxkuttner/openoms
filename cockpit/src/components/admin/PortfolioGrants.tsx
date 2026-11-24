import { useEffect, useRef, useState, type ReactNode } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Spinner } from "../common";
import { api } from "../../api/client";
import { useApiMutation } from "../../api/hooks";
import { missingRefs, needsRef, venueGrants, type Grant, type GrantConnection } from "../../grants";

type GrantsResponse = { grants: Grant[]; connections: GrantConnection[] };

// Working state of the dialog; each section reads and writes its own slice.
type Draft = { picked: Set<string>; refs: Record<string, string> };

type Section = {
  kind: Grant["kind"];
  title: string;
  render: (draft: Draft, setDraft: (d: Draft) => void, data: GrantsResponse) => ReactNode;
  toGrants: (draft: Draft, data: GrantsResponse) => Grant[];
};

// A future grant kind appends one entry here.
const SECTIONS: Section[] = [
  {
    kind: "venue",
    title: "Venues",
    render: (draft, setDraft, data) => (
      <div className="flex flex-col gap-2">
        {data.connections.length === 0 && <p className="text-sm text-muted-foreground">No broker connections yet.</p>}
        {data.connections.map((c) => {
          const on = draft.picked.has(c.code);
          return (
            <div key={c.code} className="flex items-center gap-2">
              <label className="flex flex-1 items-center gap-2 text-sm">
                <Checkbox
                  checked={on}
                  onCheckedChange={(v) => {
                    const picked = new Set(draft.picked);
                    if (v === true) picked.add(c.code);
                    else picked.delete(c.code);
                    setDraft({ ...draft, picked });
                  }}
                />
                <span>{c.code} · {c.environment}</span>
              </label>
              {on && needsRef(c.broker_code) && (
                <Input
                  className="h-8 w-44"
                  aria-invalid={!(draft.refs[c.code] ?? "").trim()}
                  title={!(draft.refs[c.code] ?? "").trim() ? "IBKR account required" : undefined}
                  placeholder="IBKR account required"
                  aria-label={`IBKR account for ${c.code}`}
                  value={draft.refs[c.code] ?? ""}
                  onChange={(e) => setDraft({ ...draft, refs: { ...draft.refs, [c.code]: e.target.value } })}
                />
              )}
            </div>
          );
        })}
      </div>
    ),
    toGrants: (draft, data) =>
      venueGrants(draft.picked, draft.refs, Object.fromEntries(data.connections.map((c) => [c.code, c.broker_code]))),
  },
];

function initialDraft(data: GrantsResponse): Draft {
  const picked = new Set<string>();
  const refs: Record<string, string> = {};
  for (const g of data.grants) {
    if (g.kind !== "venue") continue;
    picked.add(g.params.connection);
    if (g.params.account_ref) refs[g.params.connection] = g.params.account_ref;
  }
  return { picked, refs };
}

export function PortfolioGrants({ portfolio, onClose }: { portfolio: { id: string; code: string }; onClose: () => void }) {
  const path = `/admin/portfolios/${portfolio.id}/grants`;
  const query = useQuery<GrantsResponse>({ queryKey: [path], queryFn: () => api.get<GrantsResponse>(path) });
  const [draft, setDraft] = useState<Draft>({ picked: new Set(), refs: {} });
  // Seed once: a background refetch must not wipe unsaved edits.
  const seeded = useRef(false);
  useEffect(() => {
    if (query.data && !seeded.current) {
      seeded.current = true;
      setDraft(initialDraft(query.data));
    }
  }, [query.data]);

  const save = useApiMutation(
    (grants: Grant[]) => api.put(path, grants),
    { invalidate: [path], success: "Grants saved", onDone: onClose },
  );

  const data = query.data;
  return (
    <Dialog open onOpenChange={(o) => { if (!o) onClose(); }}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader><DialogTitle>Grants — {portfolio.code}</DialogTitle></DialogHeader>
        {!data ? (
          <Spinner />
        ) : (
          <div className="flex flex-col gap-4">
            {SECTIONS.map((s) => (
              <section key={s.kind} className="flex flex-col gap-2">
                <h3 className="text-sm font-semibold">{s.title}</h3>
                {s.render(draft, setDraft, data)}
              </section>
            ))}
          </div>
        )}
        <DialogFooter>
          <Button variant="outline" onClick={onClose}>Cancel</Button>
          <Button
            disabled={!data || save.isPending || missingRefs(data.connections, draft.picked, draft.refs).length > 0}
            onClick={() => data && save.mutate(SECTIONS.flatMap((s) => s.toGrants(draft, data)))}
          >
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
