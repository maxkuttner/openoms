import { useState, type ReactNode } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { cn } from "@/lib/utils";
import { NumberInput, Select, TextField } from "./form";
import { Spinner } from "./common";
import { ConfirmButton } from "./ConfirmButton";
import { Pill } from "./status";
import { api } from "../api/client";
import { useList, useApiMutation } from "../api/hooks";
import { InstrumentSelect } from "./InstrumentSelect";
import { useLifecycle } from "./Lifecycle";
import type { Resource } from "../lifecycle";
import { initialFormValues } from "../crud";

export type Field = {
  name: string;
  label: string;
  type?: "text" | "number" | "select" | "checkbox" | "instrument";
  // Fixed options, or options that depend on the other fields' values.
  options?: Option[] | ((values: Record<string, any>) => Option[]);
  // A line under the input; may depend on the other fields' values.
  description?: (values: Record<string, any>) => ReactNode;
  // Populate a select from another list endpoint, mapping each row to {value,label}.
  optionsPath?: string;
  optionValue?: string;
  optionLabel?: string;
  optionFilter?: (row: any) => boolean; // keep only some rows of optionsPath
  required?: boolean;
  default?: string; // starting value for a new row
  inCreate?: boolean; // default true
  inEdit?: boolean; // default true
};

type Option = { value: string; label: string };

export type Column = { key: string; label: string; render?: (row: any) => ReactNode };

type Props = {
  title: string;
  path: string; // collection path, e.g. "/admin/accounts"
  idKey?: string; // default "id"
  columns: Column[];
  fields: Field[];
  editable?: boolean;
  deletable?: boolean;
  // Extra per-row action rendered in the action cell, before Edit. Additive —
  // omit it and nothing changes.
  rowAction?: (row: any) => ReactNode;
  // Rows are deactivated instead of deleted: checkboxes, a Deactivate bar,
  // inactive rows hidden behind a "Show inactive" switch, and Reactivate.
  lifecycle?: { resource: Resource; afterChange?: () => Promise<unknown> };
  // Create form only: given the values before and after a change, the fields
  // to set as a consequence (e.g. a code derived from two selects).
  derive?: (previous: Record<string, any>, next: Record<string, any>) => Record<string, any>;
  // Called with the created row once a create succeeds.
  onCreated?: (row: any) => void;
  // Show only some rows of the list.
  rowFilter?: (row: any) => boolean;
  // Hide the New button (default true).
  creatable?: boolean;
};

type Form = { values: Record<string, any>; set: (name: string, value: unknown) => void };

// Render the input for one field, resolving select options (incl. async optionsPath).
function FieldInput({ field, form }: { field: Field; form: Form }) {
  const remote = useList<any>(field.optionsPath ?? "", !!field.optionsPath);
  const v = form.values[field.name];
  const common = { label: field.label, required: field.required, description: field.description?.(form.values) };
  switch (field.type) {
    case "instrument":
      return (
        <InstrumentSelect
          label={field.label}
          required={field.required}
          value={(v as string) || null}
          onChange={(x) => form.set(field.name, x ?? "")}
          apiGet={api.get}
        />
      );
    case "number":
      return <NumberInput {...common} value={v ?? ""} onChange={(x) => form.set(field.name, x)} />;
    case "checkbox":
      return (
        <label className="flex items-start gap-2">
          <Checkbox checked={!!v} onCheckedChange={(on) => form.set(field.name, on === true)} className="mt-0.5" />
          <span className="flex flex-col">
            <span>{field.label}</span>
            {common.description && <span className="text-xs text-muted-foreground">{common.description}</span>}
          </span>
        </label>
      );
    case "select": {
      const opts =
        (typeof field.options === "function" ? field.options(form.values) : field.options) ??
        (remote.data ?? []).filter(field.optionFilter ?? (() => true)).map((r) => ({
          value: String(r[field.optionValue ?? "id"]),
          label: String(r[field.optionLabel ?? "code"]),
        }));
      return <Select {...common} data={opts} clearable value={v ? String(v) : null} onChange={(x) => form.set(field.name, x ?? "")} />;
    }
    default:
      return <TextField {...common} value={v ?? ""} onChange={(x) => form.set(field.name, x)} />;
  }
}

