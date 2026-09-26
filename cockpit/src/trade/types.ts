// Mirrors src/handlers.rs's MarkRow. A field is null when that half of the
// data (live mark, or previous close) hasn't been populated for this
// instrument yet — never a fabricated 0, so callers must not `?? 0` these.
export interface MarkRow {
  instrument_id: number;
  bid: number | null;
  ask: number | null;
  mid: number | null;
  prev_close: number | null;
  pct_change: number | null;
}
