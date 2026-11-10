import { useState } from "react";
import { Alert, Badge, Button, FileInput, Group, Modal, SegmentedControl, Select, Stack, Table, Tabs, Text, TextInput } from "@mantine/core";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { ApiError } from "../api/client";
import { resolveCsv, saveConIds, searchContracts } from "../api/ibkrImport";
import type { Contract, LoadReport, ResolveRow } from "../api/ibkrImport";
import { importErrorMessage, selectedConIds } from "../ibkrImport";

const describe = (c: Contract) =>
  `${c.name} · ${c.mic ?? c.primary_exchange} · ${c.currency} · tick ${c.min_tick}`;

const STATUS_COLOR = { found: "green", ambiguous: "yellow", not_found: "red" } as const;

function errText(e: unknown): string {
  return e instanceof ApiError ? importErrorMessage(e.status, e.message) : String(e);
}

function ReportView({ report }: { report: LoadReport }) {
  return (
    <Alert color="green" title="Saved">
      {report.upserted} instruments saved ({report.mapped} mapped, {report.skipped} skipped
      {report.skipped_reasons.length > 0 ? `: ${report.skipped_reasons.join(", ")}` : ""}).
    </Alert>
  );
}

// Dialog for adding IBKR contracts: a CSV is resolved to conIds and previewed
// before saving; the search tab adds single contracts.
export function IbkrImport({ envs, onClose }: { envs: string[]; onClose: () => void }) {
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
    <Modal opened onClose={onClose} title="Import from IBKR" size="xl" centered>
      <Stack>
        {envs.length > 1 && (
          <SegmentedControl value={env} onChange={setEnv} data={envs} w="fit-content" />
        )}
        {error && <Alert color="red">{errText(error)}</Alert>}
        {report && <ReportView report={report} />}
        <Tabs defaultValue="csv">
          <Tabs.List>
            <Tabs.Tab value="csv">CSV file</Tabs.Tab>
            <Tabs.Tab value="search">Search</Tabs.Tab>
          </Tabs.List>
          <Tabs.Panel value="csv" pt="md">
            <Stack>
              <FileInput
                label="CSV file"
                accept=".csv"
                placeholder="Choose a .csv file"
                onChange={(f) => resolve.mutate(f)}
                clearable
              />
              {rows.length > 0 && (
                <Table striped>
                  <Table.Thead>
                    <Table.Tr>
                      <Table.Th>Line</Table.Th>
                      <Table.Th>Symbol</Table.Th>
                      <Table.Th>Status</Table.Th>
                      <Table.Th>Contract</Table.Th>
                    </Table.Tr>
                  </Table.Thead>
                  <Table.Tbody>
                    {rows.map((r) => (
                      <Table.Tr key={r.line}>
                        <Table.Td>{r.line}</Table.Td>
                        <Table.Td><b>{r.candidates[0]?.symbol ?? ""}</b></Table.Td>
                        <Table.Td>
                          <Badge color={STATUS_COLOR[r.status]} variant="light">{r.status}</Badge>
                        </Table.Td>
                        <Table.Td>
                          {r.status === "found" && r.candidates[0] && describe(r.candidates[0])}
                          {r.status === "ambiguous" && (
                            <Select
                              placeholder="Pick a contract"
                              data={r.candidates.map((c) => ({ value: String(c.con_id), label: describe(c) }))}
                              value={picks[r.line] !== undefined ? String(picks[r.line]) : null}
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
                          {r.status === "not_found" && <Text size="sm" c="dimmed">{r.message}</Text>}
                        </Table.Td>
                      </Table.Tr>
                    ))}
                  </Table.Tbody>
                </Table>
              )}
              <Group justify="flex-end">
                <Button disabled={ids.length === 0} loading={save.isPending} onClick={() => save.mutate(ids)}>
                  Save {ids.length} instruments
                </Button>
              </Group>
            </Stack>
          </Tabs.Panel>
          <Tabs.Panel value="search" pt="md">
            <Stack>
              <Group align="flex-end">
                <TextInput
                  style={{ flex: 1 }}
                  placeholder="Symbol or name"
                  value={text}
                  onChange={(e) => setText(e.currentTarget.value)}
                  onKeyDown={(e) => { if (e.key === "Enter" && text.trim()) search.mutate(); }}
                />
                <Button disabled={!text.trim()} loading={search.isPending} onClick={() => search.mutate()}>
                  Search
                </Button>
              </Group>
              {hits.length > 0 && (
                <Table striped>
                  <Table.Tbody>
                    {hits.map((c) => (
                      <Table.Tr key={c.con_id}>
                        <Table.Td><b>{c.symbol}</b></Table.Td>
                        <Table.Td>{describe(c)}</Table.Td>
                        <Table.Td>
                          <Button size="xs" variant="light" disabled={save.isPending} onClick={() => save.mutate([c.con_id])}>
                            Add
                          </Button>
                        </Table.Td>
                      </Table.Tr>
                    ))}
                  </Table.Tbody>
                </Table>
              )}
            </Stack>
          </Tabs.Panel>
        </Tabs>
      </Stack>
    </Modal>
  );
}
