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

// What GET /orders/{id} answers with; only the status matters here.
// Mirrors OrderAggregateState in src/domain/orders/state.rs.
type OrderState = { order_id: string; status: string };

// The statuses that mean the venue has the order. Anything else — notably
// `submitted` — means the OMS wrote the order down but the broker does not
// (yet) have it.
const LIVE_AT_VENUE = new Set(["routed", "partially_filled", "filled"]);

// Said whenever an order exists in the OMS but was never handed to a broker.
// Nothing downstream will move it, so it sits at `submitted` forever unless
// someone cancels it; the local cancel path (no external_order_id) works.
const RECORDED_NOT_ROUTED =
  "The order was RECORDED but NOT routed to the broker — nothing was sent to the venue. " +
  "Cancel it in the blotter to clear it.";

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
  //
  // `can_trade` alone is not enough: the grant outlives the portfolio's own
  // lifecycle, so a CLOSED portfolio with a stale grant would still be offered
  // and every order against it would fail downstream. Both must hold.
  const tradeable = portfolios.filter((p) => p.can_trade && p.status === "ACTIVE");

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

  // Reads an order back and says what it actually is. Used for 409, where the
  // only honest answer is the server's own view of that order_id — a 409 can
  // mean "your retry landed on a live order" or "you are re-sending an id that
  // was recorded but never routed", and those are opposite outcomes.
  async function reportExistingOrder(id: string) {
    let status: string | null = null;
    try {
      const order = await tradeApi.get<OrderState>(`/orders/${id}`);
      status = order?.status ?? null;
    } catch {
      // Could not read it back. Say exactly that rather than pick a story.
      status = null;
    }

    if (status === null) {
      notifications.show({
        color: "orange",
        title: "Already submitted — status unknown",
        message:
          "An order with this id already exists, but reading it back failed. " +
          "Check the blotter before sending anything else.",
        autoClose: false,
      });
      return;
    }

    if (LIVE_AT_VENUE.has(status)) {
      notifications.show({
        color: "green",
        title: "Order sent",
        message: `${side === "buy" ? "Buy" : "Sell"} ${quantity} ${instrumentLabel} — status ${status}.`,
      });
      return;
    }

    if (status === "submitted") {
      notifications.show({
        color: "orange",
        title: "Order NOT sent",
        message: `This order id already exists and is still \`submitted\`. ${RECORDED_NOT_ROUTED}`,
        autoClose: false,
      });
      return;
    }

    // rejected / canceled / expired / suspended — it exists and it is not live.
    notifications.show({
      color: "red",
      title: `Order NOT sent — already ${status}`,
      message: `An order with this id already exists and its status is ${status}. See the blotter.`,
      autoClose: false,
    });
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
            // This exact order_id already exists. That is USUALLY the
            // idempotency contract absorbing a repeat (double-click, retry,
            // dropped-then-resent connection) — but it is NOT automatically
            // success, and asserting that it is, is how this screen came to
            // tell a trader an order was sent when it was not.
            //
            // The server commits the order row before it routes (see
            // orders_submit in src/handlers.rs), so a 502 ("broker rejected")
            // or 503 ("no adapter registered") leaves the order persisted at
            // `submitted` with the same order_id. Re-confirming after such a
            // failure — say, with a corrected limit price — then answers 409,
            // and the old code showed "Order sent".
            //
            // So we do not guess: we read the order back and report its real
            // status. The id is spent either way, hence reset().
            setConfirmOpen(false);
            await reportExistingOrder(orderId);
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
            // Broker rejected the order; the venue's own message, verbatim —
            // plus what the trader cannot see from the blotter, because no
            // route-failure event is written: the order row already exists and
            // is stuck at `submitted`, indistinguishable from a live one.
            notifications.show({
              color: "red",
              title: "Broker rejected — order NOT sent",
              message: `${err.message} ${RECORDED_NOT_ROUTED}`,
              autoClose: false,
            });
            // The order exists, so point the blotter at it: that row is the
            // one the trader has to cancel.
            onSubmitted(orderId);
            break;
          case 503:
            notifications.show({
              color: "orange",
              title: "No broker configured — order NOT sent",
              message: `An operator needs to configure a broker connection. ${RECORDED_NOT_ROUTED}`,
              autoClose: false,
            });
            onSubmitted(orderId);
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

  // A trader with no tradeable portfolio is never shown a ticket they cannot
  // submit (the spec's rule for a view-only trader). Showing the form with an
  // empty portfolio dropdown invites them to fill it in and discover at the
  // confirmation that there is nothing to send it against.
  if (tradeable.length === 0) {
    return (
      <Paper withBorder p="md">
        <Stack gap="xs">
          <Title order={3}>Order ticket</Title>
          <Text c="dimmed" size="sm">
            None of your portfolios can be traded: you either have view-only access, or the
            portfolios you may trade are closed. The blotter and positions still work.
          </Text>
        </Stack>
      </Paper>
    );
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
