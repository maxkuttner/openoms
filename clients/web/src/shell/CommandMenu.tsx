import { useEffect } from "react";
import { useNavigate } from "react-router-dom";
import {
  Command,
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import { ADMIN_TABS, commandItems } from "../nav";

/** ⌘K / Ctrl+K: every page this user may open, plus sign out. */
export function CommandMenu({
  open,
  setOpen,
  admin,
  onSignOut,
}: {
  open: boolean;
  setOpen: (open: boolean | ((o: boolean) => boolean)) => void;
  admin: boolean;
  onSignOut: () => void;
}) {
  const navigate = useNavigate();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [setOpen]);

  const items = commandItems(admin);
  const adminIds = new Set(
    ADMIN_TABS.flatMap((t) => t.pages.map((p) => p.path)),
  );
  const go = (path: string) => {
    setOpen(false);
    navigate(path);
  };

  return (
    <CommandDialog
      open={open}
      onOpenChange={setOpen}
      title="Go to"
      description="Jump to a page or action"
    >
      {/* This CommandDialog does not provide the cmdk root itself. */}
      <Command>
        <CommandInput placeholder="Go to…" />
        <CommandList>
          <CommandEmpty>Nothing matches.</CommandEmpty>
          <CommandGroup heading="Pages">
            {items
              .filter((p) => !adminIds.has(p.path))
              .map((p) => (
                <CommandItem key={p.path} onSelect={() => go(p.path)}>
                  {p.title}
                </CommandItem>
              ))}
          </CommandGroup>
          {admin && (
            <CommandGroup heading="Admin Panel">
              {items
                .filter((p) => adminIds.has(p.path))
                .map((p) => (
                  <CommandItem
                    key={p.path}
                    value={`admin ${p.title}`}
                    onSelect={() => go(p.path)}
                  >
                    {p.title}
                  </CommandItem>
                ))}
            </CommandGroup>
          )}
          <CommandSeparator />
          <CommandGroup heading="Account">
            <CommandItem
              onSelect={() => {
                setOpen(false);
                onSignOut();
              }}
            >
              Sign out
            </CommandItem>
          </CommandGroup>
        </CommandList>
      </Command>
    </CommandDialog>
  );
}
