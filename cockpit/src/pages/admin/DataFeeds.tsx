import { useState } from "react";
import { Button } from "@/components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { Spinner } from "../../components/common";
import { Drawer } from "../../components/overlay";
import { Pill } from "../../components/status";
import { useQuery } from "@tanstack/react-query";
import { api } from "../../api/client";
import { StreamHealthStrip } from "../../components/StreamHealthStrip";
import { CredentialsPanel } from "../../components/CredentialsPanel";
import type { FeedConnectionSummary, FeedSummary } from "../../api/types";

// Market-data feeds — distinct from broker connections (execution). Shows live
// feed stream health, the configured feed connections (where credentials are
// managed), the ranked failover policy, and how many instruments each feed
// currently prices. The policy/coverage table is derived data and stays
// read-only; feed connection credentials are configured below via
// CredentialsPanel.
export function DataFeedsPage() {
  const { data, isLoading, error } = useQuery<FeedSummary[]>({
    queryKey: ["/admin/feeds"],
    queryFn: () => api.get<FeedSummary[]>("/admin/feeds"),
  });

  const connections = useQuery<FeedConnectionSummary[]>({
    queryKey: ["/admin/feed-connections"],
    queryFn: () => api.get<FeedConnectionSummary[]>("/admin/feed-connections"),
  });
  const [selected, setSelected] = useState<FeedConnectionSummary | null>(null);

  return (
    <div className="flex flex-col gap-5">
      <StreamHealthStrip
        kind="feed"
        title="Live data feeds"
        emptyText="No data feeds running in this process."
      />

      <div className="flex flex-col gap-2">
        <h2 className="text-sm font-semibold">Feed connections</h2>
        <p className="text-muted-foreground">Configure, test, and clear the API key each feed connection uses.</p>
        {connections.isLoading ? (
          <Spinner />
        ) : (
          <div className="rounded-md border">
            <Table>
              <TableHeader>
                <TableRow>
                  <TableHead>Code</TableHead>
                  <TableHead>Provider</TableHead>
                  <TableHead>Dataset</TableHead>
                  <TableHead>Status</TableHead>
                  <TableHead />
                </TableRow>
              </TableHeader>
              <TableBody>
                {(connections.data ?? []).map((row) => (
                  <TableRow key={row.code}>
                    <TableCell>{row.code}</TableCell>
                    <TableCell>{row.provider}</TableCell>
                    <TableCell>{row.dataset ?? <span className="text-muted-foreground">—</span>}</TableCell>
                    <TableCell><Pill tone={row.status === "ACTIVE" ? "buy" : "muted"}>{row.status}</Pill></TableCell>
                    <TableCell className="text-right">
                      <Button size="xs" variant="secondary" onClick={() => setSelected(row)}>Credentials</Button>
                    </TableCell>
                  </TableRow>
                ))}
                {(connections.data ?? []).length === 0 && (
                  <TableRow>
                    <TableCell colSpan={5} className="py-4 text-center text-muted-foreground">No feed connections yet.</TableCell>
                  </TableRow>
                )}
              </TableBody>
            </Table>
          </div>
        )}
      </div>

      <div className="flex flex-col gap-2">
        <div className="flex items-center justify-between">
          <h2 className="text-sm font-semibold">Feed policy</h2>
          {isLoading && <Spinner />}
        </div>
        <p className="text-muted-foreground">
          Ranked market-data sources per instrument class; lower rank wins, higher ranks are
          failover. Coverage is derived from each feed's own symbology — there is no mapping table.
        </p>
        {error && <p className="text-sell">Failed to load feeds.</p>}
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Feed</TableHead>
                <TableHead>Instrument class</TableHead>
                <TableHead className="text-right">Rank</TableHead>
                <TableHead>Enabled</TableHead>
                <TableHead className="text-right">Mapped instruments</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(data ?? []).map((f) => (
                <TableRow key={`${f.feed_code}/${f.instrument_class}`}>
                  <TableCell className="font-semibold">{f.feed_code}</TableCell>
                  <TableCell>{f.instrument_class}</TableCell>
                  <TableCell className="num text-right">{f.rank}</TableCell>
                  <TableCell><Pill tone={f.enabled ? "buy" : "muted"}>{f.enabled ? "enabled" : "disabled"}</Pill></TableCell>
                  <TableCell className="num text-right">{f.mapped_instruments.toLocaleString()}</TableCell>
                </TableRow>
              ))}
              {!isLoading && (data ?? []).length === 0 && (
                <TableRow>
                  <TableCell colSpan={5} className="py-4 text-center text-muted-foreground">No feed policy configured.</TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      </div>

      <Drawer open={!!selected} onClose={() => setSelected(null)} title={selected ? `Credentials — ${selected.code}` : ""} className="sm:max-w-md">
        {selected && <CredentialsPanel kind="feed" code={selected.code} providerCode={selected.provider} />}
      </Drawer>
    </div>
  );
}
