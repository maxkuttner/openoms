// Reported strategies (src/reported/): strategies that trade outside the OMS
// and report their fills. Kept free of React so `node --test` can load it.

export const BASE = "/strategies/reported";

export type ReportedStrategy = {
  code: string;
  name: string;
  portfolio_id: string;
  owner_principal_id: string;
  state: string;
  stale_after_secs: number;
  created_at: string;
  /** Set once archived: reporting stopped, history kept. */
  archived_at: string | null;
};

export type ReportedCreated = { code: string; portfolio_id: string; group_id: string; token: string };

export type Gap = { batch_id: string; sequence: number; expected: number };

export type ReportedStatus = {
  last_report_at: string | null;
  stale: boolean;
  last_sequence: number | null;
  gaps: Gap[];
  open_breaks: number;
  token_active: boolean;
};

export type ReportedFill = {
  id: string;
  batch_id: string;
  venue: string;
  account_ref: string;
  exec_id: string;
  client_order_id: string | null;
  symbol: string;
  instrument_id: number;
  side: string;
  qty: number;
  price: number;
  fee: number | null;
  fee_asset: string | null;
  fee_effect: string | null;
  trade_time: string;
  booked_at: string;
};

export type FillsPage = { fills: ReportedFill[]; next_before: string | null };

export type ReportedBatch = {
  batch_id: string;
  sequence: number | null;
  received_at: string;
  booked: number;
  duplicate: number;
  broken: number;
  sequence_gap: boolean;
};

function age(ms: number): string {
  const mins = Math.max(0, Math.floor(ms / 60_000));
  if (mins < 60) return `${mins}m`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return m ? `${h}h${m}m` : `${h}h`;
}

/** "never reported", "stale 12m" (since the last report) or "ok". */
export function staleLabel(s: Pick<ReportedStatus, "last_report_at" | "stale">, now = Date.now()): string {
  if (!s.last_report_at) return "never reported";
  if (s.stale) return `stale ${age(now - Date.parse(s.last_report_at))}`;
  return "ok";
}

export function gapSummary(gaps: Gap[]): string {
  if (gaps.length === 0) return "no gaps";
  const items = gaps.map((g) => `expected ${g.expected}, got ${g.sequence} (${g.batch_id})`);
  return `${gaps.length} gap${gaps.length === 1 ? "" : "s"}: ${items.join("; ")}`;
}

export const reportedBreakLabel = "Reported, not verified";
export const isReportedBreak = (b: { source: string }) => b.source === "REPORTED";

type Get = <T>(path: string) => Promise<T>;
type Post = <T>(path: string, body: unknown) => Promise<T>;
type Patch = <T>(path: string, body: unknown) => Promise<T>;
type Del = (path: string) => Promise<unknown>;

const enc = encodeURIComponent;

export const listReported = (get: Get) => get<ReportedStrategy[]>(BASE);
export const createReported = (post: Post, body: { code: string; name: string; base_currency?: string }) =>
  post<ReportedCreated>(BASE, body);
export const rotateToken = (post: Post, code: string) => post<{ token: string }>(`${BASE}/${enc(code)}/token`, {});
export const revokeToken = (del: Del, code: string) => del(`${BASE}/${enc(code)}/token`);
export const archiveReported = (post: Post, code: string) => post(`${BASE}/${enc(code)}/archive`, {});
export const getStatus = (get: Get, code: string) => get<ReportedStatus>(`${BASE}/${enc(code)}/status`);
export function fillsPath(code: string, limit = 50, before?: string | null): string {
  const q = new URLSearchParams({ limit: String(limit) });
  if (before) q.set("before", before);
  return `${BASE}/${enc(code)}/fills?${q}`;
}
export const getFills = (get: Get, code: string, before?: string | null) => get<FillsPage>(fillsPath(code, 50, before));
export const getBatches = (get: Get, code: string, limit = 20) =>
  get<ReportedBatch[]>(`${BASE}/${enc(code)}/batches?limit=${limit}`);
export const patchReported = (patch: Patch, code: string, staleAfterSecs: number) =>
  patch(`${BASE}/${enc(code)}`, { stale_after_secs: staleAfterSecs });

/** A REPORTED break's adopt body: the instrument to book into, optionally the portfolio, and whether to map the symbol. */
export const adoptReportedBreak = (
  post: Post,
  id: string,
  body: { instrument_id: string; portfolio_id?: string; map_symbol?: boolean },
) => post(`/admin/recon-breaks/${id}/adopt`, body);

/** Which token buttons a strategy card offers; unknown status hides Revoke until it loads. */
export function tokenActions(status: { token_active: boolean } | undefined): {
  rotateLabel: "Rotate token" | "Issue token";
  canRevoke: boolean;
} {
  if (!status) return { rotateLabel: "Rotate token", canRevoke: false };
  return { rotateLabel: status.token_active ? "Rotate token" : "Issue token", canRevoke: status.token_active };
}

/** The strategies the list shows: archived ones only on request. */
export function visibleStrategies<T extends { archived_at: string | null }>(rows: T[], showArchived: boolean): T[] {
  return showArchived ? rows : rows.filter((r) => !r.archived_at);
}
