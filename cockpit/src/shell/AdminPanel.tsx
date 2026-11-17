import { NavLink, Outlet, useLocation } from "react-router-dom";
import { ADMIN_TABS, adminTabFor } from "../nav";
import { SetupChecklist } from "../components/SetupChecklist";
import { cn } from "@/lib/utils";
import { useSession } from "./session";

/** Shown in the shell when a non-admin opens an admin URL. */
export function NoAccess() {
  return (
    <div className="grid h-full place-items-center">
      <p className="text-muted-foreground">You don't have access to the Admin Panel.</p>
    </div>
  );
}

/** The Admin Panel: four tabs across the top, a sub-nav per tab, the page below. */
export function AdminPanel() {
  const { admin } = useSession();
  const { pathname } = useLocation();
  if (!admin) return <NoAccess />;

  const active = adminTabFor(pathname) ?? ADMIN_TABS[0];
  return (
    <div className="flex flex-col gap-3">
      <SetupChecklist />
      <nav className="flex gap-1 border-b" aria-label="Admin sections">
        {ADMIN_TABS.map((t) => (
          <NavLink
            key={t.id}
            to={t.pages[0].path}
            className={cn(
              "-mb-px border-b-2 border-transparent px-3 py-1.5 text-foreground-2 hover:text-foreground",
              t.id === active.id && "border-primary text-foreground",
            )}
          >
            {t.title}
          </NavLink>
        ))}
      </nav>
      {active.pages.length > 1 && (
        <nav className="flex gap-1" aria-label={`${active.title} pages`}>
          {active.pages.map((p) => (
            <NavLink
              key={p.path}
              to={p.path}
              className={({ isActive }) =>
                cn("rounded-md px-2 py-1 text-foreground-2 hover:bg-accent hover:text-foreground", isActive && "bg-accent text-foreground")
              }
            >
              {p.title}
            </NavLink>
          ))}
        </nav>
      )}
      <Outlet />
    </div>
  );
}
