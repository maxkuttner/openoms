import { useState, type ReactNode } from "react";
import { Check, ChevronsUpDown, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { FormRow } from "./common";

export type Option = { value: string; label: string; disabled?: boolean };
export const toOptions = (data: (string | Option)[]): Option[] =>
  data.map((d) => (typeof d === "string" ? { value: d, label: d } : d));

type Labelled = { label?: ReactNode; description?: ReactNode; error?: ReactNode; required?: boolean; className?: string };

function Wrap({ label, description, error, required, className, children }: Labelled & { children: ReactNode }) {
  if (!label && !description && !error) return <div className={className}>{children}</div>;
  return (
    <div className={className}>
      <FormRow label={<>{label}{required && <span className="text-sell"> *</span>}</>} error={error}>
        {children}
        {description && <p className="text-xs text-muted-foreground">{description}</p>}
      </FormRow>
    </div>
  );
}

/**
 * One-of-many picker. Always searchable (type to filter); `clearable` adds a
 * clear button. `value` null means nothing chosen.
 */
export function Select({
  data, value, onChange, placeholder = "Select…", clearable, disabled, id, ...wrap
}: Labelled & {
  data: (string | Option)[];
  value: string | null;
  onChange: (value: string | null) => void;
  placeholder?: string;
  clearable?: boolean;
  disabled?: boolean;
  id?: string;
}) {
  const [open, setOpen] = useState(false);
  const opts = toOptions(data);
  const current = opts.find((o) => o.value === value);
  return (
    <Wrap {...wrap}>
      <div className="relative">
        <Popover open={open} onOpenChange={setOpen}>
          <PopoverTrigger asChild>
            <Button id={id} type="button" variant="outline" role="combobox" aria-expanded={open} disabled={disabled}
              className="h-8 w-full min-w-0 justify-between bg-background px-2.5 font-normal">
              <span className={cn("min-w-0 truncate", !current && "text-muted-foreground")}>{current?.label ?? placeholder}</span>
              <ChevronsUpDown className="size-3.5 opacity-50" />
            </Button>
          </PopoverTrigger>
          <PopoverContent className="w-[var(--radix-popover-trigger-width)] min-w-48 p-0" align="start">
            <Command>
              {opts.length > 7 && <CommandInput placeholder="Search…" />}
              <CommandList>
                <CommandEmpty>Nothing matches.</CommandEmpty>
                <CommandGroup>
                  {opts.map((o) => (
                    <CommandItem key={o.value} value={`${o.label} ${o.value}`} disabled={o.disabled}
                      onSelect={() => { onChange(o.value); setOpen(false); }}>
                      <Check className={cn("size-3.5", o.value === value ? "opacity-100" : "opacity-0")} />
                      {o.label}
                    </CommandItem>
                  ))}
                </CommandGroup>
              </CommandList>
            </Command>
          </PopoverContent>
        </Popover>
        {clearable && current && !disabled && (
          <button type="button" aria-label="Clear" onClick={() => onChange(null)}
            className="absolute top-1/2 right-7 -translate-y-1/2 text-muted-foreground hover:text-foreground">
            <X className="size-3.5" />
          </button>
        )}
      </div>
    </Wrap>
  );
}

/** Many-of-many picker with chips. */
export function MultiSelect({
  data, value, onChange, placeholder = "Select…", disabled, ...wrap
}: Labelled & { data: (string | Option)[]; value: string[]; onChange: (value: string[]) => void; placeholder?: string; disabled?: boolean }) {
  const [open, setOpen] = useState(false);
  const opts = toOptions(data);
  const toggle = (v: string) => onChange(value.includes(v) ? value.filter((x) => x !== v) : [...value, v]);
  return (
    <Wrap {...wrap}>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button type="button" variant="outline" disabled={disabled}
            className="h-auto min-h-8 w-full justify-between bg-background px-2 py-1 font-normal">
            <span className="flex flex-wrap gap-1">
              {value.length === 0 && <span className="text-muted-foreground">{placeholder}</span>}
              {value.map((v) => (
                <Badge key={v} variant="secondary" className="gap-1">
                  {opts.find((o) => o.value === v)?.label ?? v}
                </Badge>
              ))}
            </span>
            <ChevronsUpDown className="size-3.5 opacity-50" />
          </Button>
        </PopoverTrigger>
        <PopoverContent className="w-[var(--radix-popover-trigger-width)] min-w-48 p-0" align="start">
          <Command>
            <CommandInput placeholder="Search…" />
            <CommandList>
              <CommandEmpty>Nothing matches.</CommandEmpty>
              <CommandGroup>
                {opts.map((o) => (
                  <CommandItem key={o.value} value={`${o.label} ${o.value}`} onSelect={() => toggle(o.value)}>
                    <Check className={cn("size-3.5", value.includes(o.value) ? "opacity-100" : "opacity-0")} />
                    {o.label}
                  </CommandItem>
                ))}
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>
    </Wrap>
  );
}

/**
 * A number field. Keeps what the user typed as a string while editing and
 * reports `number | ""` like Mantine's NumberInput did (empty = "").
 */
export function NumberInput({
  value, onChange, min, max, step, placeholder, disabled, id, ...wrap
}: Labelled & {
  value: number | string;
  onChange: (value: number | string) => void;
  min?: number;
  max?: number;
  step?: number | string;
  placeholder?: string;
  disabled?: boolean;
  id?: string;
}) {
  return (
    <Wrap {...wrap}>
      <Input id={id} type="number" inputMode="decimal" className="num h-8" value={value} min={min} max={max} step={step ?? "any"}
        placeholder={placeholder} disabled={disabled} aria-invalid={!!wrap.error}
        onChange={(e) => { const t = e.currentTarget.value; onChange(t === "" ? "" : Number.isNaN(Number(t)) ? t : Number(t)); }} />
    </Wrap>
  );
}

/** A labelled text input. */
export function TextField({
  value, onChange, type = "text", placeholder, disabled, autoComplete, id, autoFocus, ...wrap
}: Labelled & {
  value: string;
  onChange: (value: string) => void;
  type?: string;
  placeholder?: string;
  disabled?: boolean;
  autoComplete?: string;
  id?: string;
  autoFocus?: boolean;
}) {
  return (
    <Wrap {...wrap}>
      <Input id={id} type={type} className="h-8" value={value} placeholder={placeholder} disabled={disabled} autoComplete={autoComplete}
        autoFocus={autoFocus} aria-invalid={!!wrap.error} onChange={(e) => onChange(e.currentTarget.value)} />
    </Wrap>
  );
}

/** Two or more mutually exclusive buttons (Mantine SegmentedControl). */
export function Segmented<T extends string>({
  data, value, onChange, tone,
}: { data: { value: T; label: ReactNode }[]; value: T; onChange: (value: T) => void; tone?: (value: T) => string }) {
  return (
    <div className="flex overflow-hidden rounded-md border" role="radiogroup">
      {data.map((d) => (
        <button key={d.value} type="button" role="radio" aria-checked={d.value === value} onClick={() => onChange(d.value)}
          className={cn("flex-1 px-2 py-1.5 text-foreground-2 hover:bg-accent",
            d.value === value && (tone?.(d.value) ?? "bg-accent text-foreground"))}>
          {d.label}
        </button>
      ))}
    </div>
  );
}

/** A label + big number tile. `tone` colours P&L. */
export function Stat({ label, value, tone }: { label: ReactNode; value: ReactNode; tone?: "pos" | "neg" }) {
  return (
    <div className="rounded-md border bg-card px-3 py-2">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className={cn("num mt-0.5 text-base font-semibold", tone === "pos" && "text-buy", tone === "neg" && "text-sell")}>{value}</div>
    </div>
  );
}
