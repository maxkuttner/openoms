// Pure display logic for trade times: blotter ordering, estimated/first-seen
// markers, timeline labels and position names. Timestamps are formatted by the
// caller-supplied formatter (the cockpit's local-tz toLocaleString).

export const ESTIMATED_TIP = "estimated: venue sent no execution time";
export const FIRST_SEEN_TIP = "first seen: the OMS did not place this order; this is when it first saw it at the venue";

/// Default formatter: the user's local timezone, like every other cockpit timestamp.
export const fmtLocal = (iso: string): string => new Date(iso).toLocaleString();

export interface Cell {
  text: string;
  tooltip: string | null;
}

export function tradeTimeCell(
  o: { trade_time: string | null; trade_time_estimated: boolean },
  fmt: (iso: string) => string = fmtLocal,
): Cell {
  if (o.trade_time === null) return { text: "—", tooltip: null };
  return o.trade_time_estimated
    ? { text: `~${fmt(o.trade_time)}`, tooltip: ESTIMATED_TIP }
    : { text: fmt(o.trade_time), tooltip: null };
}

export function placedCell(
  o: { placed_at: string; placed_is_first_seen: boolean },
  fmt: (iso: string) => string = fmtLocal,
): Cell {
  return o.placed_is_first_seen
    ? { text: `${fmt(o.placed_at)} (first seen)`, tooltip: FIRST_SEEN_TIP }
    : { text: fmt(o.placed_at), tooltip: null };
}

/// order_observed is a venue sighting, never a submission or routing by us.
export function eventLabel(eventType: string): string {
  return eventType === "order_observed" ? "Observed at venue" : eventType;
}

/// Labelled times for a timeline row: venue time, plus booked time only when it differs.
export function eventTimes(
  occurredAt: string,
  bookedAt: string,
  fmt: (iso: string) => string = fmtLocal,
): { label: string; text: string }[] {
  const out = [{ label: "venue", text: fmt(occurredAt) }];
  if (Date.parse(bookedAt) !== Date.parse(occurredAt)) out.push({ label: "booked", text: fmt(bookedAt) });
  return out;
}

export function positionLabel(p: {
  instrument_id: string;
  instrument_symbol: string | null;
  instrument_name: string | null;
}): Cell {
  if (p.instrument_symbol === null) {
    return p.instrument_name ? { text: p.instrument_name, tooltip: p.instrument_id } : { text: p.instrument_id, tooltip: null };
  }
  return {
    text: p.instrument_symbol,
    tooltip: p.instrument_name ? `${p.instrument_name} (${p.instrument_id})` : p.instrument_id,
  };
}
