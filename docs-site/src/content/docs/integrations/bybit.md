---
title: Bybit
description: Bybit's public spot book as a backup price source for crypto.
---

Bybit is a **price source only** — the OMS does not route orders to Bybit.

It subscribes to Bybit's public spot top-of-book (no account, no key) and uses it to mark
crypto positions when Binance's feed is down. It ranks below Binance because Binance is
where orders execute; Bybit's price for the same pair is a close proxy, not the same book.

There is nothing to configure: the feed starts with the server and covers the crypto
instruments you hold.
