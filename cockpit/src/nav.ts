// The merged app's navigation: the sidebar every user sees, the account
// pages behind the user menu, and the Admin Panel's tabs (admins only).
// Paths are relative to the router base (/app/ in production).

export type Page = { id: string; title: string; path: string };
export type AdminTab = { id: "people" | "setup" | "connectivity" | "monitoring"; title: string; pages: Page[] };

export const SIDEBAR: Page[] = [
  { id: "trade", title: "Trade", path: "/" },
  { id: "positions", title: "Positions", path: "/positions" },
];

export const ACCOUNT_PAGES: Page[] = [
  { id: "settings", title: "Settings", path: "/settings" },
];

const admin = (id: string, title: string): Page => ({ id, title, path: `/admin/${id}` });

export const ADMIN_TABS: AdminTab[] = [
  { id: "people", title: "People", pages: [admin("users", "Users"), admin("groups", "Groups"), admin("sign-in", "Sign-in"), admin("api", "API")] },
  {
    id: "setup",
    title: "Trading setup",
    pages: [
      admin("portfolios", "Portfolios"),
      admin("accounts", "Accounts"),
      admin("strategies", "Strategies"),
      admin("risk-limits", "Risk limits"),
      admin("instruments", "Instruments"),
    ],
  },
  { id: "connectivity", title: "Connectivity", pages: [admin("broker-connections", "Broker connections"), admin("data-feeds", "Data feeds")] },
  { id: "monitoring", title: "Monitoring", pages: [admin("blotter", "Blotter"), admin("breaks", "Breaks")] },
];

const isAdminPath = (path: string) => path === "/admin" || path.startsWith("/admin/");

export function adminTabFor(path: string): AdminTab | null {
  const p = path.replace(/\/+$/, "");
  return ADMIN_TABS.find((t) => t.pages.some((pg) => p === pg.path || p.startsWith(pg.path + "/"))) ?? null;
}

export function adminRouteAllowed(path: string, isAdmin: boolean): boolean {
  return isAdmin || !isAdminPath(path);
}

export function commandItems(isAdmin: boolean): Page[] {
  const pages = [...SIDEBAR, ...ACCOUNT_PAGES];
  return isAdmin ? [...pages, ...ADMIN_TABS.flatMap((t) => t.pages)] : pages;
}

/** The page a router path belongs to (for the top bar title), or null. */
export function pageFor(path: string): Page | null {
  const p = path.length > 1 ? path.replace(/\/+$/, "") : path;
  const all = [...SIDEBAR, ...ACCOUNT_PAGES, ...ADMIN_TABS.flatMap((t) => t.pages)];
  return all.find((pg) => pg.path === p) ?? all.find((pg) => pg.path !== "/" && p.startsWith(pg.path + "/")) ?? null;
}
