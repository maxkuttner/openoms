// Pure futures-roll logic for the cockpit: root search entries, the roll badge,
// the alert banner and the roll dialog's defaults. Kept free of React so
// `node --test` covers it (the web package has no DOM test setup).

import type { AlertRow, FutureContract, FutureRoot, RollTargetRow } from "./api/types.ts";

export type { AlertRow, FutureContract, FutureRoot, RollTargetRow };

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** "2026-12-18" -> "Dec 2026" */
export function expiryMonth(isoDate: string): string {
  const [y, m] = isoDate.split("-");
  return `${MONTHS[Number(m) - 1] ?? m} ${y}`;
}

/** Today's date at the venue (CME: America/Chicago), as YYYY-MM-DD. Mirrors src/futures/front.rs venue_today. */
export function venueToday(now: Date = new Date()): string {
  return new Intl.DateTimeFormat("en-CA", { timeZone: "America/Chicago", year: "numeric", month: "2-digit", day: "2-digit" }).format(now);
}

const dayNumber = (iso: string) => Date.UTC(Number(iso.slice(0, 4)), Number(iso.slice(5, 7)) - 1, Number(iso.slice(8, 10))) / 86_400_000;

/** Whole calendar days from `from` to `to` (negative when `to` is earlier). */
export function daysUntil(from: string, to: string): number {
  return dayNumber(to) - dayNumber(from);
}

/** Flag a held contract once the venue's today reaches roll_date - roll_warn_days. */
export function rollBadgeDue(today: string, rollDate: string, warnDays: number): boolean {
  return daysUntil(today, rollDate) <= warnDays;
}

export interface RootEntry {
  root: string;
  label: string;
  instrument_id: number;
  symbol: string;
  venue: string;
  name: string;
}

/** Instrument-search entries for futures roots matching `query` ("ES → ESZ6 (Dec 2026)"), to list before contract hits. Picking one trades the front contract. */
export function searchRootEntries(query: string, roots: FutureRoot[]): RootEntry[] {
  const q = query.trim().toLowerCase();
  if (q === "") return [];
  const hits = roots.filter((r) => r.front && (r.code.toLowerCase().startsWith(q) || r.name.toLowerCase().includes(q)));
  hits.sort((a, b) => Number(b.code.toLowerCase() === q) - Number(a.code.toLowerCase() === q));
  return hits.map((r) => ({
    root: r.code,
    label: `${r.code} → ${r.front!.symbol} (${expiryMonth(r.front!.expiry_date)})`,
    instrument_id: r.front!.instrument_id,
    symbol: r.front!.symbol,
    venue: r.venue,
    name: r.name,
  }));
}

const fmtQty = (n: number) => String(Math.abs(n));

/** The two legs of a roll: a long sells the near contract and buys the far; a short is the reverse. */
export function rollLegs(netQty: number, near: string, far: string, qty: number = Math.abs(netQty)): string {
  const q = fmtQty(qty);
  return netQty > 0 ? `Sell ${q} ${near} · Buy ${q} ${far}` : `Buy ${q} ${near} · Sell ${q} ${far}`;
}

/** Decimal places of a tick (0.05 -> 2, 0.25 -> 2, 1 -> 0). */
const tickDecimals = (tick: number) => {
  const t = String(tick);
  return t.includes("e-") ? Number(t.split("e-")[1]) : (t.split(".")[1] ?? "").length;
};

/**
 * Net debit per spread: buy-leg mids minus sell-leg mids, rounded to the
 * nearest spread tick when one is known (the OMS refuses a limit off it).
 * Null when either mid is unknown.
 */
export function defaultSpreadPrice(netQty: number, nearMid: number | null, farMid: number | null, tick?: number | null): number | null {
  if (nearMid == null || farMid == null) return null;
  const v = netQty > 0 ? farMid - nearMid : nearMid - farMid;
  if (tick != null && tick > 0) {
    const r = Number((Math.round(v / tick) * tick).toFixed(tickDecimals(tick)));
    return r === 0 ? 0 : r; // no -0
  }
  return Math.round(v * 1e8) / 1e8;
}

