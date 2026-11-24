import { useState } from "react";
import { Check, ChevronsUpDown, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import type { GrantedPortfolio } from "../access";

/**
 * Search-and-select for a portfolio (Popover + Command), matching on code and name.
 * With `allLabel`, a first entry picks "no portfolio" (null).
 */
export function PortfolioPicker({
  portfolios, value, onChange, allLabel, className,
}: {
  portfolios: GrantedPortfolio[];
  value: string | null;
  onChange: (id: string | null) => void;
  allLabel?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const current = portfolios.find((p) => p.portfolio_id === value);
  const pick = (id: string | null) => { setOpen(false); onChange(id); };
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="outline" size="sm" role="combobox" aria-expanded={open} aria-label="Portfolio"
          className={cn("w-56 justify-start gap-2", className)}>
          <Search className="text-muted-foreground" />
          <span className="flex-1 truncate text-left">
            {current ? <>{current.code} <span className="text-muted-foreground">{current.name}</span></> : (allLabel ?? "Pick a portfolio")}
          </span>
          <ChevronsUpDown className="text-muted-foreground" />
        </Button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 p-0">
        <Command>
          <CommandInput placeholder="Portfolio code or name…" />
          <CommandList>
            <CommandEmpty>No portfolio matches.</CommandEmpty>
            <CommandGroup>
              {allLabel && (
                <CommandItem value={`__all ${allLabel}`} onSelect={() => pick(null)}>
                  <Check className={cn(value === null ? "opacity-100" : "opacity-0")} />
                  {allLabel}
                </CommandItem>
              )}
              {portfolios.map((p) => (
                <CommandItem key={p.portfolio_id} value={`${p.code} ${p.name} ${p.portfolio_id}`} onSelect={() => pick(p.portfolio_id)}>
                  <Check className={cn(value === p.portfolio_id ? "opacity-100" : "opacity-0")} />
                  <span className="font-medium">{p.code}</span>
                  <span className="truncate text-muted-foreground">{p.name}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
