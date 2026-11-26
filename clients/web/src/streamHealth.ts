import type { StreamHealth } from "./api/types.ts";

function span(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  return `${Math.round(secs / 3600)}h`;
}

export function ago(iso: string | null, now = Date.now()): string {
  if (!iso) return "never";
  return `${span(Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000)))} ago`;
}

/** "in 30s" for a time ahead, "now" once it has come. */
export function until(iso: string, now = Date.now()): string {
  const secs = Math.round((new Date(iso).getTime() - now) / 1000);
  return secs > 0 ? `in ${span(secs)}` : "now";
}

/**
 * One line on a stream's state, the same for every stream type:
 * live: "live · checked 5s ago"; connecting: "connecting… <reason>";
 * down: "down: <reason> · retrying in 30s · checked 10s ago";
 * stopped: "stopped: <reason> — fix the credentials and save".
 */
export function describeStream(s: StreamHealth, now = Date.now()): string {
  const checked = s.last_checked_at ? ` · checked ${ago(s.last_checked_at, now)}` : "";
  switch (s.state) {
    case "live":
      return `live${checked}`;
    case "connecting":
      return s.last_error ? `connecting… ${s.last_error}` : "connecting…";
    case "down": {
      const retry = s.next_retry_at ? ` · retrying ${until(s.next_retry_at, now)}` : "";
      return `down${s.last_error ? `: ${s.last_error}` : ""}${retry}${checked}`;
    }
    case "stopped":
      return `stopped${s.last_error ? `: ${s.last_error}` : ""} — fix the credentials and save`;
  }
}

/** When the stream last carried an order/fill (a quote, for a feed); null if never. */
export function lastEventLabel(s: StreamHealth, now = Date.now()): string | null {
  if (!s.last_event_at) return null;
  return `${s.kind === "feed" ? "last quote" : "last order/fill event"} ${ago(s.last_event_at, now)}`;
}
