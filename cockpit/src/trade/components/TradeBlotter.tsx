import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { notifications } from "@mantine/notifications";
import {
  Alert, Badge, Button, Drawer, Group, Loader, Select, Stack, Table, Text, Title, Tooltip,
} from "@mantine/core";
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

// How many orders to ask for. The server defaults to 100 and clamps to 1..500
// (list_orders in src/handlers.rs); asking explicitly means the window is a
// decision made here rather than a default that silently truncates the blotter.
const BLOTTER_LIMIT = 200;

// GET /orders/{id}, of which only the status is used here. Mirrors
// OrderAggregateState in src/domain/orders/state.rs.
type OrderState = { order_id: string; status: string };

function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notifications.show({ message, color: "red" });
}

export function TradeBlotter({
  portfolios,
  followOrderId = null,
}: {
  portfolios: GrantedPortfolio[];
  // The order the ticket just submitted, if any. GET /orders is scoped
  // server-side to can_view grants, so the same permission that decides which
  // rows come back decides which portfolios can be picked — Positions uses
  // can_view for exactly this reason, and the two must not disagree.
  followOrderId?: string | null;
}) {
  const viewable = portfolios.filter((p) => p.can_view);
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

  // Filtering and limiting happen SERVER-side. Fetching an unparameterised
  // /orders and filtering in the browser meant the server's own default window
  // (LIMIT 100 ORDER BY created_at DESC) silently hid everything older, and
  // picking a portfolio could show "No orders" while that portfolio had plenty
  // — just none inside the newest hundred rows across all portfolios.
  const orders = useQuery<BlotterRow[]>({
    queryKey: ["/orders", { portfolioId, limit: BLOTTER_LIMIT }],
    queryFn: () => {
      const params = new URLSearchParams({ limit: String(BLOTTER_LIMIT) });
      if (portfolioId) params.set("portfolio_id", portfolioId);
      return tradeApi.get<BlotterRow[]>(`/orders?${params.toString()}`);
    },
    refetchInterval: 2000,
  });

  // The order the ticket just sent, watched on its own until it leaves
  // `submitted` — the spec's "after submission" behaviour. The submit response
  // is empty (204), so the only way to learn what became of the order is to
  // fetch it; an order that never leaves `submitted` is precisely the
  // recorded-but-not-routed case the 502/503 toasts warn about, and this is
  // where a trader sees it rather than being told.
  const followed = useQuery<OrderState>({
    queryKey: ["/orders", followOrderId, "state"],
    queryFn: () => tradeApi.get<OrderState>(`/orders/${followOrderId}`),
    enabled: followOrderId !== null,
    // Stop polling once it has moved: there is nothing further to learn.
    refetchInterval: (query) =>
      query.state.data && query.state.data.status !== "submitted" ? false : 2000,
  });
  const followedStatus = followOrderId ? followed.data?.status ?? null : null;

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

  // No client-side filtering: what came back IS the answer to the query asked.
  const rows = orders.data ?? [];
  const windowFull = rows.length >= BLOTTER_LIMIT;

  return (
    <Stack>
      <Group justify="space-between" align="flex-end">
        <Title order={3}>Blotter</Title>
        {viewable.length > 1 && (
          <Select
            label="Portfolio"
            data={viewable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
            value={portfolioId}
            onChange={setPortfolioId}
            clearable
            searchable
            w={200}
          />
        )}
      </Group>

      {followOrderId !== null && (
        <Alert
          color={
            followedStatus === null || followedStatus === "submitted"
              ? "gray"
              : STATUS_COLOR[followedStatus] ?? "gray"
          }
          title={
            followedStatus === null
              ? "Watching your order…"
              : followedStatus === "submitted"
                ? "Your order is recorded, not yet routed"
                : `Your order is ${followedStatus}`
          }
        >
          <Text size="sm">
            {followOrderId}
            {followedStatus === "submitted" &&
              " — it has not reached a broker yet. If it stays here, it was never routed: cancel it below."}
            {followed.isError && " — could not read this order back; check the rows below."}
          </Text>
        </Alert>
      )}

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
                <Table.Tr
                  key={o.order_id}
                  onClick={() => setSelected(o)}
                  style={{ cursor: "pointer" }}
                  bg={o.order_id === followOrderId ? "var(--mantine-color-blue-light)" : undefined}
                >
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
            {windowFull && (
              <Table.Tr>
                <Table.Td colSpan={10}>
                  <Text c="dimmed" size="xs" ta="center">
                    Showing the {BLOTTER_LIMIT} most recent orders; older ones are not listed.
                  </Text>
                </Table.Td>
              </Table.Tr>
            )}
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
