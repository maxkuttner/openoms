import { useState } from "react";
import { Timeline, Text, Group, Badge, Code, Loader, Stack, Anchor, Collapse } from "@mantine/core";
import { useQuery } from "@tanstack/react-query";
import { api } from "../api/client";
import type { OrderEvent } from "../api/types";

/** Ties the dot colour to the status the event left the order in. */
const STATUS_COLOR: Record<string, string> = {
  filled: "green",
  partially_filled: "teal",
  routed: "blue",
  submitted: "gray",
  canceled: "orange",
  rejected: "red",
  expired: "yellow",
  suspended: "grape",
};

/** One event: what it did, who did it, when — and the stored record underneath. */
function Entry({ event }: { event: OrderEvent }) {
  const [open, setOpen] = useState(false);
  const occurred = new Date(event.occurred_at);

  return (
    <Timeline.Item
      color={STATUS_COLOR[event.status_after ?? ""] ?? "gray"}
      title={
        <Group gap="xs">
          <Text size="sm" fw={500}>{event.event_type}</Text>
          {event.status_after && (
            <Badge size="xs" variant="light" color={STATUS_COLOR[event.status_after] ?? "gray"}>
              {event.status_after}
            </Badge>
          )}
        </Group>
      }
    >
      <Text size="sm">{event.summary}</Text>
      <Text size="xs" c="dimmed" mt={2}>
        {occurred.toLocaleString()} · {event.actor} · v{event.version}
      </Text>
      <Anchor component="button" type="button" size="xs" onClick={() => setOpen((o) => !o)} mt={4}>
        {open ? "hide" : "raw payload"}
      </Anchor>
      <Collapse in={open}>
        <Code block mt={4} style={{ fontSize: 11 }}>
          {JSON.stringify(event.payload, null, 2)}
        </Code>
      </Collapse>
    </Timeline.Item>
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
  apiGet = api.get,
}: {
  orderId: string;
  // Base path for the orders resource. Defaults to the admin surface; the trade
  // app passes "/orders" instead. The full events URL is `${eventsPath}/${orderId}/events`.
  eventsPath?: string;
  // Fetch function used for the request. Defaults to the cockpit's admin client
  // (which attaches an Authorization: Bearer admin token). The trade app MUST pass
  // tradeApi.get instead, or it would leak the admin token on every request.
  apiGet?: (path: string) => Promise<unknown>;
}) {
  const path = `${eventsPath}/${orderId}/events`;
  const events = useQuery<OrderEvent[]>({
    queryKey: [eventsPath, orderId, "events"],
    queryFn: () => apiGet(path) as Promise<OrderEvent[]>,
  });

  if (events.isLoading) return <Loader size="sm" />;
  if (events.error) return <Text c="red" size="sm">Could not load the order's events.</Text>;

  const rows = events.data ?? [];
  if (rows.length === 0) return <Text c="dimmed" size="sm">No events recorded.</Text>;

  return (
    <Stack gap="xs">
      <Timeline active={rows.length - 1} bulletSize={16} lineWidth={2}>
        {rows.map((e) => (
          <Entry key={e.event_id} event={e} />
        ))}
      </Timeline>
    </Stack>
  );
}
