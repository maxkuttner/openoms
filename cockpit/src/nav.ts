// The app's navigation in one table: the sidebar (Trade group, main pages,
// and the Admin group for admins), the account pages behind the user menu,
// and the Admin Panel's tabs. Sidebar, command menu and titles read it.
// Paths are relative to the router base (/app/ in production).

export type Page = { id: string; title: string; path: string };
export type AdminTab = { id: "people" | "setup" | "connectivity" | "monitoring"; title: string; pages: Page[] };

export type NavGroup = { id: "trade" | "admin"; title: string; pages: Page[] };

export const TRADE_GROUP: NavGroup = {
  id: "trade",
  title: "Trade",
  pages: [
    { id: "trade", title: "Trade entry", path: "/" },
    { id: "strategies", title: "Strategies", path: "/strategies" },
    { id: "api", title: "API", path: "/api" },
  ],
};

export const MAIN_PAGES: Page[] = [
  { id: "portfolios", title: "Portfolios", path: "/portfolios" },
  { id: "orders", title: "Orders", path: "/orders" },
  { id: "jobs", title: "Jobs", path: "/jobs" },
  { id: "events", title: "Events", path: "/events" },
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

/** One sidebar entry per Admin Panel tab, opening the tab's first page. */
export const ADMIN_GROUP: NavGroup = {
  id: "admin",
  title: "Admin",
  pages: ADMIN_TABS.map((t) => ({ id: t.id, title: t.title, path: t.pages[0].path })),
};

export function sidebarFor(isAdmin: boolean): { groups: NavGroup[]; pages: Page[] } {
  return { groups: isAdmin ? [TRADE_GROUP, ADMIN_GROUP] : [TRADE_GROUP], pages: MAIN_PAGES };
}

const LEGACY: Record<string, string> = { "/positions": "/portfolios", "/reported": "/strategies" };

/** Where a pre-redesign path now lives, or null. */
export function legacyRedirect(path: string): string | null {
  return LEGACY[path.replace(/\/+$/, "")] ?? null;
}

const isAdminPath = (path: string) => path === "/admin" || path.startsWith("/admin/");

export function adminTabFor(path: string): AdminTab | null {
  const p = path.replace(/\/+$/, "");
  return ADMIN_TABS.find((t) => t.pages.some((pg) => p === pg.path || p.startsWith(pg.path + "/"))) ?? null;
}

export function adminRouteAllowed(path: string, isAdmin: boolean): boolean {
  return isAdmin || !isAdminPath(path);
}

export function commandItems(isAdmin: boolean): Page[] {
  const pages = [...TRADE_GROUP.pages, ...MAIN_PAGES, ...ACCOUNT_PAGES];
  return isAdmin ? [...pages, ...ADMIN_TABS.flatMap((t) => t.pages)] : pages;
}

/** The page a router path belongs to (for the top bar title), or null. */
export function pageFor(path: string): Page | null {
  const p = path.length > 1 ? path.replace(/\/+$/, "") : path;
  const all = [...TRADE_GROUP.pages, ...MAIN_PAGES, ...ACCOUNT_PAGES, ...ADMIN_TABS.flatMap((t) => t.pages)];
  return all.find((pg) => pg.path === p) ?? all.find((pg) => pg.path !== "/" && p.startsWith(pg.path + "/")) ?? null;
}
