import { useEffect, useMemo, useState } from "react";
import { useQueries } from "@tanstack/react-query";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
  AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import type { Portfolio } from "../../api/types";
import { Select, TextField } from "../../components/form";
import { Pill } from "../../components/status";
import { PortfolioAccess, accessPath } from "../../components/admin/PortfolioAccess";
import { PortfolioVenues, grantsPath, savedVenues, type GrantsResponse } from "../../components/admin/PortfolioVenues";
import { statusOptions } from "../../lifecycle";
import { accessSummary, venueSummary, type AccessEntry } from "../../portfolioAccess";

const PATH = "/admin/portfolios";
const STATUS = statusOptions("portfolio");
type Dirty = (dirty: boolean) => void;

function GeneralTab({ portfolio, onSaved, onDirty }: { portfolio: Portfolio | null; onSaved: (id: string) => void; onDirty: Dirty }) {
  const [codeDraft, setCode] = useState<string | null>(null);
  const [nameDraft, setName] = useState<string | null>(null);
  const [statusDraft, setStatus] = useState<string | null>(null);
  const code = codeDraft ?? portfolio?.code ?? "";
  const name = nameDraft ?? portfolio?.name ?? "";
  const status = statusDraft ?? portfolio?.status ?? "ACTIVE";
  const dirty = codeDraft !== null || nameDraft !== null || statusDraft !== null;
  useEffect(() => { onDirty(dirty); }, [dirty, onDirty]);
  const save = useApiMutation(
    async () => {
      if (portfolio) { await api.patch(`${PATH}/${portfolio.id}`, { name: name.trim(), status }); return portfolio.id; }
      return (await api.post<{ id: string }>(PATH, { code: code.trim(), name: name.trim(), status })).id;
    },
    { invalidate: [PATH], success: "Portfolio saved", onDone: (id) => { setCode(null); setName(null); setStatus(null); onSaved(id as string); } },
  );
  return (
    <div className="flex flex-col gap-3">
      {portfolio ? <p className="font-mono text-sm">{portfolio.code}</p> : <TextField label="Code" required value={code} onChange={setCode} />}
      <TextField label="Name" required value={name} onChange={setName} />
      <Select label="Status" data={STATUS} value={status} onChange={(v) => v && setStatus(v)} />
      <p className="text-xs text-muted-foreground">Closing a portfolio keeps its orders and history; it stops taking new orders.</p>
      <div className="flex justify-end">
        <Button size="sm" disabled={save.isPending || !name.trim() || (!portfolio && !code.trim())} onClick={() => save.mutate(undefined)}>Save</Button>
      </div>
    </div>
  );
}

function Editor({ selected, portfolio, onSelect, onDirtyChange }: {
  selected: string; portfolio: Portfolio | null; onSelect: (id: string) => void; onDirtyChange: Dirty;
}) {
  const isNew = selected === "new";
  const [tab, setTab] = useState(isNew ? "general" : "access");
  const [dirtyTabs, setDirtyTabs] = useState<Record<string, boolean>>({});
  const markers = useMemo(() => {
    const mark = (t: string): Dirty => (d) => setDirtyTabs((m) => (m[t] === d ? m : { ...m, [t]: d }));
    return { access: mark("access"), venues: mark("venues"), general: mark("general") };
  }, []);
  const anyDirty = Object.values(dirtyTabs).some(Boolean);
  useEffect(() => { onDirtyChange(anyDirty); }, [anyDirty, onDirtyChange]);
  return (
    <section aria-label="Portfolio editor" className="min-w-0 flex-1 rounded-md border p-4">
      <div className="mb-3 flex items-baseline gap-3">
        <h2 className="font-mono text-base font-semibold">{isNew ? "New portfolio" : portfolio?.code}</h2>
        <span className="text-xs text-muted-foreground">{portfolio?.name}</span>
      </div>
      <Tabs value={tab} onValueChange={setTab}>
        <TabsList>
          {!isNew && <TabsTrigger value="access">Access</TabsTrigger>}
          {!isNew && <TabsTrigger value="venues">Venues</TabsTrigger>}
          <TabsTrigger value="general">General</TabsTrigger>
        </TabsList>
        <div className="mt-4">
          {portfolio && <TabsContent forceMount value="access" className="data-[state=inactive]:hidden"><PortfolioAccess portfolioId={portfolio.id} onDirty={markers.access} /></TabsContent>}
          {portfolio && <TabsContent forceMount value="venues" className="data-[state=inactive]:hidden"><PortfolioVenues portfolioId={portfolio.id} onDirty={markers.venues} /></TabsContent>}
          <TabsContent forceMount value="general" className="data-[state=inactive]:hidden"><GeneralTab portfolio={portfolio} onSaved={onSelect} onDirty={markers.general} /></TabsContent>
        </div>
      </Tabs>
    </section>
  );
}

