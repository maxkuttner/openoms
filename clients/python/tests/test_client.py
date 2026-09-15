"""Unit tests for the OMS client — no server, no network.

Everything here is about the rules that are easy to get wrong and expensive when
wrong: that a plain-text error becomes the right exception, that a 409 on a retry is
treated as success rather than failure, and that a 202 cancel is not mistaken for a
completed one.

The transport is stubbed with a fake `requests.Session`, so these run anywhere.
"""

from __future__ import annotations

import pytest

from oms_client import OMS
from oms_client.errors import (
    AlreadyExists,
    AuthError,
    BrokerRejected,
    Forbidden,
    NotFound,
    OMSError,
    Rejected,
)
from oms_client.models import Order


class FakeResponse:
    def __init__(self, status_code: int, payload=None, text: str = ""):
        self.status_code = status_code
        self._payload = payload
        self.text = text

    def json(self):
        return self._payload


class FakeSession:
    """Stands in for requests.Session, returning queued responses and recording calls."""

    def __init__(self, *responses: FakeResponse):
        self.headers = {}
        self._responses = list(responses)
        self.calls = []

    def request(self, method, url, **kw):
        self.calls.append((method, url, kw))
        return self._responses.pop(0) if self._responses else FakeResponse(204)


def client(*responses: FakeResponse) -> tuple:
    session = FakeSession(*responses)
    return OMS("http://oms.test", "ak_x.sk_y", session=session), session


# ── auth ─────────────────────────────────────────────────────────────────────


def test_token_is_sent_as_bearer():
    """The `Bearer key_id.secret` form — one of the two the server accepts."""
    _, session = client()
    assert session.headers["Authorization"] == "Bearer ak_x.sk_y"


# ── error mapping ────────────────────────────────────────────────────────────


@pytest.mark.parametrize(
    "status,exc,body",
    [
        (401, AuthError, "unauthorized"),
        (403, Forbidden, "unauthorized"),
        (404, NotFound, "order not found"),
        (422, Rejected, "instrument not active"),
        (502, BrokerRejected, "broker rejected order: closed"),
    ],
)
def test_status_maps_to_exception(status, exc, body):
    oms, _ = client(FakeResponse(status, text=body))
    with pytest.raises(exc) as caught:
        oms.order("some-id")
    assert caught.value.status == status
    assert caught.value.message == body


def test_unmapped_status_still_raises_oms_error():
    """A status the client has never seen must not escape as a raw requests error."""
    oms, _ = client(FakeResponse(418, text="teapot"))
    with pytest.raises(OMSError) as caught:
        oms.order("some-id")
    assert caught.value.status == 418


def test_error_body_is_never_parsed_as_json():
    """Errors are text/plain; calling .json() on them would raise the wrong thing."""

    class Exploding(FakeResponse):
        def json(self):
            raise AssertionError("json() must not be called on an error response")

    oms, _ = client(Exploding(422, text="risk check failed [invalid_quantity]: too big"))
    with pytest.raises(Rejected) as caught:
        oms.order("some-id")
    assert "invalid_quantity" in caught.value.message


# ── submit ───────────────────────────────────────────────────────────────────


def test_submit_generates_and_returns_order_id():
    """Submit answers 204 with no body, so the id the client made is the only handle."""
    oms, session = client(FakeResponse(204))
    order_id = oms.submit(portfolio="pf", symbol="SPY@ARCX", side="buy", quantity=1)
    assert session.calls[0][2]["json"]["order_id"] == order_id
    assert len(order_id) == 36


def test_submit_swallows_409_because_the_id_is_ours():
    """409 means the OMS already accepted this exact order — a retry, not a failure.

    Raising here would push callers toward resubmitting under a new id, which is how
    you end up with the position twice.
    """
    oms, _ = client(FakeResponse(409, text="order already exists"))
    assert oms.submit(
        portfolio="pf", symbol="SPY@ARCX", side="buy", quantity=1, order_id="fixed-id"
    ) == "fixed-id"


def test_submit_still_raises_on_broker_rejection():
    """502 is not idempotent-safe to ignore: the order exists but never routed."""
    oms, _ = client(FakeResponse(502, text="broker rejected order: market closed"))
    with pytest.raises(BrokerRejected):
        oms.submit(portfolio="pf", symbol="SPY@ARCX", side="buy", quantity=1)


def test_submit_omits_absent_instrument_fields():
    """Sending symbol AND venue when only one was given would trip the server's
    'venue given both in symbol and in the venue field' check."""
    oms, session = client(FakeResponse(204))
    oms.submit(portfolio="pf", symbol="SPY260918C00770000@OPRA", side="buy", quantity=1)
    body = session.calls[0][2]["json"]
    assert "venue" not in body and "instrument_id" not in body
    assert "limit_price" not in body


def test_submit_passes_limit_price_and_venue_when_given():
    oms, session = client(FakeResponse(204))
    oms.submit(
        portfolio="pf", symbol="AAPL", venue="XNAS", side="buy", quantity=10,
        order_type="limit", limit_price=190.5,
    )
    body = session.calls[0][2]["json"]
    assert body["venue"] == "XNAS"
    assert body["limit_price"] == 190.5
    assert body["order_type"] == "limit"


# ── cancel ───────────────────────────────────────────────────────────────────


def test_cancel_202_is_pending_not_done():
    """202 means forwarded to the broker; the order is still live until confirmed."""
    oms, _ = client(FakeResponse(202))
    assert oms.cancel("oid") == "pending"


def test_cancel_204_is_final():
    oms, _ = client(FakeResponse(204))
    assert oms.cancel("oid") == "canceled"


# ── models ───────────────────────────────────────────────────────────────────


