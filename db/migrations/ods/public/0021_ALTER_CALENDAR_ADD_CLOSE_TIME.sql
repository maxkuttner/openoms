-- The local time a venue's contracts stop trading on their expiry date. Together
-- with `calendar.timezone` this turns `instrument_derivative.expiry_date` — a bare
-- venue-local DATE — into a real UTC instant (`instrument_derivative.expires_at`),
-- computed by the OMS expiry job (src/expiry.rs).
--
-- Nullable on purpose. A timezone alone cannot produce an instant, and a venue with
-- no close (crypto, 24/7) has no honest value here. NULL leaves `expires_at` NULL,
-- which preflight names out loud rather than guessing an hour and sweeping a live
-- contract.

ALTER TABLE calendar ADD COLUMN close_time TIME;
