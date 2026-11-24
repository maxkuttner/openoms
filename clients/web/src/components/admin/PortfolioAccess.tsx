import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { api } from "../../api/client";
import { useApiMutation, useList } from "../../api/hooks";
import { ACCESS_OPTIONS, type Access } from "../../groups";
import { addEntry, removeEntry, setLevel, type AccessEntry } from "../../portfolioAccess";

export const accessPath = (portfolioId: string) => `/admin/portfolios/${portfolioId}/access`;

type GroupRef = { id: string; name: string };
type UserRef = { principal_id: string; code: string; display_name: string | null; email: string | null; status: string };

/** Access tab: groups and single people on a portfolio, each at View, Trade or Manage. */
export function PortfolioAccess({ portfolioId, onDirty }: { portfolioId: string; onDirty: (dirty: boolean) => void }) {
  const path = accessPath(portfolioId);
  const query = useQuery<AccessEntry[]>({ queryKey: [path], queryFn: () => api.get<AccessEntry[]>(path) });
  const groups = useList<GroupRef>("/admin/groups");
  const users = useList<UserRef>("/admin/users");
  // Not dirty: the list is the server's data, so a refetch shows through.
  const [edited, setEdited] = useState<AccessEntry[] | null>(null);
  const [open, setOpen] = useState(false);
  useEffect(() => { onDirty(edited !== null); }, [edited, onDirty]);
  const save = useApiMutation((list: AccessEntry[]) => api.put(path, list), {
    invalidate: [path], success: "Access saved", onDone: () => setEdited(null),
  });

  if (query.error) return <p className="text-destructive">Could not load access: {query.error instanceof Error ? query.error.message : String(query.error)}</p>;
  if (!query.data) return <p className="text-muted-foreground">Loading…</p>;
  const list = edited ?? query.data;
  const listed = (kind: string, id: string) => list.some((e) => e.kind === kind && e.id === id);
  const add = (who: Pick<AccessEntry, "kind" | "id" | "name">) => { setOpen(false); setEdited(addEntry(list, who)); };
  return (
    <div className="flex flex-col gap-2">
      <p className="text-muted-foreground">
        Who can reach this portfolio. Groups or single people; the highest level wins. What they may <em>do</em> also depends on their grants.
      </p>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button variant="outline" size="sm" role="combobox" aria-expanded={open} className="w-full justify-start text-muted-foreground">
            Add a group or person…
          </Button>
        </PopoverTrigger>
        <PopoverContent align="start" className="w-80 p-0">
          <Command>
            <CommandInput placeholder="Group or person…" />
            <CommandList>
              <CommandEmpty>No match.</CommandEmpty>
              <CommandGroup heading="Groups">
                {(groups.data ?? []).filter((g) => !listed("group", g.id)).map((g) => (
                  <CommandItem key={g.id} value={`group ${g.name}`} onSelect={() => add({ kind: "group", id: g.id, name: g.name })}>{g.name}</CommandItem>
                ))}
              </CommandGroup>
              <CommandGroup heading="People">
                {(users.data ?? []).filter((u) => u.status === "ACTIVE" && !listed("person", u.principal_id)).map((u) => {
                  const name = u.display_name ?? u.email ?? u.code;
                  return <CommandItem key={u.principal_id} value={`person ${name} ${u.code}`} onSelect={() => add({ kind: "person", id: u.principal_id, name })}>{name}</CommandItem>;
                })}
              </CommandGroup>
            </CommandList>
          </Command>
        </PopoverContent>
      </Popover>
      {list.length === 0 && <p className="text-muted-foreground">Nobody has access yet.</p>}
      {list.map((e) => (
        <div key={`${e.kind}:${e.id}`} className="flex min-h-11 items-center gap-2.5 border-t">
          <span className="rounded border px-1.5 text-[10px] uppercase text-muted-foreground">{e.kind}</span>
          <span className="flex-1 truncate">{e.name}</span>
          <ToggleGroup type="single" variant="outline" size="sm" value={e.access} aria-label={`Access for ${e.name}`}
            onValueChange={(v) => v && setEdited(setLevel(list, e.kind, e.id, v as Access))}>
            {ACCESS_OPTIONS.map((o) => <ToggleGroupItem key={o.value} value={o.value}>{o.label.replace(/ \(.*\)$/, "")}</ToggleGroupItem>)}
          </ToggleGroup>
          <Button size="icon-sm" variant="ghost" aria-label={`Remove ${e.name}`} onClick={() => setEdited(removeEntry(list, e.kind, e.id))}><X /></Button>
        </div>
      ))}
      <div className="mt-2 flex justify-end">
        <Button size="sm" disabled={edited === null || save.isPending} onClick={() => save.mutate(list)}>Save</Button>
      </div>
    </div>
  );
}
