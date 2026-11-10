import type { StreamHealth } from "./api/types.ts";

export function ago(iso: string | null, now = Date.now()): string {
  if (!iso) return "never";
  const secs = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000));
  if (secs < 60) return `${secs}s ago`;
  if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
  return `${Math.round(secs / 3600)}h ago`;
}

/** A stream's tooltip: why it is down or still connecting, else how long it has been live. */
export function healthLabel(s: StreamHealth, now = Date.now()): string {
  if (s.state === "live") {
    return `connected ${ago(s.connected_since, now)} · last event ${ago(s.last_event_at, now)}`;
  }
  return `${s.state}${s.last_error ? `: ${s.last_error}` : ""} · last event ${ago(s.last_event_at, now)}`;
}
