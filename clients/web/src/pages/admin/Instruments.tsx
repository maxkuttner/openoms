import { useState } from "react";
import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { useDebounced } from "@/lib/useDebounced";
import { api } from "../../api/client";
import type { InstrumentSummary } from "../../api/types";
import { Spinner } from "../../components/common";
import { Pill } from "../../components/status";

// Read-only browser over the seeded master catalog. Instruments are seeded
// broker-first (a universe load per broker connection); pricing coverage is derived per feed at
// startup, not seeded — there is no in-UI seeding flow. Server-side search
// (debounced) against /admin/instruments.
export function InstrumentsPage() {
  const [search, setSearch] = useState("");
  const debounced = useDebounced(search, 250);
  const { data, isLoading, error } = useQuery<InstrumentSummary[]>({
    queryKey: ["/admin/instruments", debounced],
    queryFn: () =>
      api.get<InstrumentSummary[]>(
        `/admin/instruments?limit=200${debounced ? `&search=${encodeURIComponent(debounced)}` : ""}`,
      ),
  });

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">Instruments</h2>
        {isLoading && <Spinner />}
      </div>
      <p className="text-muted-foreground">
        Loaded from each broker's catalog — see the instrument universe on Broker connections. Which
        of these a data feed can price is derived from the feed's own symbology at startup.
      </p>
      <Input className="h-8 max-w-sm" placeholder="Search symbol or name…" value={search} onChange={(e) => setSearch(e.currentTarget.value)} />
      {error && <p className="text-sell">Failed to load instruments.</p>}
      <div className="rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Symbol</TableHead>
              <TableHead>Name</TableHead>
              <TableHead>Venue</TableHead>
              <TableHead>Asset class</TableHead>
              <TableHead>Status</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(data ?? []).map((i) => (
              <TableRow key={i.id}>
                <TableCell className="font-semibold">{i.symbol}</TableCell>
                <TableCell>{i.name}</TableCell>
                <TableCell>{i.venue}</TableCell>
                <TableCell>{i.asset_class}</TableCell>
                <TableCell><Pill tone={i.status === "ACTIVE" ? "buy" : "muted"}>{i.status}</Pill></TableCell>
              </TableRow>
            ))}
            {!isLoading && (data ?? []).length === 0 && (
              <TableRow>
                <TableCell colSpan={5} className="py-4 text-center text-muted-foreground">
                  No instruments yet — load a broker's universe from{" "}
                  <Link to="/admin/broker-connections" className="text-primary hover:underline">Broker connections</Link>.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}
