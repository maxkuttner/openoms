import { NavLink, Outlet, useLocation } from "react-router-dom";
import { ADMIN_TABS, adminTabFor } from "../nav";
import { cn } from "@/lib/utils";
import { useSession } from "./session";
import { useOpenBreaks } from "../pages/admin/Breaks";

/** Shown in the shell when a non-admin opens an admin URL. */
export function NoAccess() {
  return (
    <div className="grid h-full place-items-center">
      <p className="text-muted-foreground">You don't have access to the Admin Panel.</p>
    </div>
  );
}

function CountBadge({ n }: { n: number }) {
  if (n === 0) return null;
  return (
    <span className="ml-1.5 rounded-full bg-sell px-1.5 py-px text-[10px] font-medium text-white" aria-label={`${n} open breaks`}>
      {n}
    </span>
  );
}

/** The Admin Panel: the section is picked in the sidebar; a sub-nav for its pages, the page below. */
export function AdminPanel() {
  const { admin } = useSession();
  const { pathname } = useLocation();
  const openBreaks = useOpenBreaks(admin).length;
  if (!admin) return <NoAccess />;

  const active = adminTabFor(pathname) ?? ADMIN_TABS[0];
  return (
    <div className="flex flex-col gap-3">
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
              {p.id === "breaks" && <CountBadge n={openBreaks} />}
            </NavLink>
          ))}
        </nav>
      )}
      <Outlet />
    </div>
  );
}
