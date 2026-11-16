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
