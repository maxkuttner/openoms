import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { notifications } from "@mantine/notifications";
import { Alert, Badge, Button, Group, Paper, Select, SimpleGrid, Stack, Text, Title } from "@mantine/core";
import { tradeApi } from "../api/client";
import { submitOrder, RECORDED_NOT_ROUTED, type SubmitOutcome } from "../api/submitOrder";
import type { GrantedPortfolio } from "../App";
import type { StrategyLeg, VenueOption, OrderType, TimeInForce } from "../types";

type LegStatus = "pending" | "sending" | SubmitOutcome["kind"];

type Payoff = {
  points: { s: number; pnl: number }[];
  loRange: number;
  hiRange: number;
  maxLoss: number | "unlimited";
  maxProfit: number | "unlimited";
  breakevens: number[];
};

const PAYOFF_STEPS = 120;

// Payoff-at-expiry is pure algebra from strike/side/kind/premium — no greeks
// or IV needed, so it stays correct even though those stay out of scope.
// Only called once every leg has a real premium (see allLegsPriced below).
function computePayoff(legs: StrategyLeg[]): Payoff {
  const strikes = legs.map((l) => l.strike);
  const minStrike = Math.min(...strikes);
  const maxStrike = Math.max(...strikes);
  const span = Math.max(maxStrike - minStrike, minStrike * 0.1, 10);
  const loRange = Math.max(0, minStrike - span);
  const hiRange = maxStrike + span;

  function legPnl(leg: StrategyLeg, s: number): number {
    const intrinsic = leg.optionKind === "CALL" ? Math.max(s - leg.strike, 0) : Math.max(leg.strike - s, 0);
    const premium = leg.referencePrice!;
    return leg.side === "buy" ? intrinsic - premium : premium - intrinsic;
  }

  function totalPnl(s: number): number {
    return legs.reduce((sum, l) => sum + legPnl(l, s), 0);
  }

  const points = Array.from({ length: PAYOFF_STEPS + 1 }, (_, i) => {
    const s = loRange + ((hiRange - loRange) * i) / PAYOFF_STEPS;
    return { s, pnl: totalPnl(s) };
  });

  // Exact asymptotic slopes (not finite-differenced): below every strike only
  // puts move (call intrinsic is flat at 0 there); above every strike only
  // calls move (put intrinsic is flat at 0 there).
  let lowSlope = 0;
  let highSlope = 0;
  for (const leg of legs) {
    const sign = leg.side === "buy" ? 1 : -1;
    if (leg.optionKind === "CALL") highSlope += sign;
    else lowSlope += -sign;
  }

  const sampledPnls = points.map((p) => p.pnl);
  const maxProfit = highSlope > 0 || lowSlope < 0 ? "unlimited" : Math.max(...sampledPnls);
  const maxLoss = highSlope < 0 || lowSlope > 0 ? "unlimited" : Math.min(...sampledPnls);

  const breakevens: number[] = [];
  for (let i = 1; i < points.length; i++) {
    const a = points[i - 1];
    const b = points[i];
    if ((a.pnl <= 0 && b.pnl > 0) || (a.pnl >= 0 && b.pnl < 0)) {
      const t = a.pnl === b.pnl ? 0 : -a.pnl / (b.pnl - a.pnl);
      breakevens.push(a.s + t * (b.s - a.s));
    }
  }

  return { points, loRange, hiRange, maxLoss, maxProfit, breakevens };
}

function money(v: number | "unlimited"): string {
  return v === "unlimited" ? "Unlimited" : `$${Math.abs(v).toFixed(2)}`;
}