ORDER_JSON = {
    "order_id": "o1", "client_order_id": "c1", "portfolio_id": "p1",
    "account_id": "a1", "instrument_id": "42", "side": "buy",
    "order_type": "market", "time_in_force": "day", "limit_price": None,
    "original_qty": 1.0, "leaves_qty": 0.0, "cum_qty": 1.0, "avg_px": 5.0,
    "status": "filled", "resume_to_status": None, "version": 3,
}


def test_order_parses_and_reports_terminal():
    order = Order.from_dict(ORDER_JSON)
    assert order.status == "filled"
    assert order.is_terminal


def test_working_order_is_not_terminal():
    assert not Order.from_dict({**ORDER_JSON, "status": "partially_filled"}).is_terminal


def test_unknown_fields_are_ignored():
    """The server may add a field; a client that has not been updated must not break."""
    order = Order.from_dict({**ORDER_JSON, "brand_new_field": "surprise"})
    assert order.order_id == "o1"


# ── wait_for ─────────────────────────────────────────────────────────────────


def test_wait_for_polls_until_terminal():
    oms, _ = client(
        FakeResponse(200, {**ORDER_JSON, "status": "routed"}),
        FakeResponse(200, {**ORDER_JSON, "status": "partially_filled"}),
        FakeResponse(200, ORDER_JSON),
    )
    assert oms.wait_for("o1", interval=0).status == "filled"


def test_wait_for_times_out_while_still_working():
    oms, _ = client(*[FakeResponse(200, {**ORDER_JSON, "status": "routed"})] * 50)
    with pytest.raises(TimeoutError):
        oms.wait_for("o1", timeout=0, interval=0)


# ── blotter ──────────────────────────────────────────────────────────────────


BLOTTER_JSON = {
    "order_id": "o1", "principal_id": "pr1", "principal_code": "trader",
    "portfolio_id": "p1", "portfolio_code": "PF1", "account_id": "a1",
    "broker_connection_code": "alpaca-paper", "instrument_id": "42",
    "instrument_symbol": "SPY260918C00770000", "instrument_name": "SPY option",
    "side": "buy", "order_type": "market", "status": "filled",
    "original_qty": 1.0, "leaves_qty": 0.0, "cum_qty": 1.0, "avg_px": 5.0,
    "created_at": "2026-08-14T07:00:00Z", "updated_at": "2026-08-14T07:00:01Z",
}


def test_orders_drops_none_filters_from_the_query():
    """A `status=None` sent as a literal would filter on the string 'None'."""
    oms, session = client(FakeResponse(200, [BLOTTER_JSON]))
    rows = oms.orders(limit=10)
    params = session.calls[0][2]["params"]
    assert params == {"limit": 10, "offset": 0}
    assert rows[0].instrument_symbol == "SPY260918C00770000"


def test_orders_keeps_supplied_filters():
    oms, session = client(FakeResponse(200, []))
    oms.orders(status="routed", side="buy")
    params = session.calls[0][2]["params"]
    assert params["status"] == "routed" and params["side"] == "buy"


def test_order_events_returns_the_audit_trail_oldest_first():
    """The timeline is the order's history, and history has an order."""
    session = FakeSession(
        FakeResponse(
            200,
            [
                {
                    "version": 1,
                    "event_id": "e-1",
                    "event_type": "order_submitted",
                    "actor": "oms",
                    "occurred_at": "2026-09-15T14:01:00Z",
                    "recorded_at": "2026-09-15T14:01:00Z",
                    "status_after": "submitted",
                    "correlation_id": None,
                    "causation_id": None,
                    "schema_version": 0,
                    "summary": "submitted buy 100 AAPL limit 190.02 (day)",
                    "payload": {"quantity": 100.0},
                },
                {
                    "version": 2,
                    "event_id": "e-2",
                    "event_type": "order_filled",
                    "actor": "alpaca",
                    "occurred_at": "2026-09-15T14:02:11Z",
                    "recorded_at": "2026-09-15T14:02:11Z",
                    "status_after": "filled",
                    "correlation_id": None,
                    "causation_id": None,
                    "schema_version": 0,
                    "summary": "filled 100 @ 190.02 on XNAS",
                    "payload": {"fill_qty": 100.0},
                },
            ],
        )
    )
    client = OMS("http://oms", token="t", session=session)

    events = client.order_events("o-1")

    assert session.calls[0][1].endswith("/orders/o-1/events")
    assert [e.version for e in events] == [1, 2]
    assert events[1].summary == "filled 100 @ 190.02 on XNAS"
    assert events[1].actor == "alpaca"
    assert events[1].payload == {"fill_qty": 100.0}


def test_an_order_event_tolerates_a_field_the_client_does_not_know():
    """`_of` drops unknown keys so the server can add fields freely."""
    session = FakeSession(
        FakeResponse(
            200,
            [
                {
                    "version": 1,
                    "event_id": "e-1",
                    "event_type": "order_routed",
                    "actor": "oms",
                    "occurred_at": "2026-09-15T14:01:00Z",
                    "recorded_at": "2026-09-15T14:01:00Z",
                    "status_after": "routed",
                    "schema_version": 0,
                    "summary": "routed to XNAS as 9912",
                    "payload": {},
                    "a_field_from_the_future": "ignored",
                }
            ],
        )
    )
    client = OMS("http://oms", token="t", session=session)

    events = client.order_events("o-1")

    assert events[0].summary == "routed to XNAS as 9912"
    assert not hasattr(events[0], "a_field_from_the_future")


def test_orders_history_is_wired_into_the_cli():
    from oms_client.cli import build_parser, cmd_orders_history

    args = build_parser().parse_args(["orders", "history", "o-1"])

    assert args.func is cmd_orders_history
    assert args.order_id == "o-1"
