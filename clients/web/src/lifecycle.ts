// Deactivate instead of delete: principals, portfolios, accounts and broker
// connections are never removed from the cockpit, only switched off, so the
// orders and grants that point at them keep their history. Each table has its
// own word for "off" (the CHECK constraints in db/migrations/ods/oms/).

export type Resource = "principal" | "portfolio" | "account" | "brokerConnection";

// The status a row gets when deactivated; reactivating always sets ACTIVE.
export const INACTIVE: Record<Resource, string> = {
  principal: "DISABLED",
  portfolio: "CLOSED",
  account: "CLOSED",
  brokerConnection: "DISABLED",
};

const STATUSES: Record<Resource, string[]> = {
  principal: ["ACTIVE", "DISABLED"],
  portfolio: ["ACTIVE", "CLOSED"],
  account: ["ACTIVE", "SUSPENDED", "CLOSED"],
  brokerConnection: ["ACTIVE", "DISABLED"],
};

export function isActive(status: string): boolean {
  return status === "ACTIVE";
}

// Inactive rows are hidden unless the "Show inactive" switch is on.
export function visibleRows<T extends { status: string }>(rows: T[], showInactive: boolean): T[] {
  return showInactive ? rows : rows.filter((r) => isActive(r.status));
}

export function statusOptions(resource: Resource): { value: string; label: string }[] {
  return STATUSES[resource].map((s) => ({ value: s, label: s }));
}
