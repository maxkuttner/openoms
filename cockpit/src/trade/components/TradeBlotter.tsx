import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { notifications } from "@mantine/notifications";
import { Badge, Button, Drawer, Group, Loader, Select, Stack, Table, Text, Title, Tooltip } from "@mantine/core";
import { tradeApi, ApiError } from "../api/client";
import { OrderTimeline } from "../../components/OrderTimeline";
import type { BlotterRow } from "../../api/types";
import type { GrantedPortfolio } from "../App";

// Same palette as the cockpit blotter (src/pages/Blotter.tsx) — one status
// vocabulary, one set of colours, wherever it's rendered.
const STATUS_COLOR: Record<string, string> = {
  filled: "green",
  partially_filled: "teal",
  routed: "blue",
  submitted: "gray",
  canceled: "orange",
  rejected: "red",
  expired: "yellow",
};

// Statuses the OMS will never move on from. Mirrors the terminal check in
// POST /orders/cancel (src/handlers.rs) — an order in one of these states is
// done, one way or another.
const TERMINAL = new Set(["filled", "canceled", "rejected", "expired"]);

function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notifications.show({ message, color: "red" });
}

export function TradeBlotter({ portfolios }: { portfolios: GrantedPortfolio[] }) {
  const [portfolioId, setPortfolioId] = useState<string | null>(null);
  const [selected, setSelected] = useState<BlotterRow | null>(null);

  // POST /orders/cancel answers 202 when the cancel was FORWARDED to the broker
  // and is still working, and 204 only when it is already done. The execution
  // stream finalises OrderCanceled on broker confirmation — that asymmetry is
  // what fixed the fill-versus-cancel race, so the UI must not flatten it back
  // out by claiming the order is cancelled the moment the request returns.
  // A row stays "cancelling…" until the polled order's OWN status reaches a
  // terminal state (which may be `filled`, not `canceled` — the cancel lost
  // the race, and that is exactly the case this exists for).
  const [cancelling, setCancelling] = useState<Set<string>>(new Set());

  const orders = useQuery<BlotterRow[]>({
    queryKey: ["/orders"],
    queryFn: () => tradeApi.get<BlotterRow[]>("/orders"),
    refetchInterval: 2000,
  });

  // Drop an order from `cancelling` once its own polled status says it's done —
  // never on the strength of the POST response alone.
  useEffect(() => {
    if (!orders.data) return;
    setCancelling((current) => {
      if (current.size === 0) return current;
      let next: Set<string> | null = null;
      for (const row of orders.data!) {
        if (current.has(row.order_id) && TERMINAL.has(row.status)) {
          if (!next) next = new Set(current);
          next.delete(row.order_id);
        }
      }
      return next ?? current;
    });
  }, [orders.data]);

  async function cancel(orderId: string) {
    setCancelling((s) => new Set(s).add(orderId));
    try {
      await tradeApi.post("/orders/cancel", { order_id: orderId });
      // Either 202 or 204: in both cases the row keeps polling until the
      // order's own status changes. We never render "cancelled" on our own
      // authority.
      notifications.show({ message: "Cancel sent", color: "blue" });
    } catch (err) {
      setCancelling((s) => {
        const n = new Set(s);
        n.delete(orderId);
        return n;
      });
      notifyError(err);
    }
  }

  const rows = (orders.data ?? []).filter((o) => !portfolioId || o.portfolio_id === portfolioId);

  return (
    <Stack>
      <Group justify="space-between" align="flex-end">
        <Title order={3}>Blotter</Title>
        {portfolios.length > 1 && (
          <Select
            label="Portfolio"
            data={portfolios.map((p) => ({ value: p.portfolio_id, label: p.code }))}
            value={portfolioId}
            onChange={setPortfolioId}
            clearable
            searchable
            w={200}
          />
        )}
      </Group>

      <Drawer
        opened={selected !== null}
        onClose={() => setSelected(null)}
        position="right"
        size="lg"
        title={
          selected && (
            <Stack gap={0}>
              <Text fw={600}>
                {selected.instrument_symbol ?? selected.instrument_id} · {selected.side} {selected.original_qty}
              </Text>
              <Text size="xs" c="dimmed">
                {selected.order_id} · {selected.portfolio_code}
              </Text>
            </Stack>
          )
        }
      >
        {selected && <OrderTimeline orderId={selected.order_id} eventsPath="/orders" apiGet={tradeApi.get} />}
      </Drawer>

      {orders.isLoading ? (
        <Loader />
      ) : (
        <Table striped highlightOnHover withTableBorder>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Time</Table.Th>
              <Table.Th>Instrument</Table.Th>
              <Table.Th>Side</Table.Th>
              <Table.Th>Type</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th ta="right">Qty</Table.Th>
              <Table.Th ta="right">Cum</Table.Th>
              <Table.Th ta="right">Leaves</Table.Th>
              <Table.Th ta="right">Avg px</Table.Th>
              <Table.Th />
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rows.map((o) => {
              const isCancelling = cancelling.has(o.order_id) && !TERMINAL.has(o.status);
              const canCancel = !TERMINAL.has(o.status) && !isCancelling;
              return (
                <Table.Tr key={o.order_id} onClick={() => setSelected(o)} style={{ cursor: "pointer" }}>
                  <Table.Td>{new Date(o.created_at).toLocaleString()}</Table.Td>
                  <Table.Td>
                    <Tooltip label={o.instrument_name ?? `id ${o.instrument_id}`} disabled={!o.instrument_name}>
                      <Text size="sm">{o.instrument_symbol ?? o.instrument_id}</Text>
                    </Tooltip>
                  </Table.Td>
                  <Table.Td>
                    <Badge color={o.side === "buy" ? "green" : "red"} variant="light">
                      {o.side}
                    </Badge>
                  </Table.Td>
                  <Table.Td>{o.order_type}</Table.Td>
                  <Table.Td>
                    {isCancelling ? (
                      <Badge color="gray" variant="light">
                        cancelling…
                      </Badge>
                    ) : (
                      <Badge color={STATUS_COLOR[o.status] ?? "gray"}>{o.status}</Badge>
                    )}
                  </Table.Td>
                  <Table.Td ta="right">{o.original_qty}</Table.Td>
                  <Table.Td ta="right">{o.cum_qty}</Table.Td>
                  <Table.Td ta="right">{o.leaves_qty}</Table.Td>
                  <Table.Td ta="right">{o.avg_px ?? "—"}</Table.Td>
                  <Table.Td>
                    <Button
                      size="xs"
                      variant="light"
                      color="red"
                      disabled={!canCancel}
                      loading={isCancelling}
                      onClick={(e) => {
                        e.stopPropagation();
                        cancel(o.order_id);
                      }}
                    >
                      Cancel
                    </Button>
                  </Table.Td>
                </Table.Tr>
              );
            })}
            {rows.length === 0 && (
              <Table.Tr>
                <Table.Td colSpan={10}>
                  <Text c="dimmed" ta="center" py="md">
                    No orders.
                  </Text>
                </Table.Td>
              </Table.Tr>
            )}
          </Table.Tbody>
        </Table>
      )}
    </Stack>
  );
}
