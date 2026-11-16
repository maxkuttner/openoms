import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Check, ChevronsUpDown, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { cn } from "@/lib/utils";
import { useDebounced } from "@/lib/useDebounced";
import { FormRow } from "./common";

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
  // it required turns that into a compile error instead. Admin screens pass
  // `api.get` with the admin path; trading screens pass it with "/instruments".
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
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  // The label of the last pick, so the trigger keeps showing it after the
  // search moves on and the row drops out of `rows`.
  const [pickedLabel, setPickedLabel] = useState<string | null>(null);
  const debounced = useDebounced(search, 250);
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
  const current = data.find((d) => d.value === value)?.label ?? (value ? pickedLabel : null);

  const pick = (v: string | null) => {
    onChange(v);
    setPickedLabel(v ? data.find((d) => d.value === v)?.label ?? null : null);
    onSelected?.(rows.find((i) => String(i.id) === v) ?? null);
  };

  const picker = (
    <div className="relative">
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" role="combobox" aria-expanded={open}
            className="h-8 w-full min-w-0 justify-between bg-background px-2.5 font-normal">
            <span className={cn("min-w-0 truncate", !current && "text-muted-foreground")}>
              {current ?? placeholder ?? "Search symbol or name…"}
            </span>
            <ChevronsUpDown className="size-3.5 opacity-50" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[var(--radix-popover-trigger-width)] min-w-64 p-0" align="start">
          <Command shouldFilter={false}>
            <CommandInput placeholder="Search symbol or name…" value={search} onValueChange={setSearch} />
            <CommandList>
              <CommandEmpty>{q.isFetching ? "Searching…" : "No matches"}</CommandEmpty>
              <CommandGroup>
                {data.map((d) => (
                  <CommandItem key={d.value} value={d.value} onSelect={() => { pick(d.value); setOpen(false); }}>
                    <Check className={cn("size-3.5", d.value === value ? "opacity-100" : "opacity-0")} />
                    {d.label}
                  </CommandItem>
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>
      {value && (
        <button type="button" aria-label="Clear" onClick={() => pick(null)}
          className="absolute top-1/2 right-7 -translate-y-1/2 text-muted-foreground hover:text-foreground">
          <X className="size-3.5" />
        </button>
      )}
    </div>
  );
  if (!label) return picker;
  return <FormRow label={<>{label}{required && <span className="text-sell"> *</span>}</>}>{picker}</FormRow>;
}
