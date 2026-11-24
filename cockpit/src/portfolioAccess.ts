// Pure helpers for a portfolio's access list: groups and single people at a level.

import { accessLabel, type Access } from "./groups.ts";

export type AccessEntry = { kind: "group" | "person"; id: string; name: string; access: Access };

const same = (e: AccessEntry, kind: string, id: string) => e.kind === kind && e.id === id;

/** "Traders: Trade · Ana R.: Manage" for the table; a dash when nobody is listed. */
export function accessSummary(list: AccessEntry[]): string {
  if (list.length === 0) return "—";
  return list.map((e) => `${e.name}: ${accessLabel(e.access).replace(/ \(.*\)$/, "")}`).join(" · ");
}

/** Add a group or person at view; already listed means no change. */
export function addEntry(list: AccessEntry[], who: Pick<AccessEntry, "kind" | "id" | "name">): AccessEntry[] {
  if (list.some((e) => same(e, who.kind, who.id))) return list;
  return [...list, { ...who, access: "view" }];
}

export function setLevel(list: AccessEntry[], kind: string, id: string, access: Access): AccessEntry[] {
  return list.map((e) => (same(e, kind, id) ? { ...e, access } : e));
}

export function removeEntry(list: AccessEntry[], kind: string, id: string): AccessEntry[] {
  return list.filter((e) => !same(e, kind, id));
}

/** Venue connection codes for the table; a dash when none. */
export function venueSummary(codes: string[]): string {
  return codes.length === 0 ? "—" : codes.join(", ");
}
