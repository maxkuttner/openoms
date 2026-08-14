"""Exceptions mapped from the OMS's HTTP responses.

The OMS returns errors as **plain text, not JSON** — `ApiError` on the Rust side is
`(StatusCode, String)`, so a failed request's body is a bare message like
``instrument not active`` or ``risk check failed [invalid_quantity]: ...``. Parsing a
failure as JSON would raise the wrong exception entirely, so nothing here touches
`.json()`.

Every exception carries `status` and `message`, so a caller that does not want to
match on types can still branch on the status code.
"""

from __future__ import annotations


class OMSError(Exception):
    """Base for every error the OMS returns. Catch this to catch them all."""

    def __init__(self, status: int, message: str):
        super().__init__(f"{status}: {message}")
        self.status = status
        self.message = message


class AuthError(OMSError):
    """401 — the token is missing, malformed, unknown, or revoked."""


class Forbidden(OMSError):
    """403 — authenticated, but lacking the grant this route requires.

    Trading needs `can_trade` on the portfolio, reading needs `can_view`, and
    allocating needs `can_allocate`.
    """


class NotFound(OMSError):
    """404 — no such order."""


class AlreadyExists(OMSError):
    """409 — an order with this `order_id` was already accepted.

    Not a failure for a retry. `order_id` is the idempotency key, so re-sending one
    the OMS already has means the first attempt landed — see `OMS.submit`, which
    treats this as success for an id it generated itself.
    """


class Rejected(OMSError):
    """422 — the order was refused before reaching a broker.

    Covers an unknown or inactive instrument, an ambiguous symbol, an option with a
    time-in-force other than `day`, a size below the broker minimum, and pre-trade
    risk. Risk rejections carry their code inline:
    ``risk check failed [invalid_quantity]: ...``.
    """


class BrokerRejected(OMSError):
    """502 — the OMS accepted the order but the broker refused it.

    The order still exists in the OMS as `submitted`: the event was committed before
    routing was attempted. Do not resubmit under a fresh id, or the position doubles
    if the first one actually landed.
    """


class Unavailable(OMSError):
    """503 — no adapter configured for the account's broker, or the DB is unreachable."""


class BadRequest(OMSError):
    """400 — malformed request: a bad UUID, or a missing/contradictory instrument reference."""


_BY_STATUS = {
    400: BadRequest,
    401: AuthError,
    403: Forbidden,
    404: NotFound,
    409: AlreadyExists,
    422: Rejected,
    502: BrokerRejected,
    503: Unavailable,
}


def raise_for_status(status: int, body: str) -> None:
    """Raise the exception matching `status`, or `OMSError` for anything unmapped."""
    cls = _BY_STATUS.get(status, OMSError)
    raise cls(status, body.strip())
