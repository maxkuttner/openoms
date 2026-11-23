import { useState } from "react";
import { NavLink, Outlet, useLocation } from "react-router-dom";
import { CandlestickChart, FileInput, Wallet } from "lucide-react";
import { authApi } from "../login/api";
import { goToLogin } from "../api/client";
import { Logo } from "../components/Logo";
import { JobsIndicator } from "../components/JobsIndicator";
import { SIDEBAR, pageFor } from "../nav";
import { cn } from "@/lib/utils";
import { CommandMenu } from "./CommandMenu";
import { ConnectionDot } from "./ConnectionDot";
import { UserMenu } from "./UserMenu";
import { useSession } from "./session";

const ICONS: Record<string, typeof Wallet> = { trade: CandlestickChart, positions: Wallet, reported: FileInput };

/** Sidebar (same for every user) + top bar + page. */
export function Shell() {
  const { admin } = useSession();
  const { pathname } = useLocation();
  const [cmdOpen, setCmdOpen] = useState(false);
  const title = pathname.startsWith("/admin") ? "Admin Panel" : (pageFor(pathname)?.title ?? "");

  const signOut = () => {
    void authApi.logout().catch(() => undefined).then(() => goToLogin());
  };

  return (
    <div className="flex h-screen overflow-hidden bg-background text-foreground">
      <aside className="flex w-[190px] shrink-0 flex-col border-r bg-sidebar px-2 py-2.5">
        <div className="flex items-center justify-between px-2 pb-3">
          <Logo />
          <button
            type="button"
            onClick={() => setCmdOpen(true)}
            className="rounded border px-1 text-[10px] text-muted-foreground hover:text-foreground"
            aria-label="Open command menu"
          >
            ⌘K
          </button>
        </div>
        <nav className="flex flex-col gap-px">
          {SIDEBAR.map((p) => {
            const Icon = ICONS[p.id];
            return (
              <NavLink
                key={p.path}
                to={p.path}
                end
                className={({ isActive }) =>
                  cn(
                    "flex items-center gap-2 rounded-md px-2 py-[5px] text-sidebar-foreground hover:bg-accent hover:text-foreground",
                    isActive && "bg-accent text-foreground",
                  )
                }
              >
                {Icon && <Icon className="size-3.5" />}
                {p.title}
              </NavLink>
            );
          })}
        </nav>
        <div className="mt-auto border-t pt-2">
          <UserMenu onSignOut={signOut} />
        </div>
      </aside>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="flex h-[38px] shrink-0 items-center gap-3 border-b px-3.5">
          <span className="font-semibold">{title}</span>
          <div className="ml-auto flex items-center gap-3">
            {admin && <JobsIndicator />}
            <ConnectionDot />
          </div>
        </header>
        <main className="min-h-0 flex-1 overflow-auto p-3.5">
          <Outlet />
        </main>
      </div>
      <CommandMenu open={cmdOpen} setOpen={setCmdOpen} admin={admin} onSignOut={signOut} />
    </div>
  );
}
