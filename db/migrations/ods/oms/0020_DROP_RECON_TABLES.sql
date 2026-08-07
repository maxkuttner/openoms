-- Retire custodian/position reconciliation. Verifying OMS-derived positions against
-- a broker's holdings is a book-of-record / back-office concern, not the OMS's — the
-- OMS only sees its own fills, so it can't reconcile cash, corporate actions, or
-- external activity without manufacturing false breaks. Order reconciliation
-- (OMS orders vs the broker's open orders/fills) stays; it is the OMS's own domain.
-- Nothing reads these tables after removing the recon admin endpoints.

DROP TABLE IF EXISTS recon_break;
DROP TABLE IF EXISTS recon_run;
