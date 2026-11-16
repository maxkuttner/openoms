// Pure helpers for groups: the way people get portfolio rights.

export type Access = "view" | "trade" | "manage";

export const ACCESS_OPTIONS: { value: Access; label: string }[] = [
  { value: "view", label: "View (viewer)" },
  { value: "trade", label: "Trade (trader)" },
  { value: "manage", label: "Manage (PM)" },
];

const RANK: Record<Access, number> = { view: 1, trade: 2, manage: 3 };

/** A person's access on a portfolio: the highest across their groups (as the server decides). */
export function highestAccess(levels: Access[]): Access | null {
  return levels.reduce<Access | null>((best, a) => (best === null || RANK[a] > RANK[best] ? a : best), null);
}

export function accessLabel(a: string): string {
  return ACCESS_OPTIONS.find((o) => o.value === a)?.label ?? a;
}

/** Access-table state (portfolio id -> level, absent = none) to the API body. */
export function accessTableBody(picked: Record<string, Access>): { portfolio_id: string; access: Access }[] {
  return Object.entries(picked).map(([portfolio_id, access]) => ({ portfolio_id, access }));
}

/** "3 members · 2 portfolios" for a group row. */
export function groupSummary(g: { members: unknown[]; portfolios: unknown[] }): string {
  const n = (k: number, one: string) => `${k} ${one}${k === 1 ? "" : "s"}`;
  return `${n(g.members.length, "member")} · ${n(g.portfolios.length, "portfolio")}`;
}
