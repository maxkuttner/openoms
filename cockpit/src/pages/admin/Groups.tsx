import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription,
  AlertDialogFooter, AlertDialogHeader, AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import type { Portfolio } from "../../api/types";
import { resolveGrants, toggleGrant } from "../../accessGrants";
import { ACCESS_OPTIONS, accessTableBody, groupSummary, type Access } from "../../groups";
import { ConfirmButton } from "../../components/ConfirmButton";
import { MultiSelect, Select, TextField } from "../../components/form";
import { Pill } from "../../components/status";

const PATH = "/admin/groups";
const DEFAULT_PATH = "/admin/grants/default";
const CATALOG_PATH = "/admin/grants/catalog";
const DEFAULT_ID = "default";

type GroupRow = {
  id: string;
  name: string;
  description: string | null;
  strategy_code: string | null;
  strategy_archived: boolean;
  members: string[];
  portfolios: { portfolio_id: string; code: string; access: Access }[];
};
type UserRef = { principal_id: string; code: string; display_name: string | null; email: string | null; status: string };
type CatalogItem = { key: string; label: string; category: string };
type GrantsBody = { grants: string[] };
type PreviewItem = { key: string; label: string; granted: boolean; from: "default" | "group" | null };

type Dirty = (dirty: boolean) => void;
/** Tell the editor whether this tab holds unsaved changes. */
function useReportDirty(dirty: boolean, onDirty: Dirty) {
  useEffect(() => { onDirty(dirty); }, [dirty, onDirty]);
}
const errText = (e: unknown) => (e instanceof Error ? e.message : String(e));

const userLabel = (u: UserRef) => u.display_name ?? u.email ?? u.code;
const grantsPath = (id: string) => (id === DEFAULT_ID ? DEFAULT_PATH : `${PATH}/${id}/grants`);
const previewPath = (id: string) => `${PATH}/${id}/preview`;

function SaveBar({ onSave, pending, disabled }: { onSave: () => void; pending: boolean; disabled?: boolean }) {
  return (
    <div className="mt-4 flex justify-end">
      <Button size="sm" onClick={onSave} disabled={pending || disabled}>Save</Button>
    </div>
  );
}

/** Grant switches by category. Grants from the default are on and locked in a group. */
function GrantsTab({ id, onDirty }: { id: string; onDirty: Dirty }) {
  const isDefault = id === DEFAULT_ID;
  const catalog = useQuery({ queryKey: [CATALOG_PATH], queryFn: () => api.get<CatalogItem[]>(CATALOG_PATH) });
  const defaults = useQuery({ queryKey: [DEFAULT_PATH], queryFn: () => api.get<GrantsBody>(DEFAULT_PATH) });
  const own = useQuery({ queryKey: [grantsPath(id)], queryFn: () => api.get<GrantsBody>(grantsPath(id)) });
  const qc = useQueryClient();
  const [draft, setDraft] = useState<string[] | null>(null);
  useReportDirty(draft !== null, onDirty);
  const save = useApiMutation((grants: string[]) => api.put(grantsPath(id), { grants }), {
    invalidate: [grantsPath(id), DEFAULT_PATH, previewPath(id)], success: "Grants saved",
    onDone: () => {
      setDraft(null);
      // Default grants feed every group's grants and preview.
      if (id === DEFAULT_ID) qc.invalidateQueries({ predicate: (q) => String(q.queryKey[0]).startsWith(`${PATH}/`) });
    },
  });
  const failed = catalog.error ?? defaults.error ?? own.error;
  if (failed) return <p className="text-destructive">Could not load grants: {errText(failed)}</p>;
  if (!catalog.data || !defaults.data || !own.data) return <p className="text-muted-foreground">Loading…</p>;
  const current = draft ?? own.data.grants;
  const inherited = new Set(isDefault ? [] : resolveGrants(defaults.data.grants, []).map((g) => g.key));
  const on = new Set(resolveGrants(isDefault ? [] : defaults.data.grants, current).map((g) => g.key));
  const categories = [...new Set(catalog.data.map((c) => c.category))];
  return (
    <div className="flex flex-col gap-4">
      <p className="text-muted-foreground">
        {isDefault
          ? "Turn on what every user may use. A group can add grants, never remove these."
          : "Members get these on top of the default grants. Locked items are already granted by default."}
      </p>
      {categories.map((cat) => (
        <div key={cat} className="flex flex-col gap-1">
          <h3 className="text-xs font-semibold text-muted-foreground uppercase">{cat}</h3>
          {catalog.data.filter((c) => c.category === cat).map((c) => (
            <div key={c.key} className="flex min-h-9 items-center gap-3 rounded-md px-2 hover:bg-muted/50">
              <label htmlFor={`grant-${c.key}`} className="flex-1">{c.label}</label>
              {inherited.has(c.key) && <Badge variant="secondary">from default</Badge>}
              <Switch id={`grant-${c.key}`} checked={on.has(c.key)} disabled={inherited.has(c.key)}
                onCheckedChange={(v) => setDraft(toggleGrant(current, c.key, v))} />
            </div>
          ))}
        </div>
      ))}
      <SaveBar onSave={() => save.mutate(current)} pending={save.isPending} disabled={draft === null} />
    </div>
  );
}

