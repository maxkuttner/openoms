import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import type { OrderEvent } from "../api/types";
import { cn } from "@/lib/utils";
import { Spinner } from "./common";
import { eventLabel, eventTimes } from "../tradeTimes";
import { ORDER_STATUS_TONE, Pill } from "./status";

const DOT: Record<string, string> = { buy: "bg-buy", sell: "bg-sell", warn: "bg-warn", info: "bg-foreground-2", muted: "bg-muted-foreground" };

/** One event: what it did, who did it, when — and the stored record underneath. */
function Entry({ event, last }: { event: OrderEvent; last: boolean }) {
  const [open, setOpen] = useState(false);
  const times = eventTimes(event.occurred_at, event.booked_at ?? event.recorded_at);
  const tone = ORDER_STATUS_TONE[event.status_after ?? ""] ?? "muted";

  return (
    <li className="relative pb-4 pl-6">
      {!last && <span className="absolute top-3 left-[5px] h-full w-px bg-border" aria-hidden />}
      <span className={cn("absolute top-1 left-0 size-[11px] rounded-full border-2 border-background", DOT[tone])} aria-hidden />
      <div className="flex items-center gap-2">
        <span className="font-medium">{eventLabel(event.event_type)}</span>
        {event.status_after && <Pill tone={tone}>{event.status_after}</Pill>}
      </div>
      <p>{event.summary}</p>
      <p className="num mt-0.5 text-xs text-muted-foreground">
        {times.map((x, i) => (i === 0 ? x.text : `${x.label} ${x.text}`)).join(" · ")} · {event.actor} · v{event.version}
      </p>
      <button type="button" onClick={() => setOpen((o) => !o)} className="mt-1 text-xs text-primary hover:underline">
        {open ? "hide" : "raw payload"}
      </button>
      {open && (
        <pre className="num mt-1 overflow-x-auto rounded-md border bg-background p-2 text-[11px]">{JSON.stringify(event.payload, null, 2)}</pre>
      )}
    </li>
  );
}

/**
 * An order's audit trail, oldest event first — the event store rendered for reading.
 *
 * The events are immutable and complete: this is the whole history of the order, not
 * a log of it.
 */
export function OrderTimeline({
  orderId,
  eventsPath = "/admin/orders",
  apiGet,
}: {
  orderId: string;
  // Base path for the orders resource. Defaults to the admin surface; the trading
  // screens pass "/orders" instead. The full events URL is `${eventsPath}/${orderId}/events`.
  eventsPath?: string;
  // Fetch function used for the request. REQUIRED, and deliberately so: there is
  // no default. A default of the cockpit's admin client (which attaches
  // Authorization: Bearer <admin token>) would be fail-open — one forgotten prop
  // on a trade-side usage would put the admin token on a trader request. Making
  // it required turns that into a compile error instead. Both surfaces pass
  // `api.get` (session cookie only); only the path differs.
  apiGet: (path: string) => Promise<unknown>;
}) {
  const path = `${eventsPath}/${orderId}/events`;
  const events = useQuery<OrderEvent[]>({
    queryKey: [eventsPath, orderId, "events"],
    queryFn: () => apiGet(path) as Promise<OrderEvent[]>,
  });

  if (events.isLoading) return <Spinner />;
  if (events.error) return <p className="text-sell">Could not load the order's events.</p>;

  const rows = events.data ?? [];
  if (rows.length === 0) return <p className="text-muted-foreground">No events recorded.</p>;

  return (
    <ol className="flex flex-col">
      {rows.map((e, i) => (
        <Entry key={e.event_id} event={e} last={i === rows.length - 1} />
      ))}
    </ol>
  );
}
