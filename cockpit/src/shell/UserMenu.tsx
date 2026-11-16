import { Link } from "react-router-dom";
import { ChevronsUpDown, ExternalLink, LogOut, Settings, Shield } from "lucide-react";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useSession } from "./session";

/** Sidebar foot: who is signed in, and Settings · Admin Panel (admins) · Sign out. */
export function UserMenu({ onSignOut }: { onSignOut: () => void }) {
  const { me, admin } = useSession();
  const name = me.display_name ?? me.code;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
        <span className="grid size-6 shrink-0 place-items-center rounded-full bg-accent text-xs font-semibold uppercase text-foreground">
          {name.slice(0, 1)}
        </span>
        <span className="min-w-0 flex-1 truncate text-foreground">{name}</span>
        <ChevronsUpDown className="size-3.5 text-muted-foreground" />
      </DropdownMenuTrigger>
      <DropdownMenuContent side="top" align="start" className="w-52">
        <DropdownMenuLabel className="truncate text-xs text-muted-foreground">{me.code}</DropdownMenuLabel>
        <DropdownMenuItem asChild><Link to="/settings"><Settings /> Settings</Link></DropdownMenuItem>
        {admin && <DropdownMenuItem asChild><Link to="/admin"><Shield /> Admin Panel</Link></DropdownMenuItem>}
        <DropdownMenuItem asChild>
          <a href="https://openoms.dev/" target="_blank" rel="noreferrer"><ExternalLink /> Docs</a>
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem onSelect={onSignOut}><LogOut /> Sign out</DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
