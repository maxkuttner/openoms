// Pure helpers for the Users page.

/** Account roles (Open WebUI's admin / user / pending). */
export const ACCOUNT_ROLE_OPTIONS = [
  { value: "user", label: "User" },
  { value: "admin", label: "Admin" },
  { value: "pending", label: "Pending" },
];

export function accountRoleLabel(role: string): string {
  return ACCOUNT_ROLE_OPTIONS.find((r) => r.value === role)?.label ?? role;
}

/** Accounts waiting for an admin, oldest-listed first as the API gives them. */
export function pendingUsers<T extends { account_role: string }>(rows: T[]): T[] {
  return rows.filter((u) => u.account_role === "pending");
}

/** Users whose name, email or code contains `q` (case-insensitive); all for a blank query. */
export function filterUsers<T extends { code: string; display_name: string | null; email: string | null }>(rows: T[], q: string): T[] {
  const needle = q.trim().toLowerCase();
  if (!needle) return rows;
  return rows.filter((u) => [u.code, u.display_name, u.email].some((v) => v?.toLowerCase().includes(needle)));
}

/** The first `max` group names, and "+N" for the rest ("" when none are hidden). */
export function groupBadges(names: string[], max = 2): { shown: string[]; more: string } {
  return { shown: names.slice(0, max), more: names.length > max ? `+${names.length - max}` : "" };
}

/** "—", "1 portfolio", "3 portfolios". */
export function portfolioCount(n: number): string {
  return n === 0 ? "—" : `${n} portfolio${n === 1 ? "" : "s"}`;
}
