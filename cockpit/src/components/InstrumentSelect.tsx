import { useState } from "react";
import { Select } from "@mantine/core";
import { useDebouncedValue } from "@mantine/hooks";
import { useQuery } from "@tanstack/react-query";

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
  apiGet,
  externalSelection,
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
  // Fetch function used for the request. REQUIRED, and deliberately so: there is
  // no default. A default of the cockpit's admin client (which attaches
  // Authorization: Bearer <admin token>) would be fail-open — one forgotten prop
  // on a trade-side usage would put the admin token on a trader request. Making
  // it required turns that into a compile error instead. The cockpit passes
  // `api.get`; the trade app passes `tradeApi.get`.
  apiGet: (path: string) => Promise<unknown>;
  // An instrument selected OUTSIDE this component's own search (e.g. a
  // Watchlist row elsewhere on the page) that `value` may now point at. This
  // endpoint only returns a page of up to 50 rows (or the active search), so
  // that instrument won't generally be among `rows` — without this, `value`
  // would resolve to no matching option and the Select would render
  // blank/wrong even though the caller's state is correct. Merged into
  // `data` below only when `rows` doesn't already contain it.
  externalSelection?: { id: string; symbol: string; name: string } | null;
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
  if (externalSelection && !rows.some((i) => String(i.id) === externalSelection.id)) {
    data.push({ value: externalSelection.id, label: `${externalSelection.symbol} · ${externalSelection.name}` });
  }
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