function PayoffDiagram({ payoff }: { payoff: Payoff }) {
  const W = 400;
  const H = 140;
  const pad = 4;
  const pnls = payoff.points.map((p) => p.pnl);
  const minPnl = Math.min(...pnls, 0);
  const maxPnl = Math.max(...pnls, 0);
  const pnlRange = Math.max(maxPnl - minPnl, 0.01);

  const x = (s: number) => pad + ((s - payoff.loRange) / (payoff.hiRange - payoff.loRange)) * (W - 2 * pad);
  const y = (pnl: number) => H - pad - ((pnl - minPnl) / pnlRange) * (H - 2 * pad);

  const path = payoff.points.map((p, i) => `${i === 0 ? "M" : "L"}${x(p.s).toFixed(1)},${y(p.pnl).toFixed(1)}`).join(" ");
  const zeroY = y(0);

  return (
    <svg width="100%" height={H} viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img" aria-label="Payoff at expiry">
      <line x1={0} y1={zeroY} x2={W} y2={zeroY} stroke="var(--mantine-color-dark-3)" strokeWidth={1} />
      {payoff.breakevens.map((be) => (
        <line
          key={be}
          x1={x(be)}
          y1={0}
          x2={x(be)}
          y2={H}
          stroke="var(--mantine-color-dark-2)"
          strokeWidth={1}
          strokeDasharray="3 3"
        />
      ))}
      <path d={path} fill="none" stroke="var(--mantine-color-depth-6)" strokeWidth={2} strokeLinejoin="round" />
    </svg>
  );
}

// Outcomes where the order is actually live at the venue — the only ones
// worth calling "sent" in the summary toast/title.
const LIVE_KINDS = new Set<SubmitOutcome["kind"]>(["sent", "idempotent_live"]);
// Outcomes where the order exists in the OMS (worth following in the blotter)
// but never reached, or is no longer at, the venue.
const PERSISTED_NOT_LIVE_KINDS = new Set<SubmitOutcome["kind"]>([
  "idempotent_recorded",
  "broker_rejected",
  "no_broker",
  "idempotent_terminal",
]);

function describeOutcome(outcome: SubmitOutcome): { color: string; message: string } {
  switch (outcome.kind) {
    case "sent":
      return { color: "green", message: "Sent" };
    case "idempotent_live":
      return { color: "green", message: `Sent — status ${outcome.status}` };
    case "idempotent_recorded":
      return { color: "orange", message: `Not routed. ${RECORDED_NOT_ROUTED}` };
    case "idempotent_terminal":
      return { color: "red", message: `Already ${outcome.status}` };
    case "idempotent_unknown":
      return { color: "orange", message: "Status unknown — check the blotter" };
    case "rejected":
      return { color: "red", message: outcome.message };
    case "broker_rejected":
      return { color: "red", message: `${outcome.message} ${RECORDED_NOT_ROUTED}` };
    case "no_broker":
      return { color: "orange", message: `No broker configured. ${RECORDED_NOT_ROUTED}` };
    case "not_permitted":
      return { color: "red", message: "Not permitted" };
    case "unknown_error":
      return { color: "red", message: outcome.message };
  }
}

