// Reconciliation in the cockpit (src/recon_admin_api.rs): the per-connection
// settings, attribution rules, sweeps and breaks. Kept free of React so
// `node --test` can load it.

export type ReconOption = { value: string; label: string };

// Exactly the values the server accepts (broker_connection_eod_lookback_check).
export function lookbackOptions(): ReconOption[] {
  return [
    ["1d", "1 day"],
    ["1w", "1 week"],
    ["1m", "1 month"],
    ["1y", "1 year"],
    ["5y", "5 years"],
    ["10y", "10 years"],
  ].map(([value, label]) => ({ value, label }));
}

export function policyOptions(): ReconOption[] {
  return [
    { value: "adopt", label: "adopt — book into the attributed portfolio" },
    { value: "break", label: "break — hold for review on the Breaks page" },
  ];
}

// Per-account tables only make sense where one credential reaches several
// venue accounts (IBKR); other connections get their single account for free.
export function showAccounts(connection: { broker_code: string }, venues: { code: string; multi_account?: boolean }[]): boolean {
  return venues.find((v) => v.code === connection.broker_code)?.multi_account === true;
}

export type ReconForm = {
  default_portfolio_id: string | null;
  external_policy: string | null;
  eod_lookback: string | null;
  intraday_poll_secs: string;
};

// The API cannot clear a setting, so a PATCH carries only what is set.
export function reconPatch(form: ReconForm): Record<string, string | number> {
  const patch: Record<string, string | number> = {};
  if (form.default_portfolio_id) patch.default_portfolio_id = form.default_portfolio_id;
  if (form.external_policy) patch.external_policy = form.external_policy;
  if (form.eod_lookback) patch.eod_lookback = form.eod_lookback;
  const secs = Number.parseInt(form.intraday_poll_secs, 10);
  if (Number.isFinite(secs)) patch.intraday_poll_secs = secs;
  return patch;
}

export type SweepReport = {
  requested_from: string;
  obtained_from: string;
  booked: number;
  duplicates: number;
  breaks: number;
  failed: number;
  errors: string[];
};

export type ReconBreak = {
  id: string;
  broker_connection_code: string;
  account_id: string;
  venue_order_id: string;
  kind: string;
  events: unknown;
  status: string;
  created_at: string;
};

export function sweepSummary(r: SweepReport): string {
  const day = (t: string) => t.slice(0, 10);
  const parts = [`${r.booked} booked`, `${r.duplicates} duplicates`, `${r.breaks} breaks`, `${r.failed} failed`];
  const served = r.obtained_from === r.requested_from ? `history from ${day(r.obtained_from)}` : `history starts ${day(r.obtained_from)} (asked ${day(r.requested_from)}; the venue caps it)`;
  return `${parts.join(", ")}; ${served}`;
}

export function openBreakCount(breaks: { status: string }[] | undefined): number {
  return breaks?.filter((b) => b.status === "open").length ?? 0;
}

export function eventCount(events: unknown): number {
  return Array.isArray(events) ? events.length : 0;
}

type Post = (path: string, body: unknown) => Promise<unknown>;

export const adoptBreak = (post: Post, id: string, portfolioId: string) =>
  post(`/admin/recon-breaks/${id}/adopt`, { portfolio_id: portfolioId });

export const dismissBreak = (post: Post, id: string) => post(`/admin/recon-breaks/${id}/dismiss`, {});

// The server explains 422 and 409 in the body; show that, not "422: ...".
export function errorText(err: unknown): string {
  if (err && typeof err === "object" && "message" in err) return String((err as { message: unknown }).message);
  return String(err);
}
