"""Typed views over the OMS's JSON responses.

Dataclasses rather than raw dicts, so a typo is an AttributeError at the call site
instead of a KeyError three frames later. Every one is built with `_of`, which drops
keys it does not know about — the server can add a field without breaking a client
that has not been updated.

Ids stay strings. The OMS serializes them that way (order and portfolio ids are
UUIDs, `instrument_id` is a stringified BIGINT), and converting would mean converting
back on every request.
"""

from __future__ import annotations

from dataclasses import dataclass, fields
from typing import Any, Dict, List, Optional

# The statuses no further event can move an order out of. Mirrors
# `OrderStatus::is_terminal` in src/domain/orders/state.rs — `OMS.wait_for` stops here.
TERMINAL_STATUSES = frozenset({"filled", "rejected", "canceled", "expired"})


def _of(cls, data: Dict[str, Any]):
    """Build a dataclass from a response dict, ignoring unknown keys."""
    known = {f.name for f in fields(cls)}
    return cls(**{k: v for k, v in data.items() if k in known})


@dataclass
class Order:
    """An order's current state, from ``GET /orders/{id}``."""

    order_id: str
    client_order_id: str
    portfolio_id: str
    account_id: str
    instrument_id: str
    side: str
    order_type: str
    time_in_force: str
    original_qty: float
    leaves_qty: float
    cum_qty: float
    version: int
    status: str
    limit_price: Optional[float] = None
    avg_px: Optional[float] = None
    resume_to_status: Optional[str] = None

    @property
    def is_terminal(self) -> bool:
        return self.status in TERMINAL_STATUSES

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Order":
        return _of(cls, d)


@dataclass
class BlotterRow:
    """One row of the blotter, from ``GET /orders``.

    Denormalized for display — it carries the portfolio and instrument *codes*, which
    `Order` does not. Note it omits `client_order_id`, `limit_price`, and
    `time_in_force`; fetch the order itself when those matter.
    """

    order_id: str
    principal_id: str
    principal_code: str
    portfolio_id: str
    portfolio_code: str
    account_id: str
    broker_connection_code: str
    instrument_id: str
    side: str
    order_type: str
    status: str
    original_qty: float
    leaves_qty: float
    cum_qty: float
    created_at: str
    updated_at: str
    instrument_symbol: Optional[str] = None
    instrument_name: Optional[str] = None
    avg_px: Optional[float] = None

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "BlotterRow":
        return _of(cls, d)


@dataclass
class Portfolio:
    """A portfolio this token may act on, from ``GET /portfolios``.

    The permission flags are the caller's own grant, so a client can tell in advance
    what it may do rather than finding out from a 403.
    """

    portfolio_id: str
    code: str
    name: str
    status: str
    can_trade: bool
    can_view: bool
    can_allocate: bool
    base_currency: Optional[str] = None

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Portfolio":
        return _of(cls, d)


@dataclass
class Position:
    """A position, from ``GET /portfolios/{id}/positions``.

    The mark-derived fields are `None` when no live quote exists for the instrument —
    an unpriceable or expired contract, or a feed that is down.
    """

    portfolio_id: str
    instrument_id: str
    net_qty: float
    avg_cost: float
    realized_pnl: float
    updated_at: str
    mark: Optional[float] = None
    market_value: Optional[float] = None
    unrealized_pnl: Optional[float] = None
    mark_ts: Optional[str] = None

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Position":
        return _of(cls, d)


@dataclass
class Allocation:
    """One split of a block order into a sub-portfolio."""

    id: str
    order_id: str
    from_portfolio_id: str
    to_portfolio_id: str
    instrument_id: str
    qty: float
    price: float
    created_at: str

    @classmethod
    def from_dict(cls, d: Dict[str, Any]) -> "Allocation":
        return _of(cls, d)


class RowList(list):
    """A plain `list` that can also turn itself into a DataFrame.

    Subclassing `list` keeps indexing, iteration, and `len()` working exactly as
    expected; `to_pandas()` is the only addition, and it imports pandas lazily so the
    dependency is genuinely optional.
    """

    def to_pandas(self):
        try:
            import pandas as pd
        except ImportError as exc:  # pragma: no cover - depends on the environment
            raise ImportError(
                "to_pandas() needs pandas — install with: pip install 'oms-client[pandas]'"
            ) from exc
        from dataclasses import asdict

        return pd.DataFrame([asdict(r) for r in self])


def parse_list(cls, payload: List[Dict[str, Any]]) -> RowList:
    return RowList(cls.from_dict(d) for d in payload)
