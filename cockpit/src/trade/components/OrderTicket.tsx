import { useState } from "react";
import { notifications } from "@mantine/notifications";
import { Button, Group, Modal, NumberInput, Paper, Select, Stack, Text, Title } from "@mantine/core";
import { tradeApi, ApiError } from "../api/client";
import { InstrumentSelect, type Instrument } from "../../components/InstrumentSelect";
import type { GrantedPortfolio } from "../App";

type Side = "buy" | "sell";
type OrderType = "market" | "limit";
type TimeInForce = "day" | "gtc" | "ioc" | "fok";

function notifyError(err: unknown) {
  const message = err instanceof ApiError ? `${err.status}: ${err.message}` : String(err);
  notifications.show({ message, color: "red" });
}

// The order ticket. Sits beside the blotter on the trade screen: fills out an
// order, forces a confirmation step that states the order in words, and only
// the confirmation's own button ever calls the API.
export function OrderTicket({
  portfolios,
  onSubmitted,
}: {
  portfolios: GrantedPortfolio[];
  onSubmitted: (orderId: string) => void;
}) {
  // 403 from the server should be unreachable because of this filter — see the
  // 403 branch below, which treats it as a bug report rather than a routine
  // rejection.
  const tradeable = portfolios.filter((p) => p.can_trade);

  const [portfolioId, setPortfolioId] = useState<string | null>(
    tradeable.length === 1 ? tradeable[0].portfolio_id : null,
  );
  const [instrumentId, setInstrumentId] = useState<string | null>(null);
  // The full row for the currently selected instrument, handed up by
  // InstrumentSelect's onSelected alongside its onChange — it already holds
  // this in memory from the search results, so there is no second fetch.
  // Since it can only ever be a row the user just picked from the dropdown,
  // this is correct for ANY instrument, not just a sample of the catalog.
  const [selectedInstrument, setSelectedInstrument] = useState<Instrument | null>(null);
  const [side, setSide] = useState<Side>("buy");
  const [quantity, setQuantity] = useState<number | string>("");
  const [orderType, setOrderType] = useState<OrderType>("market");
  const [tif, setTif] = useState<TimeInForce>("day");
  const [limitPrice, setLimitPrice] = useState<number | string>("");

  // The idempotency key. POST /orders/submit takes a client-generated order_id
  // which IS the idempotency key — a repeat of the same id is answered with
  // 409, and the server treats that as the same order, not a new one. It is
  // generated exactly once per ticket, here, at mount. It is regenerated in
  // exactly two other places, both below in this file: at the end of
  // `reset()` (covers an explicit reset AND the state right after a
  // successful/409-absorbed submit, since submit calls reset()) — never on
  // every click, and never inside the click handler that opens the
  // confirmation modal.
  const [orderId, setOrderId] = useState(() => crypto.randomUUID());

  const [confirmOpen, setConfirmOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  // "SYMBOL@VENUE" for the confirmation text, from the row InstrumentSelect
  // handed up — real for any instrument the user could have picked, since it
  // is that same picked row, not a lookup against a capped/alphabetical list.
  const instrumentLabel = selectedInstrument
    ? `${selectedInstrument.symbol}@${selectedInstrument.venue}`
    : (instrumentId ?? "");

  const portfolioLabel = tradeable.find((p) => p.portfolio_id === portfolioId)?.code ?? (portfolioId ?? "");

  const canConfirm =
    !!portfolioId &&
    !!instrumentId &&
    Number(quantity) > 0 &&
    (orderType === "market" || Number(limitPrice) > 0);

  // Clears every field AND rolls the idempotency key. Called both for an
  // explicit reset and right after a submit lands (success or the 409 that
  // means "already landed") — a ticket that starts a new order must never
  // carry a key that already identifies a previous one.
  function reset() {
    setInstrumentId(null);
    setSelectedInstrument(null);
    setSide("buy");
    setQuantity("");
    setOrderType("market");
    setTif("day");
    setLimitPrice("");
    setOrderId(crypto.randomUUID());
  }

  async function confirmSubmit() {
    setSubmitting(true);
    try {
      await tradeApi.post("/orders/submit", {
        order_id: orderId,
        // The server requires its own free-text reference distinct from the
        // idempotency key (client_order_id: String, not optional — see
        // SubmitOrderRequest in src/handlers.rs). Nothing in this UI needs a
        // second identity for an order it only ever submits once, so it
        // reuses order_id here too.
        client_order_id: orderId,
        portfolio_id: portfolioId,
        instrument_id: instrumentId,
        side,
        quantity: Number(quantity),
        order_type: orderType,
        time_in_force: tif,
        limit_price: orderType === "limit" ? Number(limitPrice) : undefined,
      });
      notifications.show({
        color: "green",
        title: "Order sent",
        message: `${side === "buy" ? "Buy" : "Sell"} ${quantity} ${instrumentLabel}`,
      });
      setConfirmOpen(false);
      onSubmitted(orderId);
      reset();
    } catch (err) {
      if (err instanceof ApiError) {
        switch (err.status) {
          case 409:
            // This exact order_id already exists — the idempotency contract
            // absorbing a repeat (double-click, retry, dropped-then-resent
            // connection). The order is live at the venue: this is success,
            // not failure.
            setConfirmOpen(false);
            onSubmitted(orderId);
            reset();
            break;
          case 422:
            // Unknown/inactive instrument, no tradeable broker mapping, or a
            // pre-trade risk rejection. The server's message carries the
            // reason (e.g. "risk check failed [...]: notional limit
            // breached") and is shown verbatim — it is information the
            // trader needs, not a generic failure.
            notifications.show({ color: "red", title: "Rejected", message: err.message });
            break;
          case 502:
            // Broker rejected the order; the venue's own message, verbatim.
            notifications.show({ color: "red", title: "Broker rejected", message: err.message });
            break;
          case 503:
            notifications.show({
              color: "orange",
              title: "No broker configured",
              message: "An operator needs to configure a broker connection.",
            });
            break;
          case 403:
            // portfolios is already filtered to can_trade above, so this
            // should be unreachable. Presented as a bug to report, not a
            // routine rejection.
            notifications.show({
              color: "red",
              title: "Not permitted",
              message: "This portfolio is not tradeable by your account. Please report this.",
            });
            break;
          default:
            notifyError(err);
        }
      } else {
        notifyError(err);
      }
      // No case for 401: tradeApi redirects to the identity provider on 401
      // and returns a promise that never resolves, so this catch never runs
      // for it — accepted behaviour, not worked around. The page navigates
      // away entirely; on return from a fresh login this component remounts,
      // so every field (and the idempotency key) starts empty rather than
      // replaying an order the trader may no longer intend.
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Paper withBorder p="md">
      <Stack>
        <Title order={3}>Order ticket</Title>

        <Select
          label="Portfolio"
          placeholder="Select portfolio"
          required
          data={tradeable.map((p) => ({ value: p.portfolio_id, label: p.code }))}
          value={portfolioId}
          onChange={setPortfolioId}
        />

        <InstrumentSelect
          label="Instrument"
          required
          value={instrumentId}
          onChange={setInstrumentId}
          onSelected={setSelectedInstrument}
          basePath="/instruments"
          apiGet={tradeApi.get}
        />

        <Group grow>
          <Select
            label="Side"
            data={[
              { value: "buy", label: "Buy" },
              { value: "sell", label: "Sell" },
            ]}
            value={side}
            onChange={(v) => v && setSide(v as Side)}
            allowDeselect={false}
          />
          <NumberInput label="Quantity" required min={0} value={quantity} onChange={setQuantity} />
        </Group>

        <Group grow>
          <Select
            label="Order type"
            data={[
              { value: "market", label: "Market" },
              { value: "limit", label: "Limit" },
            ]}
            value={orderType}
            onChange={(v) => v && setOrderType(v as OrderType)}
            allowDeselect={false}
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
            allowDeselect={false}
          />
        </Group>

        {orderType === "limit" && (
          <NumberInput
            label="Limit price"
            required
            min={0}
            decimalScale={4}
            value={limitPrice}
            onChange={setLimitPrice}
          />
        )}

        <Group justify="flex-end">
          <Button variant="subtle" onClick={reset}>
            Reset
          </Button>
          <Button disabled={!canConfirm} onClick={() => setConfirmOpen(true)}>
            Review order
          </Button>
        </Group>
      </Stack>

      {/* The confirmation is the only path to the API: no other button here
          ever calls tradeApi.post. It restates the resolved order in words so
          the trader confirms what will actually be sent, not just that they
          clicked a button. */}
      <Modal opened={confirmOpen} onClose={() => !submitting && setConfirmOpen(false)} title="Confirm order">
        <Stack>
          <Text>
            {side === "buy" ? "Buy" : "Sell"} {quantity} {instrumentLabel}
            {orderType === "limit" ? ` · limit ${limitPrice}` : " · market"} · {tif} · portfolio {portfolioLabel}
          </Text>
          <Group justify="flex-end">
            <Button variant="default" onClick={() => setConfirmOpen(false)} disabled={submitting}>
              Cancel
            </Button>
            <Button color={side === "buy" ? "green" : "red"} loading={submitting} onClick={confirmSubmit}>
              Confirm {side}
            </Button>
          </Group>
        </Stack>
      </Modal>
    </Paper>
  );
}
