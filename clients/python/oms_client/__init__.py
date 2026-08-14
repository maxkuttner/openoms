"""Python client for the OMS trading API.

    from oms_client import OMS

    oms = OMS("http://localhost:3001", token=os.environ["OMS_TRADING_TOKEN"])
    oid = oms.submit(portfolio=pf, symbol="SPY260918C00770000@OPRA",
                     side="buy", quantity=1)
    print(oms.wait_for(oid).status)

Needs only a trading token (`key_id.secret`, minted by an admin via
`POST /admin/trading-tokens`). No admin credential is used anywhere in this package.
"""

from .client import OMS
from .errors import (
    AlreadyExists,
    AuthError,
    BadRequest,
    BrokerRejected,
    Forbidden,
    NotFound,
    OMSError,
    Rejected,
    Unavailable,
)
from .models import Allocation, BlotterRow, Order, Portfolio, Position

__version__ = "0.1.0"

__all__ = [
    "OMS",
    "OMSError",
    "AuthError",
    "Forbidden",
    "NotFound",
    "AlreadyExists",
    "Rejected",
    "BrokerRejected",
    "Unavailable",
    "BadRequest",
    "Order",
    "BlotterRow",
    "Portfolio",
    "Position",
    "Allocation",
]