export function StrategyTicket({
  portfolios,
  legs,
  onCancel,
  onSubmitted,
}: {
  portfolios: GrantedPortfolio[];
  legs: StrategyLeg[];
  onCancel: () => void;
  onSubmitted: (orderIds: string[]) => void;
}) {
  const tradeable = portfolios.filter((p) => p.can_trade && p.status === "ACTIVE");
  const [portfolioId, setPortfolioId] = useState<string | null>(
    tradeable.length === 1 ? tradeable[0].portfolio_id : null,
  );
  const [orderType, setOrderType] = useState<OrderType>("market");
  const [tif, setTif] = useState<TimeInForce>("day");
  const [phase, setPhase] = useState<"edit" | "confirm" | "done">("edit");
  const [submitting, setSubmitting] = useState(false);
  const [statuses, setStatuses] = useState<Record<string, LegStatus>>({});
  const [messages, setMessages] = useState<Record<string, string>>({});
  const [colors, setColors] = useState<Record<string, string>>({});

  // One id per leg, generated once for this ticket's lifetime — not on every
  // submit attempt. A retry after every leg failed with an unclear error
  // (idempotent_unknown / unknown_error) reuses the SAME order/tag ids, so it
  // is a true idempotent retry rather than a second, distinct set of orders.
  const [clientOrderId] = useState(() => `combo-${crypto.randomUUID().slice(0, 8)}`);
  const [legOrderIds] = useState<Record<string, string>>(() =>
    Object.fromEntries(legs.map((l) => [l.instrumentId + l.side, crypto.randomUUID()])),
  );

  // Representative venue lookup: a strategy's legs are the same underlying's
  // option contracts, so whichever accounts are eligible for the first leg
  // are assumed eligible for the rest. If that assumption is wrong for a
  // particular leg, that leg's own submitOrder call surfaces the real 422 —
  // nothing here silently forces it through.
  const firstLegId = legs[0]?.instrumentId;
  const venues = useQuery<VenueOption[]>({
    queryKey: ["/portfolios", portfolioId, "venues", firstLegId],
    queryFn: () => tradeApi.get<VenueOption[]>(`/portfolios/${portfolioId}/venues?instrument_id=${firstLegId}`),
    enabled: !!portfolioId && !!firstLegId,
  });
  const eligibleVenue = venues.data?.find((v) => v.eligible);
  const eligibleAccountId = eligibleVenue?.account_id ?? undefined;

  // Net premium is reference-only and only meaningful when every leg actually
  // has a price — a leg with no live mark (referencePrice null) makes the
  // total unknowable, never silently 0 (see types.ts's MarkRow contract note).
  const pricedLegs = legs.filter((l) => l.referencePrice != null);
  const netPremium = pricedLegs.reduce(
    (sum, l) => sum + (l.side === "buy" ? l.referencePrice! : -l.referencePrice!),
    0,
  );
  const allLegsPriced = pricedLegs.length === legs.length;
  const payoff = allLegsPriced && legs.length > 0 ? computePayoff(legs) : null;

  async function confirmSubmit() {
    if (!portfolioId) return;
    setSubmitting(true);
    const liveOrderIds: string[] = [];
    const persistedOrderIds: string[] = [];

    // Strictly sequential: leg n+1 only starts once leg n has resolved.
    // A failed leg neither rolls back an earlier one nor blocks a later one.
    for (const leg of legs) {
      const key = leg.instrumentId + leg.side;
      setStatuses((s) => ({ ...s, [key]: "sending" }));
      const orderId = legOrderIds[key];
      const outcome = await submitOrder({
        orderId,
        clientOrderId,
        portfolioId,
        accountId: eligibleAccountId,
        instrumentId: leg.instrumentId,
        side: leg.side,
        quantity: 1,
        orderType,
        timeInForce: tif,
      });
      const { color, message } = describeOutcome(outcome);
      setStatuses((s) => ({ ...s, [key]: outcome.kind }));
      setMessages((m) => ({ ...m, [key]: message }));
      setColors((c) => ({ ...c, [key]: color }));
      if (LIVE_KINDS.has(outcome.kind)) liveOrderIds.push(orderId);
      else if (PERSISTED_NOT_LIVE_KINDS.has(outcome.kind)) persistedOrderIds.push(orderId);
    }

    setSubmitting(false);
    setPhase("done");

    const followIds = [...liveOrderIds, ...persistedOrderIds];
    if (liveOrderIds.length === legs.length) {
      notifications.show({
        color: "green",
        title: `${liveOrderIds.length}/${legs.length} legs sent`,
        message: `Tag: ${clientOrderId}`,
      });
    } else if (followIds.length > 0) {
      notifications.show({
        color: "orange",
        title: `${liveOrderIds.length}/${legs.length} legs sent`,
        message: `${persistedOrderIds.length} recorded but NOT routed, ${
          legs.length - followIds.length
        } failed outright. See each leg below. Tag: ${clientOrderId}`,
        autoClose: false,
      });
    } else {
      notifications.show({
        color: "red",
        title: "No legs sent",
        message: `Nothing reached the OMS. Tag: ${clientOrderId}`,
        autoClose: false,
      });
    }
    if (followIds.length > 0) onSubmitted(followIds);
  }

  return (
    <Paper withBorder p="md">
      <Stack gap="sm">
        <Group justify="space-between">
          <Title order={4}>Strategy ({legs.length} legs)</Title>
          <Button variant="subtle" size="xs" onClick={onCancel} disabled={submitting}>
            {phase === "done" ? "Close" : "Cancel"}
          </Button>
        </Group>

        {legs.map((leg) => {
          const key = leg.instrumentId + leg.side;
          const status = statuses[key];
          return (
            <Group key={key} justify="space-between">
              <Group gap="xs">
                <Badge color={leg.side === "buy" ? "depth" : "offer"}>{leg.side.toUpperCase()}</Badge>
                <Text size="sm">
                  {leg.optionKind} {leg.strike} · {leg.expiry}
                </Text>
              </Group>
              <Group gap="xs">
                <Text size="sm" c="dimmed">
                  {leg.referencePrice != null ? leg.referencePrice.toFixed(2) : "—"}
                </Text>
                {status === "sending" && (
                  <Text size="xs" c="dimmed">
                    Sending…
                  </Text>
                )}
                {status && status !== "sending" && (
                  <Text size="xs" c={colors[key]}>
                    {messages[key]}
                  </Text>
                )}
              </Group>
            </Group>
          );
        })}

        {payoff ? (
          <>
            <PayoffDiagram payoff={payoff} />
            <SimpleGrid cols={4} spacing="xs">
              <Stack gap={0} align="center">
                <Text size="xs" c="dimmed">
                  Max loss
                </Text>
                <Text size="sm" c="offer" fw={600}>
                  {money(payoff.maxLoss)}
                </Text>
              </Stack>
              <Stack gap={0} align="center">
                <Text size="xs" c="dimmed">
                  Max profit
                </Text>
                <Text size="sm" c="depth" fw={600}>
                  {money(payoff.maxProfit)}
                </Text>
              </Stack>
              <Stack gap={0} align="center">
                <Text size="xs" c="dimmed">
                  Breakeven{payoff.breakevens.length === 1 ? "" : "s"}
                </Text>
                <Text size="sm" fw={600}>
                  {payoff.breakevens.length > 0 ? payoff.breakevens.map((b) => b.toFixed(2)).join(" / ") : "—"}
                </Text>
              </Stack>
              <Stack gap={0} align="center">
                <Text size="xs" c="dimmed">
                  Net {netPremium >= 0 ? "debit" : "credit"}
                </Text>
                <Text size="sm" fw={600}>
                  ${Math.abs(netPremium).toFixed(2)}
                </Text>
              </Stack>
            </SimpleGrid>
          </>
        ) : (
          <Text size="sm" c="dimmed">
            Payoff diagram needs a live price on every leg — unavailable right now.
          </Text>
        )}

        {phase !== "done" && (
          <>
            <Select
              label="Portfolio"
              data={tradeable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
              value={portfolioId}
              onChange={setPortfolioId}
              disabled={phase === "confirm"}
            />
            <Select
              label="Order type"
              data={[{ value: "market", label: "Market" }]}
              value={orderType}
              onChange={(v) => v && setOrderType(v as OrderType)}
              disabled={phase === "confirm"}
            />
            <Select
              label="Time in force"
              data={[
                { value: "day", label: "Day" },
                { value: "gtc", label: "GTC" },
                { value: "ioc", label: "IOC" },
                { value: "fok", label: "FOK" },
              ]}
              value={tif}
              onChange={(v) => v && setTif(v as TimeInForce)}
              disabled={phase === "confirm"}
            />
          </>
        )}

        {phase === "edit" && (
          <Button
            disabled={!portfolioId || venues.isLoading}
            loading={venues.isLoading && !!portfolioId}
            onClick={() => setPhase("confirm")}
          >
            Review {legs.length} leg{legs.length === 1 ? "" : "s"}
          </Button>
        )}

        {phase === "confirm" && (
          <>
            <Alert color="blue" title="Confirm strategy">
              <Stack gap={4}>
                <Text size="sm">
                  {legs.length} leg{legs.length === 1 ? "" : "s"} · {orderType} · {tif} · portfolio{" "}
                  {tradeable.find((p) => p.portfolio_id === portfolioId)?.code}
                </Text>
                <Text size="sm">
                  Route:{" "}
                  {eligibleVenue
                    ? `${eligibleVenue.broker_code} ${eligibleVenue.environment ?? ""}`
                    : "portfolio default account"}
                </Text>
              </Stack>
            </Alert>
            <Group grow>
              <Button variant="default" onClick={() => setPhase("edit")} disabled={submitting}>
                Back
              </Button>
              <Button loading={submitting} onClick={confirmSubmit}>
                Confirm &amp; submit
              </Button>
            </Group>
          </>
        )}
      </Stack>
    </Paper>
  );
}