function PreviewTab({ id }: { id: string }) {
  const preview = useQuery({ queryKey: [previewPath(id)], queryFn: () => api.get<{ grants: PreviewItem[] }>(previewPath(id)) });
  if (preview.error) return <p className="text-destructive">Could not load preview: {errText(preview.error)}</p>;
  return (
    <div className="flex flex-col gap-2">
      <p className="text-muted-foreground">
        What members end up with: default grants plus every group they are in (any "on" wins). Admins skip grant checks.
      </p>
      {(preview.data?.grants ?? []).map((g) => (
        <div key={g.key} className="flex min-h-8 items-center justify-between rounded-md px-2 hover:bg-muted/50">
          <span>{g.label}</span>
          {g.granted
            ? <span className="text-xs text-buy">Granted{g.from ? ` · ${g.from}` : ""}</span>
            : <span className="text-xs text-muted-foreground">—</span>}
        </div>
      ))}
    </div>
  );
}

function GeneralTab({ group, onSaved, onDeleted, onDirty }: { group: GroupRow | null; onSaved: (id: string) => void; onDeleted: () => void; onDirty: Dirty }) {
  const [nameDraft, setName] = useState<string | null>(null);
  const [descDraft, setDescription] = useState<string | null>(null);
  const name = nameDraft ?? group?.name ?? "";
  const description = descDraft ?? group?.description ?? "";
  useReportDirty(nameDraft !== null || descDraft !== null, onDirty);
  const save = useApiMutation(
    async () => {
      const body = { name: name.trim(), description: description.trim() || null };
      if (group) { await api.put(`${PATH}/${group.id}`, body); return group.id; }
      return (await api.post<{ id: string }>(PATH, body)).id;
    },
    { invalidate: [PATH, "/admin/users"], success: "Group saved", onDone: (id) => { setName(null); setDescription(null); onSaved(id as string); } },
  );
  const remove = useApiMutation(() => api.del(`${PATH}/${group!.id}`), {
    invalidate: [PATH, "/admin/users"], success: "Group deleted", onDone: () => { setName(null); setDescription(null); onDeleted(); },
  });
  return (
    <div className="flex flex-col gap-3">
      <TextField label="Name" required value={name} onChange={setName} />
      <TextField label="Description" value={description} onChange={setDescription} />
      {group?.strategy_code && (
        <p className="text-xs text-muted-foreground">
          Belongs to reported strategy {group.strategy_code}: its owner stays a member, and the group can be deleted only once the strategy is archived.
        </p>
      )}
      <div className="flex items-center justify-between">
        {group ? (
          <ConfirmButton size="sm" variant="destructive" disabled={!!group.strategy_code && !group.strategy_archived}
            title={`Delete ${group.name}?`} description="Its members lose the access it gives."
            confirmLabel="Delete" onConfirm={() => remove.mutate(undefined)}>Delete group</ConfirmButton>
        ) : <span />}
        <Button size="sm" onClick={() => save.mutate(undefined)} disabled={save.isPending || !name.trim()}>Save</Button>
      </div>
    </div>
  );
}

