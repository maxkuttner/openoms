import { useState } from "react";
import { Input } from "@/components/ui/input";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import type { Me } from "../../access";
import { TradeBlotter } from "../../components/trading/TradeBlotter";
import type { StatusFilter } from "../../orders";

const FILTERS: [StatusFilter, string][] = [["all", "All"], ["open", "Open"], ["filled", "Filled"], ["closed", "Closed"]];

/** Every order the user may see: status buckets and search over the blotter (portfolio picker, cancel, timeline). */
export function OrdersPage({ me }: { me: Me }) {
  const [filter, setFilter] = useState<StatusFilter>("all");
  const [query, setQuery] = useState("");
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2">
        <Tabs value={filter} onValueChange={(v) => setFilter(v as StatusFilter)}>
          <TabsList>{FILTERS.map(([v, l]) => <TabsTrigger key={v} value={v}>{l}</TabsTrigger>)}</TabsList>
        </Tabs>
        <Input
          aria-label="Search orders"
          placeholder="Order id, client id or symbol…"
          value={query}
          onChange={(e) => setQuery(e.currentTarget.value)}
          className="h-8 w-64"
        />
      </div>
      <TradeBlotter portfolios={me.portfolios} statusFilter={filter} query={query} />
    </div>
  );
}