export function CrudResource({
  title, path, idKey = "id", columns, fields, editable, deletable, rowAction, lifecycle, derive, onCreated, rowFilter, creatable = true,
}: Props) {
  const list = useList<any>(path);
  const life = useLifecycle(list.data, {
    resource: lifecycle?.resource ?? "principal",
    path,
    idKey,
    label: (r) => String(r.code ?? r[idKey]),
    afterChange: lifecycle?.afterChange,
  });
  const rows = (lifecycle ? life.visible : (list.data ?? [])).filter(rowFilter ?? (() => true));
  const [editing, setEditing] = useState<any | null>(null); // row being edited, or {} for create
  const isCreate = editing && !editing[idKey];

  const [values, setValues] = useState<Record<string, any>>({});
  const form: Form = {
    values,
    set: (name, value) =>
      setValues((previous) => {
        const next = { ...previous, [name]: value };
        // Create form only: fields derived from the others follow them.
        return isCreate && derive ? { ...next, ...derive(previous, next) } : next;
      }),
  };

  const open = (row: any | null) => {
    setValues(initialFormValues(fields, row));
    setEditing(row ?? {});
  };

  const save = useApiMutation(
    (vals: Record<string, any>) => {
      const body = cleanup(vals, fields, !!isCreate);
      return isCreate ? api.post(path, body) : api.patch(`${path}/${editing[idKey]}`, body);
    },
    {
      invalidate: [path],
      success: isCreate ? "Created" : "Updated",
      onDone: (row) => {
        setEditing(null);
        if (isCreate) onCreated?.(row);
      },
    },
  );

  const remove = useApiMutation((row: any) => api.del(`${path}/${row[idKey]}`), {
    invalidate: [path],
    success: "Deleted",
  });

  const hasActions = editable || deletable || rowAction;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <h2 className="text-sm font-semibold">{title}</h2>
        <div className="flex items-center gap-3">
          {lifecycle && life.switchControl}
          {creatable && <Button size="sm" onClick={() => open(null)}>New</Button>}
        </div>
      </div>
      {lifecycle && life.selectionBar}

      {list.isLoading ? (
        <Spinner />
      ) : (
        <div className="rounded-md border">
          <Table>
            <TableHeader>
              <TableRow>
                {lifecycle && <TableHead className="w-10">{life.headerCheckbox}</TableHead>}
                {columns.map((c) => <TableHead key={c.key}>{c.label}</TableHead>)}
                {hasActions && <TableHead />}
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((row) => (
                <TableRow key={row[idKey]} className={cn(lifecycle && !life.isActive(row) && "text-muted-foreground")}>
                  {lifecycle && <TableCell>{life.rowCheckbox(row)}</TableCell>}
                  {columns.map((c) => (
                    <TableCell key={c.key}>{c.render ? c.render(row) : formatCell(row[c.key])}</TableCell>
                  ))}
                  {hasActions && (
                    <TableCell>
                      <div className="flex justify-end gap-1.5">
                        {rowAction?.(row)}
                        {lifecycle && !life.isActive(row)
                          ? life.reactivateButton(row)
                          : editable && <Button size="xs" variant="secondary" onClick={() => open(row)}>Edit</Button>}
                        {deletable && (
                          <ConfirmButton size="icon-xs" variant="ghost" className="text-sell" aria-label="Delete"
                            title="Delete this row?" confirmLabel="Delete" onConfirm={() => remove.mutate(row)}>
                            <X />
                          </ConfirmButton>
                        )}
                      </div>
                    </TableCell>
                  )}
                </TableRow>
              ))}
              {rows.length === 0 && (
                <TableRow>
                  <TableCell colSpan={columns.length + 2} className="py-4 text-center text-muted-foreground">No rows yet.</TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </div>
      )}

      {lifecycle && life.confirmModal}

      <Dialog open={!!editing} onOpenChange={(o) => { if (!o) setEditing(null); }}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader><DialogTitle>{isCreate ? `New ${title}` : `Edit ${title}`}</DialogTitle></DialogHeader>
          <form onSubmit={(e) => { e.preventDefault(); save.mutate(values); }} className="flex flex-col gap-3">
            {fields
              .filter((f) => (isCreate ? f.inCreate !== false : f.inEdit !== false))
              .map((f) => <FieldInput key={f.name} field={f} form={form} />)}
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setEditing(null)}>Cancel</Button>
              <Button type="submit" disabled={save.isPending}>Save</Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function formatCell(v: unknown): ReactNode {
  if (v === null || v === undefined || v === "") return <span className="text-muted-foreground">—</span>;
  if (typeof v === "boolean") return <Pill tone={v ? "buy" : "muted"}>{v ? "yes" : "no"}</Pill>;
  return String(v);
}

// Drop empty strings (so optional fields aren't sent as ""), coerce nothing else.
function cleanup(vals: Record<string, any>, fields: Field[], isCreate: boolean): Record<string, any> {
  const out: Record<string, any> = {};
  for (const f of fields) {
    if (isCreate ? f.inCreate === false : f.inEdit === false) continue;
    const v = vals[f.name];
    if (v === "" || v === undefined) {
      if (isCreate && f.required) out[f.name] = v;
      continue;
    }
    out[f.name] = v;
  }
  return out;
}
