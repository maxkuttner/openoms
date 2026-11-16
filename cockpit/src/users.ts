// Pure helpers for the Users page.

export type Access = "trade" | "view";

/** Portfolio roles: what a user may do in the portfolios they have access to. */
export const ROLE_OPTIONS = [
  { value: "pm", label: "PM" },
  { value: "trader", label: "Trader" },
  { value: "viewer", label: "Viewer" },
];

/** Account roles (Open WebUI's admin / user / pending). */
export const ACCOUNT_ROLE_OPTIONS = [
  { value: "user", label: "User" },
  { value: "admin", label: "Admin" },
  { value: "pending", label: "Pending" },
];

export function roleLabel(role: string | null): string {
  return ROLE_OPTIONS.find((r) => r.value === role)?.label ?? role ?? "—";
}

export function accountRoleLabel(role: string): string {
  return ACCOUNT_ROLE_OPTIONS.find((r) => r.value === role)?.label ?? role;
}

/** Accounts waiting for an admin, oldest-listed first as the API gives them. */
export function pendingUsers<T extends { account_role: string }>(rows: T[]): T[] {
  return rows.filter((u) => u.account_role === "pending");
}

/** Picker state (portfolio id -> access, absent = none) to the API body. */
export function accessBody(picked: Record<string, Access>): { portfolio_id: string; can_trade: boolean }[] {
  return Object.entries(picked).map(([portfolio_id, a]) => ({ portfolio_id, can_trade: a === "trade" }));
}
