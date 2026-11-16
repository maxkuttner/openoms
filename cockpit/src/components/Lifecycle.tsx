import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Switch } from "@/components/ui/switch";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { api } from "../api/client";
import { notifyError, notifyOk } from "../api/hooks";
import { INACTIVE, isActive, visibleRows, type Resource } from "../lifecycle";

type Row = Record<string, any> & { status: string };

type Options = {
  resource: Resource;
  path: string; // collection path; a row is patched at `${path}/${row[idKey]}`
  idKey: string;
  label: (row: Row) => string; // what the confirm dialog lists, e.g. the code
  // Runs after a status change lands, e.g. reloading broker connections so a
  // deactivated one stops streaming.
  afterChange?: () => Promise<unknown>;
};

// Select rows, deactivate them, and bring them back: the state and the
// controls shared by every admin table that deactivates instead of deleting.
export function useLifecycle<T extends Row>(rows: T[] | undefined, opts: Options) {
  const qc = useQueryClient();
  const [showInactive, setShowInactive] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState<T[] | null>(null);

  const visible = visibleRows(rows ?? [], showInactive);
  const id = (r: T) => String(r[opts.idKey]);
  const picked = visible.filter((r) => selected.has(id(r)));
  // One action per selection: deactivate when anything active is picked,
  // otherwise (only inactive rows picked) reactivate.
  const deactivating = picked.some((r) => isActive(r.status));

  const setStatus = useMutation({
    mutationFn: async ({ targets, status }: { targets: T[]; status: string }) => {
      for (const r of targets) await api.patch(`${opts.path}/${encodeURIComponent(id(r))}`, { status });
      await opts.afterChange?.();
      return { n: targets.length, status };
    },
    onSuccess: ({ n, status }) => {
      qc.invalidateQueries({ queryKey: [opts.path] });
      notifyOk(`${n} ${isActive(status) ? "reactivated" : "deactivated"}`);
      setSelected(new Set());
      setConfirming(null);
    },
    onError: (e) => {
      // Some rows may have changed before the failure; show the truth.
      qc.invalidateQueries({ queryKey: [opts.path] });
      notifyError(e);
    },
  });

  const toggle = (r: T) =>
    setSelected((s) => {
      const next = new Set(s);
      next.has(id(r)) ? next.delete(id(r)) : next.add(id(r));
      return next;
    });
  const allPicked = visible.length > 0 && picked.length === visible.length;
  const toggleAll = () => setSelected(allPicked ? new Set() : new Set(visible.map(id)));

  const reactivate = (targets: T[]) => setStatus.mutate({ targets, status: "ACTIVE" });

  const switchControl = (
    <label className="flex items-center gap-2 text-foreground-2">
      <Switch
        checked={showInactive}
        onCheckedChange={(on) => {
          setShowInactive(on);
          setSelected(new Set());
        }}
      />
      Show inactive
    </label>
  );

  const headerCheckbox = (
    <Checkbox
      aria-label="Select all"
      checked={allPicked ? true : picked.length > 0 ? "indeterminate" : false}
      onCheckedChange={toggleAll}
    />
  );

  const rowCheckbox = (r: T) => (
    <Checkbox aria-label={`Select ${opts.label(r)}`} checked={selected.has(id(r))} onCheckedChange={() => toggle(r)} />
  );

  // Shown in place of Edit on an inactive row.
  const reactivateButton = (r: T) => (
    <Button size="xs" variant="outline" disabled={setStatus.isPending} onClick={() => reactivate([r])}>
      Reactivate
    </Button>
  );

  const selectionBar: ReactNode =
    picked.length > 0 ? (
      <div className="flex items-center justify-between rounded-md border bg-card px-3 py-1.5">
        <span>{picked.length} selected</span>
        <div className="flex gap-2">
          <Button size="xs" variant="outline" onClick={() => setSelected(new Set())}>Clear</Button>
          {deactivating ? (
            <Button size="xs" variant="destructive" onClick={() => setConfirming(picked.filter((r) => isActive(r.status)))}>
              Deactivate {picked.filter((r) => isActive(r.status)).length}
            </Button>
          ) : (
            <Button size="xs" disabled={setStatus.isPending} onClick={() => reactivate(picked)}>
              Reactivate {picked.length}
            </Button>
          )}
        </div>
      </div>
    ) : null;

  const confirmModal = (
    <Dialog open={!!confirming} onOpenChange={(o) => { if (!o) setConfirming(null); }}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Deactivate</DialogTitle>
          <DialogDescription>
            These are set to {INACTIVE[opts.resource]} and hidden. Their history stays; "Show inactive" brings them back.
          </DialogDescription>
        </DialogHeader>
        <ul className="num flex flex-col gap-0.5">
          {(confirming ?? []).map((r) => <li key={id(r)}>{opts.label(r)}</li>)}
        </ul>
        <DialogFooter>
          <Button variant="outline" onClick={() => setConfirming(null)}>Cancel</Button>
          <Button
            variant="destructive"
            disabled={setStatus.isPending}
            onClick={() => confirming && setStatus.mutate({ targets: confirming, status: INACTIVE[opts.resource] })}
          >
            Deactivate {confirming?.length}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );

  return {
    visible,
    isActive: (r: T) => isActive(r.status),
    switchControl,
    headerCheckbox,
    rowCheckbox,
    reactivateButton,
    selectionBar,
    confirmModal,
  };
}