/** Portfolios: who can reach each one (groups and people), where it may trade, and its name and status. */
export function PortfoliosPage() {
  const list = useList<Portfolio>(PATH);
  const rows = list.data ?? [];
  const [selected, setSelected] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  // Same query keys as the editor tabs, so saving there refreshes the table.
  const access = useQueries({ queries: rows.map((p) => ({ queryKey: [accessPath(p.id)], queryFn: () => api.get<AccessEntry[]>(accessPath(p.id)) })) });
  const venues = useQueries({ queries: rows.map((p) => ({ queryKey: [grantsPath(p.id)], queryFn: () => api.get<GrantsResponse>(grantsPath(p.id)) })) });
  const first = rows[0]?.id ?? null;
  const portfolio = rows.find((p) => p.id === selected) ?? null;
  const effective = selected === "new" ? "new" : portfolio ? portfolio.id : first;
  // Switching away from unsaved edits asks first.
  const go = (id: string) => { if (id === effective) return; if (dirty) setPending(id); else setSelected(id); };
  const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-col gap-4 lg:flex-row lg:items-start">
        <section aria-label="Portfolios" className="min-w-0 flex-1 rounded-md border p-3">
          <div className="mb-2 flex items-center justify-between">
            <h2 className="text-sm font-semibold">Portfolios</h2>
            <Button size="sm" onClick={() => go("new")}>New portfolio</Button>
          </div>
          {list.error && <p className="text-destructive">Could not load portfolios: {errText(list.error)}</p>}
          <Table>
            <TableHeader>
              <TableRow><TableHead>Portfolio</TableHead><TableHead>Access</TableHead><TableHead>Venues</TableHead><TableHead>Status</TableHead></TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((p, i) => (
                <TableRow key={p.id} onClick={() => go(p.id)} aria-selected={effective === p.id} className={cn("cursor-pointer", effective === p.id && "bg-muted")}>
                  <TableCell className="font-mono">{p.code}</TableCell>
                  <TableCell className="text-muted-foreground">{access[i]?.data ? accessSummary(access[i].data!) : access[i]?.error ? "?" : "…"}</TableCell>
                  <TableCell className="text-muted-foreground">{venues[i]?.data ? venueSummary(savedVenues(venues[i].data)) : venues[i]?.error ? "?" : "…"}</TableCell>
                  <TableCell><Pill tone={p.status === "ACTIVE" ? "buy" : "muted"}>{p.status}</Pill></TableCell>
                </TableRow>
              ))}
              {list.data && rows.length === 0 && <TableRow><TableCell colSpan={4} className="text-center text-muted-foreground">No portfolios yet.</TableCell></TableRow>}
            </TableBody>
          </Table>
        </section>
        {effective && <Editor key={effective} selected={effective} portfolio={portfolio ?? rows.find((p) => p.id === effective) ?? null} onSelect={setSelected} onDirtyChange={setDirty} />}
      </div>
      <AlertDialog open={pending !== null} onOpenChange={(o) => !o && setPending(null)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Discard unsaved changes?</AlertDialogTitle>
            <AlertDialogDescription>Edits you have not saved will be lost.</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep editing</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={() => { setSelected(pending!); setPending(null); }}>Discard</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
