---
title: Interactive Brokers
description: Trade Interactive Brokers through the TWS API (recommended) or an experimental FIX session.
---

There are two ways to reach Interactive Brokers. The TWS API connection is the
recommended one. The [FIX session](#fix-session-experimental) is experimental.

## TWS API connection

The OMS runs IB Gateway for you in a Docker container, logs in with your IBKR
username and password, and trades through the TWS API. Instruments come from IBKR's
own contract search, so you can import what you want to trade.

### What you need

- Docker, running on the machine that runs the OMS. The OMS starts and watches the
  container; it never needs you to run IB Gateway yourself.
- An IBKR account. Start with a paper account.

### Create the connection

Create a broker connection with **Interactive Brokers (TWS API)** as the venue. Pick
the **paper** environment first. Its credentials drawer asks for your IBKR username
and password.

The OMS starts one container per connection and environment, named
`oms-ibgw-<connection>`. The API port is published on `127.0.0.1` only. After an OMS
restart it picks the running container up again instead of starting a new one, as
long as your credentials are unchanged. Saving new credentials replaces the container
and logs in again; no OMS restart is needed. If the account is logged in somewhere
else, the gateway takes the session over. The
password is handed to the container through a short-lived file and is never put on a
command line or in a log.

The connection's status shows `connecting`, `live` or `down`. While it is `down` or
`connecting`, the message beside it (hover the status) says why:

| Status | Message | Meaning |
|---|---|---|
| connecting | IB Gateway is starting | The container is up and logging in. |
| connecting | IB Gateway not reachable yet: ... | The container is still starting its API. |
| connecting | waiting for two-factor approval on the IBKR mobile app | Approve the login in the IBKR Mobile app. IBKR normally requires this for live logins, and asks again about once a week. |
| connecting | IBKR account is logged in elsewhere; the gateway is taking over the session | Another TWS, IB Gateway or IBKR web session holds the login. The gateway takes over, which logs the other session out. |
| down | IBKR login failed: ... | IBKR rejected the username or password. The OMS stops retrying, so it does not lock your account. Fix the password in the credentials drawer and save; the OMS logs in again with the new one. |
| down | Docker is required for IBKR_TWS but is not running | The OMS can't reach Docker. It keeps retrying until Docker is back. |
| down | IB Gateway is disconnected, or IB Gateway lost its connection to IBKR | The gateway dropped; the OMS reconnects. |

### Instruments

Each instrument is an IBKR contract, identified by its IBKR contract id (conId) and
listed under its listing exchange's MIC, for example `XETR` for SAP. Orders always go
out by conId.

In the cockpit, open **Broker connections** and press **Import instruments** on the IBKR_TWS
connection: choose a CSV file, check the preview, pick a contract for any ambiguous row,
then save. The same dialog has a search for single instruments.

The CSV needs a header with a `symbol` or `conid` column; `exchange` and `currency`
narrow the search, and a `conid` pins an exact contract. Leave `exchange` empty to let
IBKR route it.

```csv
symbol,exchange,currency
AAPL,,USD
SAP,XETR,EUR
VOD,XLON,GBP
```

`exchange` takes a MIC or an IBKR exchange code. The OMS knows these:

| Exchange | MIC | IBKR code |
|---|---|---|
| Nasdaq | XNAS | NASDAQ |
| NYSE | XNYS | NYSE |
| NYSE Arca | ARCX | ARCA |
| NYSE American | XASE | AMEX |
| Cboe BZX | BATS | BATS |
| Xetra | XETR | IBIS |
| Frankfurt | XFRA | FWB |
| London | XLON | LSE |
| Paris | XPAR | SBF |
| Amsterdam | XAMS | AEB |
| Swiss | XSWX | EBS |
| Milan | XMIL | BVME |
| Madrid | XMAD | BM |
| Stockholm | XSTO | SFB |
| Toronto | XTSE | TSE |
| Hong Kong | XHKG | SEHK |
| Australia | XASX | ASX |
| Tokyo | XTKS | TSEJ |

An import takes at most 1000 rows per request.

To import from a terminal instead, with the OMS running and connected:

```sh
oms setup sync-broker --symbols symbols.csv
```

This uses the paper environment unless `IBKR_TWS_ENV=LIVE` is set. It prints rows that
are ambiguous or not found; add a `conid` column to settle those. A nightly refresh at
06:00 UTC updates the imported contracts and marks any whose conId IBKR no longer knows
as untradeable.

### Prices

Prices are delayed (IBKR market data type 3). A subscription for real-time data is not
used.

### Currencies, P&L and limits

Amounts stay in each instrument's own currency; nothing is converted. Positions and
Day P&L are totalled per currency. A notional limit applies only to orders in the
limit's currency, so for a non-USD instrument without a limit in its currency only
quantity limits apply. The price band is not applied on this connection.

Commissions are logged but not yet booked into P&L.

### Known gaps

- After a long outage, fills from before the current day come back as totals, not as
  individual fills.
- A cancel while the gateway is disconnected is refused with a temporary error; it is
  never queued.
- The OMS sees only the orders it placed through its own API connection. Orders entered
  by hand in TWS or the IBKR apps are invisible to it, including to reconciliation.

## Credentials

For the TWS API connection, in the cockpit's credentials drawer:

| Field | What it is | Default |
| --- | --- | --- |
| IBKR username | Your IBKR login (for paper, the paper username from Client Portal) | |
| Password | Its password. It reaches IB Gateway only through a short-lived file, never a command line or log. | |

The FIX session's fields are listed under [FIX session](#fix-session-experimental).

## FIX session (experimental)

The OMS can run a FIX order-entry session to Interactive Brokers, using the QuickFIX
engine built into the binary. QuickFIX handles logon, heartbeats, sequence numbers,
resends, TLS and reconnects.

:::caution[Experimental]
There is no catalog sync for IBKR: `oms setup sync-broker` does not support it, so no
command creates the IBKR mappings an order needs to route. IBKR order entry works only
for instruments that already have one. Prefer the TWS API connection above.
:::

### Credentials

IBKR gives you the FIX session details when they enable FIX for your account. Create a
broker connection in the cockpit with **Interactive Brokers** as the venue; its
credentials drawer asks for:

| Field | What it is | Default |
|---|---|---|
| Password | Your FIX session password | |

and, under **FIX session**:

| Field | What it is | Default |
|---|---|---|
| Host | Your IBKR FIX gateway's host | |
| Port | Its port | |
| SenderCompID | Your side of the session, assigned by IBKR | |
| TargetCompID | IBKR's side of the session | `IBKR` |
| TLS | Encrypt the session; leave on unless IBKR says otherwise | on |

Then restart the OMS: a FIX session starts at boot and cannot be swapped while running.
