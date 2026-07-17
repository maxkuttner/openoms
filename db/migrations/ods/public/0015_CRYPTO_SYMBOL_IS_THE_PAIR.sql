-- A crypto instrument is a PAIR, so its symbol must name the pair.
--
-- seed_binance_crypto modelled crypto as symbol = base asset (SOL), currency =
-- quote (USDT), venue = BINANCE. That cannot represent SOL/USDT and SOL/BTC at the
-- same time: both key to (SOL, BINANCE) under instrument_symbol_venue_key, so
-- adding any second quote currency for an existing base fails the unique
-- constraint. The model silently allows exactly one quote currency per base per
-- venue, which is not a property of the market.
--
-- Name the pair, as the venues do. Binance and Bybit both call it SOLUSDT, and our
-- own xref already stores SOLUSDT as external_symbol on the BROKER and PROVIDER
-- rows -- instrument.symbol was the only place in the system calling it SOL.
-- Matches NautilusTrader's raw_symbol convention (SOLUSDT.BINANCE) and CCXT's
-- rationale for putting base/quote in the symbol: contracts that differ must have
-- distinct symbols.
--
-- currency stays USDT, so the pair remains decomposable and the quote asset is
-- still first-class for settlement/valuation. The base asset is unchanged on the
-- BROKER xref's external_native_id ('SOL'), which is what recon matches balances
-- on -- so this does not disturb reconciliation.
--
-- Idempotent: the NOT LIKE guard skips rows already carrying the quote suffix.

UPDATE instrument
SET symbol = symbol || currency,
    updated_at = now()
WHERE asset_class = 'CRYPTO'
  AND instrument_class = 'SPOT'
  AND symbol NOT LIKE '%' || currency;
