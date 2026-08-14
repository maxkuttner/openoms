"""Synchronous client for the OMS trading API.

One trading token is all this needs — it never touches the admin surface. That is
deliberate: the admin token is a single shared secret covering principal management,
broker connections, and risk limits, so a script holding it could mint itself
credentials and edit the limits meant to constrain it.

Three properties of the wire protocol shape this whole module:

* **Submit returns 204 with an empty body.** The client generates `order_id` and that
  id *is* the idempotency key. Nothing comes back, so observing the order means
  fetching it (`order`) or waiting on it (`wait_for`).
* **Cancel is asynchronous when the order is live at a broker.** 202 means the cancel
  was forwarded and will be finalized by the execution stream; only 204 means done.
* **Errors are plain text.** See `errors.raise_for_status`.

There is no push channel — no SSE, no WebSocket — so `wait_for` polls. That is a
property of the server, not a shortcut here.
"""

from __future__ import annotations

import time
import uuid
from typing import Any, Dict, List, Optional

import requests

from .errors import AlreadyExists, OMSError, raise_for_status
from .models import (
    Allocation,
    BlotterRow,
    Order,
    Portfolio,
    Position,
    RowList,
    parse_list,
)

DEFAULT_TIMEOUT = 30.0


class OMS:
    """A trading-token client.

    >>> oms = OMS("http://localhost:3001", token="ak_....sk_....")
    >>> oid = oms.submit(portfolio="3a8d…", symbol="SPY260918C00770000@OPRA",
    ...                  side="buy", quantity=1)
    >>> oms.wait_for(oid).status
    'filled'
    """

    def __init__(
        self,
        base_url: str,
        token: str,
        *,
        timeout: float = DEFAULT_TIMEOUT,
        session: Optional[requests.Session] = None,
    ):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        # `Bearer key_id.secret` is one of the two credential forms the server accepts
        # (the other being Basic key_id:secret); this one survives a shell env var.
        self._session = session or requests.Session()
        self._session.headers.update({"Authorization": f"Bearer {token}"})

    # ── plumbing ─────────────────────────────────────────────────────────────

    def _request(self, method: str, path: str, **kw) -> requests.Response:
        resp = self._session.request(
            method, f"{self.base_url}{path}", timeout=self.timeout, **kw
        )
        if resp.status_code >= 400:
            # Never .json() here: failures are text/plain.
            raise_for_status(resp.status_code, resp.text)
        return resp

    def _get_json(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        clean = {k: v for k, v in (params or {}).items() if v is not None}
        return self._request("GET", path, params=clean).json()

    # ── reference ────────────────────────────────────────────────────────────

    def portfolios(self) -> "RowList[Portfolio]":
        """Portfolios this token may act on, with its own permission flags."""
        return parse_list(Portfolio, self._get_json("/portfolios"))

    def positions(self, portfolio_id: str) -> "RowList[Position]":
        """Open positions in a portfolio. Needs `can_view`."""
        return parse_list(Position, self._get_json(f"/portfolios/{portfolio_id}/positions"))

    # ── orders ───────────────────────────────────────────────────────────────

    def submit(
        self,
        *,
        portfolio: str,
        side: str,
        quantity: float,
        symbol: Optional[str] = None,
        venue: Optional[str] = None,
        instrument_id: Optional[str] = None,
        order_type: str = "market",
        time_in_force: str = "day",
        limit_price: Optional[float] = None,
        account_id: Optional[str] = None,
        client_order_id: Optional[str] = None,
        order_id: Optional[str] = None,
    ) -> str:
        """Submit an order. Returns its `order_id`.

        Name the instrument any of three ways: `instrument_id`, `symbol` + `venue`, or
        `symbol` alone as ``SYMBOL@VENUE``. A bare `symbol` works only when it is
        unique across venues — many equity tickers are not, and the server answers 422
        naming the candidates.

        `order_id` is generated when not supplied, and is the idempotency key. Because
        of that, a 409 on retry means the order was *already accepted* — so it is
        swallowed here rather than raised, making `submit` safe to retry on a timeout.
        Passing your own `order_id` opts into that same guarantee across processes.

        Note the server returns 204 with no body, so nothing but the id can be
        returned; call `order()` or `wait_for()` to see what became of it.
        """
        oid = order_id or str(uuid.uuid4())
        body: Dict[str, Any] = {
            "order_id": oid,
            "client_order_id": client_order_id or oid,
            "portfolio_id": portfolio,
            "side": side,
            "order_type": order_type,
            "time_in_force": time_in_force,
            "quantity": quantity,
        }
        if instrument_id is not None:
            body["instrument_id"] = instrument_id
        if symbol is not None:
            body["symbol"] = symbol
        if venue is not None:
            body["venue"] = venue
        if limit_price is not None:
            body["limit_price"] = limit_price
        if account_id is not None:
            body["account_id"] = account_id

        try:
            self._request("POST", "/orders/submit", json=body)
        except AlreadyExists:
            # The id is ours and the OMS already has it: the earlier attempt landed.
            pass
        return oid

    def order(self, order_id: str) -> Order:
        """Fetch one order's current state. Needs `can_view` on its portfolio."""
        return Order.from_dict(self._get_json(f"/orders/{order_id}"))

    def orders(
        self,
        *,
        status: Optional[str] = None,
        portfolio_id: Optional[str] = None,
        instrument_id: Optional[str] = None,
        side: Optional[str] = None,
        since: Optional[str] = None,
        until: Optional[str] = None,
        limit: int = 100,
        offset: int = 0,
    ) -> "RowList[BlotterRow]":
        """The blotter, bounded to portfolios this token may view.

        Scope comes from the authenticated principal's grants, so this returns every
        order in those portfolios — including ones another principal submitted. It is
        a portfolio's activity, not a personal history.

        `since`/`until` are RFC3339 timestamps; `limit` is clamped server-side to
        1..500.
        """
        return parse_list(
            BlotterRow,
            self._get_json(
                "/orders",
                {
                    "status": status,
                    "portfolio_id": portfolio_id,
                    "instrument_id": instrument_id,
                    "side": side,
                    "since": since,
                    "until": until,
                    "limit": limit,
                    "offset": offset,
                },
            ),
        )

    def cancel(self, order_id: str, reason: Optional[str] = None) -> str:
        """Cancel an order. Needs `can_trade`.

        Returns ``"canceled"`` when the OMS cancelled it outright (it had not reached a
        broker), or ``"pending"`` when the cancel was forwarded to the broker and will
        be confirmed asynchronously by the execution stream. On ``"pending"`` the order
        is not yet cancelled — poll `order()` to see it land.
        """
        body: Dict[str, Any] = {"order_id": order_id}
        if reason is not None:
            body["reason"] = reason
        resp = self._request("POST", "/orders/cancel", json=body)
        return "pending" if resp.status_code == 202 else "canceled"

    def wait_for(
        self,
        order_id: str,
        *,
        timeout: float = 60.0,
        interval: float = 0.5,
    ) -> Order:
        """Poll until the order reaches a terminal status, then return it.

        Polling because the OMS has no push channel for order events. Raises
        `TimeoutError` if it is still working when `timeout` elapses — the order is
        untouched by that, it just has not finished.
        """
        deadline = time.monotonic() + timeout
        while True:
            current = self.order(order_id)
            if current.is_terminal:
                return current
            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"order {order_id} still {current.status} after {timeout:g}s"
                )
            time.sleep(interval)

    # ── allocations ──────────────────────────────────────────────────────────

    def allocate(self, order_id: str, splits: List[Dict[str, Any]]) -> "RowList[Allocation]":
        """Split a filled block order into sub-portfolios. Needs `can_allocate`.

        `splits` is a list of ``{"portfolio_id": ..., "qty": ...}``.
        """
        payload = self._request(
            "POST", f"/orders/{order_id}/allocations", json={"splits": splits}
        ).json()
        return parse_list(Allocation, payload)

    def allocations(self, order_id: str) -> "RowList[Allocation]":
        """Existing allocations for an order."""
        return parse_list(Allocation, self._get_json(f"/orders/{order_id}/allocations"))


__all__ = ["OMS", "OMSError"]
