import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { FormRow, Notice } from "./common";
import { Segmented, Select } from "./form";
import { Modal } from "./overlay";
import { Pill } from "./status";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ApiError } from "../api/client";
import { resolveCsv, saveConIds, searchContracts } from "../api/ibkrImport";
import type { Contract, LoadReport, ResolveRow } from "../api/ibkrImport";
import { importErrorMessage, selectedConIds } from "../ibkrImport";

const describe = (c: Contract) =>
  `${c.name} · ${c.mic ?? c.primary_exchange} · ${c.currency} · tick ${c.min_tick}`;

const STATUS_TONE = { found: "buy", ambiguous: "warn", not_found: "sell" } as const;

function errText(e: unknown): string {
  return e instanceof ApiError ? importErrorMessage(e.status, e.message) : String(e);
}

function ReportView({ report }: { report: LoadReport }) {
  return (
    <Notice tone="success" title="Saved">
      {report.upserted} instruments saved ({report.mapped} mapped, {report.skipped} skipped
      {report.skipped_reasons.length > 0 ? `: ${report.skipped_reasons.join(", ")}` : ""}).
    </Notice>
  );
}

// Dialog for adding IBKR contracts: a CSV is resolved to conIds and previewed
// before saving; the search tab adds single contracts.
export function IbkrImport({ envs, onClose, intro }: { envs: string[]; onClose: () => void; intro?: ReactNode }) {
  const qc = useQueryClient();
  const [env, setEnv] = useState(envs.includes("PAPER") ? "PAPER" : envs[0]);
  const [rows, setRows] = useState<ResolveRow[]>([]);
  const [picks, setPicks] = useState<Record<number, number>>({});
  const [report, setReport] = useState<LoadReport | null>(null);
  const [text, setText] = useState("");
  const [hits, setHits] = useState<Contract[]>([]);

  const onSaved = (r: LoadReport) => {
    setReport(r);
    void qc.invalidateQueries({ queryKey: ["/admin/instruments"] });
  };
  const resolve = useMutation({
    mutationFn: async (file: File | null) => (file ? resolveCsv(await file.text(), env) : []),
    onSuccess: (r) => { setRows(r); setPicks({}); setReport(null); },
  });
  const save = useMutation({ mutationFn: (ids: number[]) => saveConIds(ids, env), onSuccess: onSaved });
  const search = useMutation({ mutationFn: () => searchContracts(text.trim(), env), onSuccess: setHits });

  const ids = selectedConIds(rows, picks);
  const error = resolve.error ?? save.error ?? search.error;

  return (
    <Modal open onClose={onClose} title="Import from IBKR" className="sm:max-w-3xl">
      <div className="flex flex-col gap-3">
        {intro}
        {envs.length > 1 && (
          <div className="w-fit"><Segmented value={env} onChange={setEnv} data={envs.map((e) => ({ value: e, label: e }))} /></div>
        )}
        {error && <Notice>{errText(error)}</Notice>}
        {report && <ReportView report={report} />}
        <Tabs defaultValue="csv">
          <TabsList>
            <TabsTrigger value="csv">CSV file</TabsTrigger>
            <TabsTrigger value="search">Search</TabsTrigger>
          </TabsList>
          <TabsContent value="csv" className="flex flex-col gap-3 pt-2">
            <FormRow label="CSV file" htmlFor="ibkr-csv">
              <Input id="ibkr-csv" type="file" accept=".csv" onChange={(e) => resolve.mutate(e.currentTarget.files?.[0] ?? null)} />
            </FormRow>
            {rows.length > 0 && (
              <div className="rounded-md border">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>Line</TableHead>
                      <TableHead>Symbol</TableHead>
                      <TableHead>Status</TableHead>
                      <TableHead>Contract</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((r) => (
                      <TableRow key={r.line}>
                        <TableCell className="num">{r.line}</TableCell>
                        <TableCell className="font-semibold">{r.candidates[0]?.symbol ?? ""}</TableCell>
                        <TableCell><Pill tone={STATUS_TONE[r.status]}>{r.status}</Pill></TableCell>
                        <TableCell className="whitespace-normal">
                          {r.status === "found" && r.candidates[0] && describe(r.candidates[0])}
                          {r.status === "ambiguous" && (
                            <Select
                              placeholder="Pick a contract"
                              data={r.candidates.map((c) => ({ value: String(c.con_id), label: describe(c) }))}
                              value={picks[r.line] !== undefined ? String(picks[r.line]) : null}
                              clearable
                              onChange={(v) =>
                                setPicks((p) => {
                                  const next = { ...p };
                                  if (v) next[r.line] = Number(v);
                                  else delete next[r.line];
                                  return next;
                                })
                              }
                            />
                          )}
                          {r.status === "not_found" && <span className="text-muted-foreground">{r.message}</span>}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
            <div className="flex justify-end">
              <Button disabled={ids.length === 0 || save.isPending} onClick={() => save.mutate(ids)}>
                Save {ids.length} instruments
              </Button>
            </div>
          </TabsContent>
          <TabsContent value="search" className="flex flex-col gap-3 pt-2">
            <div className="flex items-end gap-2">
              <Input
                className="h-8 flex-1"
                placeholder="Symbol or name"
                value={text}
                onChange={(e) => setText(e.currentTarget.value)}
                onKeyDown={(e) => { if (e.key === "Enter" && text.trim()) search.mutate(); }}
              />
              <Button disabled={!text.trim() || search.isPending} onClick={() => search.mutate()}>Search</Button>
            </div>
            {hits.length > 0 && (
              <div className="rounded-md border">
                <Table>
                  <TableBody>
                    {hits.map((c) => (
                      <TableRow key={c.con_id}>
                        <TableCell className="font-semibold">{c.symbol}</TableCell>
                        <TableCell className="whitespace-normal">{describe(c)}</TableCell>
                        <TableCell className="text-right">
                          <Button size="xs" variant="secondary" disabled={save.isPending} onClick={() => save.mutate([c.con_id])}>Add</Button>
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </div>
            )}
          </TabsContent>
        </Tabs>
      </div>
    </Modal>
  );
}
