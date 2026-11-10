// Pure position maths for the Positions page. Amounts are in each instrument's
// own currency and are never converted, so every total is kept per currency.

// Mirrors src/positions.rs Position. mark, market_value, unrealized_pnl and
// mark_ts are None when no live mark exists for the instrument — an
// unpriceable/expired contract, or a feed that's down — and MUST stay
// nullable here. Defaulting them (in the type, or with `?? 0`) would turn an
// honest "we don't know" into a lie that reads as a real, zero-valued P&L.
export interface PositionRow {
  portfolio_id: string;
  instrument_id: string;
  currency: string;
  net_qty: number;
  avg_cost: number;
  realized_pnl: number;
  updated_at: string;
  mark: number | null;
  market_value: number | null;
  unrealized_pnl: number | null;
  mark_ts: string | null;
}

export interface CurrencyTotals {
  realized: number;
  unrealized: number;
  day: number;
  marketValue: number;
  grossExposure: number;
}

/// Quote fields Day P&L needs, keyed by instrument_id.
export type DayMarks = Map<string, { mid?: number | null; prev_close?: number | null }>;

/// Totals per currency, in first-seen order. Rows without a live mark are left
/// out of the mark-dependent sums (not zeroed); Day P&L additionally needs a
/// previous close. Amounts of different currencies are never added together.
export function totalsByCurrency(rows: PositionRow[], marks: DayMarks = new Map()): Map<string, CurrencyTotals> {
  const out = new Map<string, CurrencyTotals>();
  for (const p of rows) {
    let t = out.get(p.currency);
    if (!t) {
      t = { realized: 0, unrealized: 0, day: 0, marketValue: 0, grossExposure: 0 };
      out.set(p.currency, t);
    }
    t.realized += p.realized_pnl;
    if (p.unrealized_pnl !== null) t.unrealized += p.unrealized_pnl;
    if (p.market_value !== null) {
      t.marketValue += p.market_value;
      t.grossExposure += Math.abs(p.market_value);
    }
    const m = marks.get(p.instrument_id);
    if (m?.mid != null && m?.prev_close != null) t.day += p.net_qty * (m.mid - m.prev_close);
  }
  return out;
}