function UsersTab({ group, onDirty }: { group: GroupRow; onDirty: Dirty }) {
  const users = useList<UserRef>("/admin/users");
  const [draft, setMembers] = useState<string[] | null>(null);
  const members = draft ?? group.members;
  useReportDirty(draft !== null, onDirty);
  const save = useApiMutation(() => api.put(`${PATH}/${group.id}/members`, members), {
    invalidate: [PATH, "/admin/users"], success: "Members saved", onDone: () => setMembers(null),
  });
  return (
    <div className="flex flex-col gap-3">
      <MultiSelect label="Members" value={members} onChange={setMembers} placeholder="Add a person…"
        data={(users.data ?? []).filter((u) => u.status !== "DISABLED" || members.includes(u.principal_id))
          .map((u) => ({ value: u.principal_id, label: userLabel(u) }))} />
      {group.strategy_code && <p className="text-xs text-muted-foreground">The strategy owner stays a member.</p>}
      <SaveBar onSave={() => save.mutate(undefined)} pending={save.isPending} />
    </div>
  );
}

/** Per-portfolio access level for this group: unchecked = none. */
function PortfoliosTab({ group, onDirty }: { group: GroupRow; onDirty: Dirty }) {
  const portfolios = useList<Portfolio>("/admin/portfolios");
  const [draft, setValue] = useState<Record<string, Access> | null>(null);
  const value = draft ?? Object.fromEntries(group.portfolios.map((p) => [p.portfolio_id, p.access]));
  useReportDirty(draft !== null, onDirty);
  const save = useApiMutation(() => api.put(`${PATH}/${group.id}/portfolios`, accessTableBody(value)), {
    invalidate: [PATH], success: "Portfolio access saved", onDone: () => setValue(null),
  });
  return (
    <div className="flex flex-col gap-1.5">
      <p className="text-muted-foreground">Portfolios this group can reach. Set it here or on the portfolio's Access tab: same list.</p>
      {(portfolios.data ?? []).map((p) => (
        <div key={p.id} className="flex h-8 items-center gap-3">
          <label className="flex w-48 items-center gap-2">
            <Checkbox checked={p.id in value}
              onCheckedChange={(on) => {
                const next = { ...value };
                if (on === true) next[p.id] = "view";
                else delete next[p.id];
                setValue(next);
              }} />
            {p.code}
          </label>
          {p.id in value && (
            <Select className="w-40" data={ACCESS_OPTIONS} value={value[p.id]}
              onChange={(v) => v && setValue({ ...value, [p.id]: v as Access })} />
          )}
        </div>
      ))}
      {portfolios.data?.length === 0 && <p className="text-muted-foreground">No portfolios yet.</p>}
      {group.strategy_code && <p className="text-xs text-muted-foreground">The strategy's portfolio stays on this group.</p>}
      <SaveBar onSave={() => save.mutate(undefined)} pending={save.isPending} />
    </div>
  );
}

function Editor({ selected, group, onSelect, onDirtyChange }: { selected: string; group: GroupRow | null; onSelect: (id: string | null) => void; onDirtyChange: Dirty }) {
  const isDefault = selected === DEFAULT_ID;
  const isNew = selected === "new";
  const [tab, setTab] = useState(isNew ? "general" : "grants");
  const [dirtyTabs, setDirtyTabs] = useState<Record<string, boolean>>({});
  const markers = useMemo(() => {
    const mark = (t: string): Dirty => (d) => setDirtyTabs((m) => (m[t] === d ? m : { ...m, [t]: d }));
    return { general: mark("general"), users: mark("users"), portfolios: mark("portfolios"), grants: mark("grants") };
  }, []);
  const anyDirty = Object.values(dirtyTabs).some(Boolean);
  useEffect(() => { onDirtyChange(anyDirty); }, [anyDirty, onDirtyChange]);
  const title = isDefault ? "Default grants" : isNew ? "New group" : group?.name;
  const sub = isDefault ? "Applies to every user. Groups can only add to this." : group ? groupSummary(group) : "";
  return (
    <section aria-label="Group editor" className="min-w-0 flex-1 rounded-md border p-4">
      <div className="mb-3 flex items-baseline gap-3">
        <h2 className="text-sm font-semibold">{title}</h2>
        <span className="text-xs text-muted-foreground">{sub}</span>
      </div>
      {isDefault ? <GrantsTab id={DEFAULT_ID} onDirty={markers.grants} /> : (
        <Tabs value={tab} onValueChange={setTab}>
          <TabsList>
            <TabsTrigger value="general">General</TabsTrigger>
            {!isNew && <TabsTrigger value="users">Users</TabsTrigger>}
            {!isNew && <TabsTrigger value="portfolios">Portfolios</TabsTrigger>}
            {!isNew && <TabsTrigger value="grants">Grants</TabsTrigger>}
            {!isNew && <TabsTrigger value="preview">Preview</TabsTrigger>}
          </TabsList>
          <div className="mt-4">
            <TabsContent forceMount value="general" className="data-[state=inactive]:hidden"><GeneralTab group={group} onSaved={(id) => onSelect(id)} onDeleted={() => onSelect(null)} onDirty={markers.general} /></TabsContent>
            {group && <TabsContent forceMount value="users" className="data-[state=inactive]:hidden"><UsersTab group={group} onDirty={markers.users} /></TabsContent>}
            {group && <TabsContent forceMount value="portfolios" className="data-[state=inactive]:hidden"><PortfoliosTab group={group} onDirty={markers.portfolios} /></TabsContent>}
            {group && <TabsContent forceMount value="grants" className="data-[state=inactive]:hidden"><GrantsTab id={group.id} onDirty={markers.grants} /></TabsContent>}
            {group && <TabsContent value="preview"><PreviewTab id={group.id} /></TabsContent>}
          </div>
        </Tabs>
      )}
    </section>
  );
}

/** Groups: who gets which portfolio access and which grants (a person's highest access and any "on" grant wins). */
export function GroupsPage() {
  const list = useList<GroupRow>(PATH);
  const [selected, setSelected] = useState<string>(DEFAULT_ID);
  const [dirty, setDirty] = useState(false);
  const [pending, setPending] = useState<string | null>(null);
  const groups = list.data ?? [];
  const group = groups.find((g) => g.id === selected) ?? null;
  const effective = selected === DEFAULT_ID || selected === "new" || group ? selected : DEFAULT_ID;
  // Switching away from unsaved edits asks first.
  const go = (id: string) => { if (id === effective) return; if (dirty) setPending(id); else setSelected(id); };
  const row = (id: string, name: React.ReactNode, sub: React.ReactNode, extra?: string) => (
    <button key={id} type="button" onClick={() => go(id)} aria-current={effective === id || undefined}
      className={cn("flex min-h-12 flex-col items-start justify-center rounded-md border border-transparent px-2.5 py-1.5 text-left hover:bg-muted/50",
        effective === id && "border-border bg-muted", extra)}>
      <span>{name}</span>
      <span className="text-xs text-muted-foreground">{sub}</span>
    </button>
  );
  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-col gap-4 md:flex-row md:items-start">
        <section aria-label="Groups" className="flex w-full shrink-0 flex-col gap-1 md:w-64">
          <div className="mb-1 flex items-center justify-between">
            <h2 className="text-sm font-semibold">Groups</h2>
            <Button size="sm" onClick={() => go("new")}>New group</Button>
          </div>
          {list.error && <p className="text-destructive">Could not load groups: {errText(list.error)}</p>}
          {row(DEFAULT_ID, "Default grants", "Every user starts here", "mb-1 border-b-border")}
          {groups.map((g) => row(g.id,
            <>{g.name}{g.strategy_code && <Pill tone="info" className="ml-1.5">strategy {g.strategy_code}{g.strategy_archived ? " · archived" : ""}</Pill>}</>,
            groupSummary(g)))}
          {!list.error && groups.length === 0 && <p className="px-2.5 text-muted-foreground">No groups yet.</p>}
        </section>
        <Editor key={effective} selected={effective} group={group} onSelect={(id) => setSelected(id ?? DEFAULT_ID)} onDirtyChange={setDirty} />
      </div>
      <AlertDialog open={pending !== null} onOpenChange={(o) => !o && setPending(null)}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Discard unsaved changes?</AlertDialogTitle>
            <AlertDialogDescription>Edits you have not saved will be lost.</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>Keep editing</AlertDialogCancel>
            <AlertDialogAction variant="destructive" onClick={() => { setSelected(pending!); setPending(null); }}>Discard</AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </div>
  );
}