export const defaultQty = (netQty: number) => Math.abs(netQty);

/** The qty to send: whole contracts, capped at the position; null when not a positive whole number. */
export function clampQty(raw: number, netQty: number): number | null {
  if (!Number.isInteger(raw) || raw <= 0) return null;
  return Math.min(raw, Math.abs(netQty));
}

export function bannerText(a: { contract: string; days: number; qty: number; portfolio: string }, long: boolean): string {
  const when =
    a.days > 1 ? `rolls in ${a.days} days` : a.days === 1 ? "rolls in 1 day" : a.days === 0 ? "rolls today"
    : `roll is ${-a.days} day${a.days === -1 ? "" : "s"} overdue`;
  const n = fmtQty(a.qty);
  return `${a.contract} ${when} — ${n} contract${Math.abs(a.qty) === 1 ? "" : "s"} ${long ? "long" : "short"} in ${a.portfolio}`;
}

export interface PositionLite { instrument_id: string; net_qty: number }

/**
 * ROLL_DUE alerts stay listed until acked, even after the position was rolled or
 * closed: hide those whose position is now flat. `undefined` (positions not
 * loaded yet) hides, `null` (positions failed to load) shows.
 */
export function visibleRollAlerts(alerts: AlertRow[], positions: Map<string, PositionLite[] | null | undefined>): AlertRow[] {
  return alerts.filter((a) => {
    if (a.kind !== "ROLL_DUE" || a.instrument_id == null) return false;
    const held = positions.get(a.portfolio_id);
    if (held === undefined) return false;
    if (held === null) return true;
    return held.some((p) => p.instrument_id === String(a.instrument_id) && p.net_qty !== 0);
  });
}

export interface RollOffer {
  root: string;
  nearInstrumentId: number;
  nearSymbol: string;
  farInstrumentId: number;
  farSymbol: string;
  rollDate: string;
  /** Within the root's warn window (or past the roll date): show the badge. */
  due: boolean;
}

/**
 * What rolling this position would do, or null when it cannot be rolled: the
 * OMS's roll target for the held contract (GET /futures/roll-targets). That
 * covers overdue contracts too (no longer front), whether or not their alert
 * was dismissed.
 */
export function rollOffer(
  position: { portfolio_id: string; instrument_id: string; net_qty: number },
  targets: RollTargetRow[],
  today: string,
): RollOffer | null {
  if (position.net_qty === 0) return null;
  const t = targets.find((x) => x.portfolio_id === position.portfolio_id && String(x.instrument_id) === position.instrument_id);
  if (!t) return null;
  return {
    root: t.root,
    nearInstrumentId: t.instrument_id,
    nearSymbol: t.symbol,
    farInstrumentId: t.next.instrument_id,
    farSymbol: t.next.symbol,
    rollDate: t.roll_date,
    due: rollBadgeDue(today, t.roll_date, t.roll_warn_days),
  };
}

/** The spread tick of `root`, or null when the root is unknown (no rounding then). */
export function spreadTickOf(root: string, roots: FutureRoot[]): number | null {
  return roots.find((r) => r.code === root)?.spread_tick ?? null;
}

/** The roll an alert's payload describes, or null if the payload is not the shape this client knows. */
export function offerFromAlert(a: AlertRow): RollOffer | null {
  const p = a.payload;
  if (a.instrument_id == null || typeof p.contract !== "string" || typeof p.next_contract !== "string"
      || typeof p.next_instrument_id !== "number" || typeof p.roll_date !== "string") return null;
  return {
    root: typeof p.root === "string" ? p.root : "",
    nearInstrumentId: a.instrument_id,
    nearSymbol: p.contract,
    farInstrumentId: p.next_instrument_id,
    farSymbol: p.next_contract,
    rollDate: p.roll_date,
    due: true,
  };
}
