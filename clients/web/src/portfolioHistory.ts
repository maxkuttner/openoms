export interface HistoryEvent {
  at: string;
  actor: string;
  action: string;
  detail: unknown;
}

type Obj = Record<string, unknown>;
const obj = (v: unknown): Obj => (v && typeof v === "object" ? (v as Obj) : {});
const show = (v: unknown) => (v === null || v === undefined || v === "" ? "—" : String(v));

/** One human line for an audit event on a portfolio; unknown actions come back raw. */
export function historyLabel(e: HistoryEvent): string {
  const d = obj(e.detail);
  switch (e.action) {
    case "portfolio.created":
      return "Created";
    case "portfolio.updated": {
      const parts = Object.entries(obj(d.changes)).map(([k, v]) => {
        const [from, to] = Array.isArray(v) ? v : [];
        return `${k} ${show(from)} → ${show(to)}`;
      });
      return parts.length ? `Edited: ${parts.join(", ")}` : "Edited";
    }
    case "grants.set": {
      const names = (Array.isArray(d.grants) ? d.grants : []).map((g) => obj(obj(g).params).connection ?? obj(g).kind).filter(Boolean);
      return `Venues set: ${names.length ? names.join(", ") : "none"}`;
    }
    case "portfolio.access_set": {
      const n = Array.isArray(d.detail) ? d.detail.length : 0;
      return `Access changed (${n} ${n === 1 ? "entry" : "entries"})`;
    }
    case "limits.changed":
      return "Risk limits changed";
    case "group.created":
      return `Group created: ${show(obj(d.detail).name)}`;
    default:
      return e.action;
  }
}
