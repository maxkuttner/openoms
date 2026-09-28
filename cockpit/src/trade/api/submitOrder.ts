import { tradeApi, ApiError } from "./client";
import type { Side, OrderType, TimeInForce } from "../types";

// GET /orders/{id}, of which only the status is used here. Mirrors
// OrderAggregateState in src/domain/orders/state.rs.
type OrderState = { order_id: string; status: string };

// The statuses that mean the venue has the order. Anything else — notably
// `submitted` — means the OMS wrote the order down but the broker does not
// (yet) have it.
const LIVE_AT_VENUE = new Set(["routed", "partially_filled", "filled"]);

// Said whenever an order exists in the OMS but was never handed to a broker.
// Nothing downstream will move it, so it sits at `submitted` forever unless
// someone cancels it; the local cancel path (no external_order_id) works.
export const RECORDED_NOT_ROUTED =
  "The order was RECORDED but NOT routed to the broker — nothing was sent to the venue. " +
  "Cancel it in the blotter to clear it.";

export type SubmitOrderParams = {
  orderId: string;
  clientOrderId: string;
  portfolioId: string;
  accountId?: string;
  instrumentId: string;
  side: Side;
  quantity: number;
  orderType: OrderType;
  timeInForce: TimeInForce;
  limitPrice?: number;
};

// Every shape POST /orders/submit's response can resolve to. This is exactly
// the branching OrderTicket.tsx's confirmSubmit used to do inline — moved
// here so a second caller (a multi-leg ticket) doesn't have to reimplement
// it. The caller still owns every notification/reset/onSubmitted decision;
// this only classifies what happened.
export type SubmitOutcome =
  | { kind: "sent" }
  | { kind: "idempotent_live"; status: string }
  | { kind: "idempotent_recorded" }
  | { kind: "idempotent_terminal"; status: string }
  | { kind: "idempotent_unknown" }
  | { kind: "rejected"; message: string }
  | { kind: "broker_rejected"; message: string }
  | { kind: "no_broker" }
  | { kind: "not_permitted" }
  | { kind: "unknown_error"; message: string };

// A 409 means this exact order_id already exists — the only honest answer is
// the server's own view of that order, never a guess about what a retry
// "usually" means (see SubmitOutcome's idempotent_* variants).
async function classifyReplay(orderId: string): Promise<SubmitOutcome> {
  let status: string | null = null;
  try {
    const order = await tradeApi.get<OrderState>(`/orders/${orderId}`);
    status = order?.status ?? null;
  } catch {
    status = null;
  }
  if (status === null) return { kind: "idempotent_unknown" };
  if (LIVE_AT_VENUE.has(status)) return { kind: "idempotent_live", status };
  if (status === "submitted") return { kind: "idempotent_recorded" };
  return { kind: "idempotent_terminal", status };
}

export async function submitOrder(params: SubmitOrderParams): Promise<SubmitOutcome> {
  try {
    await tradeApi.post("/orders/submit", {
      order_id: params.orderId,
      client_order_id: params.clientOrderId,
      portfolio_id: params.portfolioId,
      instrument_id: params.instrumentId,
      side: params.side,
      quantity: params.quantity,
      order_type: params.orderType,
      time_in_force: params.timeInForce,
      limit_price: params.orderType === "limit" ? params.limitPrice : undefined,
      account_id: params.accountId,
    });
    return { kind: "sent" };
  } catch (err) {
    if (err instanceof ApiError) {
      switch (err.status) {
        case 409:
          return classifyReplay(params.orderId);
        case 422:
          return { kind: "rejected", message: err.message };
        case 502:
          return { kind: "broker_rejected", message: err.message };
        case 503:
          return { kind: "no_broker" };
        case 403:
          return { kind: "not_permitted" };
        default:
          return { kind: "unknown_error", message: `${err.status}: ${err.message}` };
      }
    }
    return { kind: "unknown_error", message: String(err) };
  }
}
