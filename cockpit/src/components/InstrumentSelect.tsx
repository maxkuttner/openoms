import { useState } from "react";
import { Select } from "@mantine/core";
import { useDebouncedValue } from "@mantine/hooks";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";

export interface Instrument {
  id: number;
  symbol: string;
  name: string;
  venue: string;
  asset_class: string;
  status: string;
}

// Searchable instrument picker. Binds the master instrument id (as a string, the form
// of instrument_id used on orders/risk), displays "SYMBOL · name". Server-side search
// (debounced) against basePath (defaults to the admin instruments endpoint).
export function InstrumentSelect({
  value,
  onChange,
  onSelected,
  label,
  required,
  placeholder,
  basePath = "/admin/instruments",
  apiGet = api.get,
}: {
  value: string | null;
  onChange: (v: string | null) => void;
  // Optional: fired alongside onChange with the full row already held in
  // memory from the search results (or null when cleared/not found). Additive
  // and optional so existing callers (cockpit's Blotter filter and
  // CrudResource's "instrument" field type) are unaffected; a caller that
  // needs to render the picked instrument (e.g. a confirmation screen) can
  // use this instead of re-fetching by id, which this endpoint doesn't
  // support anyway.
  onSelected?: (instrument: Instrument | null) => void;
  label?: string;
  required?: boolean;
  placeholder?: string;
  // Base path for the instrument search endpoint. Defaults to the admin surface;
  // the trade app passes "/instruments" instead.
  basePath?: string;
  // Fetch function used for the request. Defaults to the cockpit's admin client
  // (which attaches an Authorization: Bearer admin token). The trade app MUST pass
  // tradeApi.get instead, or it would leak the admin token on every request.
  apiGet?: (path: string) => Promise<unknown>;
}) {
  const [search, setSearch] = useState("");
  const [debounced] = useDebouncedValue(search, 250);
  const q = useQuery<Instrument[]>({
    queryKey: [basePath, debounced],
    queryFn: () =>
      apiGet(
        `${basePath}?limit=50${debounced ? `&search=${encodeURIComponent(debounced)}` : ""}`,
      ) as Promise<Instrument[]>,
  });
  const rows = q.data ?? [];
  const data = rows.map((i) => ({ value: String(i.id), label: `${i.symbol} · ${i.name}` }));
  return (
    <Select
      label={label}
      required={required}
      placeholder={placeholder ?? "Search symbol or name…"}
      searchable
      clearable
      data={data}
      value={value}
      onChange={(v) => {
        onChange(v);
        onSelected?.(rows.find((i) => String(i.id) === v) ?? null);
      }}
      searchValue={search}
      onSearchChange={setSearch}
      nothingFoundMessage={q.isFetching ? "Searching…" : "No matches"}
      filter={({ options }) => options}
    />
  );
}
