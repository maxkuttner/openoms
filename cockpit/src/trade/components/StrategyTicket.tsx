import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { notifications } from "@mantine/notifications";
import { Badge, Button, Group, Paper, Select, Stack, Text, Title } from "@mantine/core";
import { tradeApi } from "../api/client";
import { submitOrder, RECORDED_NOT_ROUTED, type SubmitOutcome } from "../api/submitOrder";
import type { GrantedPortfolio } from "../App";
import type { StrategyLeg, VenueOption, OrderType, TimeInForce } from "../types";

type LegStatus = "pending" | "sending" | SubmitOutcome["kind"];

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
  const [submitting, setSubmitting] = useState(false);
  const [statuses, setStatuses] = useState<Record<string, LegStatus>>({});
  const [messages, setMessages] = useState<Record<string, string>>({});

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
  const eligibleAccountId = venues.data?.find((v) => v.eligible)?.account_id ?? undefined;

  const netPremium = legs.reduce((sum, l) => sum + (l.side === "buy" ? l.referencePrice ?? 0 : -(l.referencePrice ?? 0)), 0);

  async function confirmSubmit() {
    if (!portfolioId) return;
    setSubmitting(true);
    const clientOrderId = `combo-${crypto.randomUUID().slice(0, 8)}`;
    const sentOrderIds: string[] = [];

    // Strictly sequential: leg n+1 only starts once leg n has resolved.
    // A failed leg neither rolls back an earlier one nor blocks a later one.
    for (const leg of legs) {
      setStatuses((s) => ({ ...s, [leg.instrumentId + leg.side]: "sending" }));
      const orderId = crypto.randomUUID();
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
      setStatuses((s) => ({ ...s, [leg.instrumentId + leg.side]: outcome.kind }));
      setMessages((m) => ({ ...m, [leg.instrumentId + leg.side]: message }));
      if (outcome.kind !== "rejected" && outcome.kind !== "not_permitted" && outcome.kind !== "unknown_error") {
        sentOrderIds.push(orderId);
      }
      void color;
    }

    setSubmitting(false);
    if (sentOrderIds.length > 0) {
      notifications.show({
        color: sentOrderIds.length === legs.length ? "green" : "orange",
        title: `${sentOrderIds.length}/${legs.length} legs sent`,
        message: `Tag: ${clientOrderId}`,
      });
      onSubmitted(sentOrderIds);
    } else {
      notifications.show({ color: "red", title: "No legs sent", message: `Tag: ${clientOrderId}` });
    }
  }

  return (
    <Paper withBorder p="md">
      <Stack gap="sm">
        <Group justify="space-between">
          <Title order={4}>Strategy ({legs.length} legs)</Title>
          <Button variant="subtle" size="xs" onClick={onCancel} disabled={submitting}>
            Cancel
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
                {status && <Text size="xs">{messages[key]}</Text>}
              </Group>
            </Group>
          );
        })}

        <Text size="sm" c="dimmed">
          Net {netPremium >= 0 ? "debit" : "credit"} {Math.abs(netPremium).toFixed(2)} (reference only)
        </Text>

        <Select
          label="Portfolio"
          data={tradeable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
          value={portfolioId}
          onChange={setPortfolioId}
        />
        <Select
          label="Order type"
          data={[
            { value: "market", label: "Market" },
            { value: "limit", label: "Limit" },
          ]}
          value={orderType}
          onChange={(v) => v && setOrderType(v as OrderType)}
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
        />

        <Button disabled={!portfolioId || submitting} loading={submitting} onClick={confirmSubmit}>
          Submit {legs.length} leg{legs.length === 1 ? "" : "s"}
        </Button>
      </Stack>
    </Paper>
  );
}
