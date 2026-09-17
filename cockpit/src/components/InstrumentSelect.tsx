import { useState } from "react";
import { Select } from "@mantine/core";
import { useDebouncedValue } from "@mantine/hooks";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";

interface Instrument {
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
  label,
  required,
  placeholder,
  basePath = "/admin/instruments",
  apiGet = api.get,
}: {
  value: string | null;
  onChange: (v: string | null) => void;
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
  const data = (q.data ?? []).map((i) => ({ value: String(i.id), label: `${i.symbol} · ${i.name}` }));
  return (
    <Select
      label={label}
      required={required}
      placeholder={placeholder ?? "Search symbol or name…"}
      searchable
      clearable
      data={data}
      value={value}
      onChange={onChange}
      searchValue={search}
      onSearchChange={setSearch}
      nothingFoundMessage={q.isFetching ? "Searching…" : "No matches"}
      filter={({ options }) => options}
    />
  );
}
