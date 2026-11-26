---
title: Interactive Brokers
description: Trade Interactive Brokers through the TWS API (recommended) or an experimental FIX session.
---

There are two ways to reach Interactive Brokers. The TWS API connection is the
recommended one. The [FIX session](#fix-session-experimental) is experimental.

## TWS API connection

You run IB Gateway (or TWS), logged into your IBKR account; the OMS connects to it
and trades through the TWS API. Instruments come from IBKR's own contract search, so
you can import what you want to trade.

### What you need

- An IBKR account. Start with a paper account.
- IB Gateway or TWS, running where the OMS can reach it, logged in, with the API
  enabled. Run it any way you like:
  - the Docker image `ghcr.io/gnzsnz/ib-gateway`, with `TWS_USERID`, `TWS_PASSWORD`
    and `TRADING_MODE` (`paper` or `live`) set. It serves the API on port 4004
    (paper) or 4003 (live). When the OMS runs in Docker too (the
    [Docker Compose install](/getting-started/installation/)), put it on the same
    network and use its service name as the host;
  - IBKR's IB Gateway installer, on the OMS's machine or another one;
  - TWS, with **Enable ActiveX and Socket Clients** on (Configure > API > Settings).

  IB Gateway installed from IBKR listens on 4002 (paper) or 4001 (live) by default; TWS
  on 7497 (paper) or 7496 (live). Allow the OMS's address in the API settings
  (**Trusted IPs**) when it is not on the same machine.

### Create the connection

Create a broker connection with **Interactive Brokers (TWS API)** as the venue. Pick
the **paper** environment first. In its credentials drawer, set **Gateway address** to
the gateway's `host:port`, for example `ib-gateway:4004` or `127.0.0.1:4002`. The IBKR
login lives in your gateway, not in the OMS.

The connection's status shows `connecting`, `live`, `down` or `stopped`. The message
beside it (hover the status) says why:

| Status | Message | Meaning |
|---|---|---|
| connecting | IB Gateway connection lost; reconnecting | The socket to the gateway dropped and is being reconnected (up to two minutes). |
| down | IB Gateway at ib-gateway:4004 not reachable: ... | The gateway is not running, not logged in yet, or did not answer within 15 seconds. The OMS keeps trying. |
| down | IB Gateway lost its connection to IBKR | The gateway is up but lost IBKR (its daily restart, a network drop). The OMS resumes when it is back. |
| down | IB Gateway did not answer the heartbeat ... | The gateway stopped answering; the OMS reconnects. |
| stopped | set the Gateway address ... | The connection has no Gateway address. Set it and save. |
| stopped | port 4003 is IB Gateway's live port; a PAPER connection uses 4004 (or the reverse) | The address points at the other environment's port. Fix it and save. |
| stopped | IB Gateway at ... is logged into a live account but this connection is PAPER (or the reverse) | The gateway's trading mode does not match the connection's environment. Fix it and save the credentials again. |

- **Two-factor.** The gateway's login may need approval in the IBKR Mobile app. Until it
  is logged in, the connection shows `down` with `IB Gateway at … not reachable`.
- **Daily restart.** IBKR restarts its gateways once a day. The OMS reconnects when the
  gateway is back; nothing to do.
- **Wrong password.** Repeated failed logins can lock the account: give a gateway in
  Docker a capped restart policy, not "always", and stop it before fixing a bad login.
- **One environment per gateway.** With the Docker image's ports, a PAPER connection
  takes port 4004 and a LIVE one 4003; saving the other one is refused. On connecting,
  the OMS also checks the accounts the gateway is logged into (paper account ids start
  with `D`): a PAPER connection on a live account, or a LIVE connection on a paper one,
  is `stopped` until you fix it and save the credentials again.
- **Test.** The credentials drawer's **Test** connects briefly to the address and says
  whether the gateway answers, whether it is logged in, and to which kind of account.
  It connects as API client id 999 (998 when the connection itself uses 999); if
  another API client holds that id, the test says so.
- **Upgrading from the OMS-managed gateway.** On a host where an older OMS ran IB Gateway
  for you, remove its container (`docker rm -f oms-ibgw-<connection code, lowercase>`;
  `docker ps -a --filter name=oms-ibgw` lists it): IBKR allows one
  session per user, so the old gateway would keep logging your own gateway out.

Never expose the API port to the internet: anyone who reaches it can trade on
the account.

### Instruments

Each instrument is an IBKR contract, identified by its IBKR contract id (conId) and
listed under its listing exchange's MIC, for example `XETR` for SAP. Orders always go
out by conId.

In the Admin Panel, open **Connectivity → Broker connections** and press **Import instruments** on the IBKR_TWS
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

### Futures

CME index futures trade through the TWS API connection: **ES**, **MES** and **NQ** on
CME (`XCME`). The OMS loads their contracts from IBKR once a day and on demand with
`POST /admin/futures/sync` (admin). Each root is a row in the `future_root` table, so
adding another root means adding a row there.

**Front month.** Type the root, for example `ES`, in instrument search to get the
current front contract. The front contract switches on the roll date, which is the last
trade date minus 8 days, in Chicago time.

**Roll warnings.** When you hold a contract, the OMS raises a `ROLL_DUE` alert
`roll_warn_days` (3) days before its roll date. The cockpit shows a badge on the
position and a banner. Alerts are also published to the Kafka `oms.alerts` topic
(change it with `KAFKA_ALERTS_TOPIC`; key = portfolio id, value = the alert as JSON),
listed by `GET /alerts` and acknowledged with `POST /alerts/{id}/ack`, which needs the
right to trade the portfolio. Dismissing an alert hides the banner only: the position
keeps its Roll action, past the roll date too.

**One-click roll.** The roll dialog sends one IBKR combo (BAG) order on the calendar
spread. For a long position it sells the held contract and buys the next; for a short
position it buys the held contract back and sells the next. The legs appear as child
orders of the roll order. The spread price is the net debit per spread, buy-leg prices
minus sell-leg prices, and it can be negative. It must be a multiple of the root's
spread tick (`future_root.spread_tick`, 0.05 index points for ES, MES and NQ); the
dialog's suggested price is rounded to it. Quantities are whole contracts. A roll is
never automatic; you start it.

Limits:

- Market data is IBKR's delayed feed; live CME data needs an IBKR subscription.
- The [FIX session](#fix-session-experimental) refuses futures.
- When IBKR gives no last-trade time, the expiry instant is approximated from the
  venue's calendar.
- Options route through IBKR, but there is no option chain browsing yet.
- Perpetual futures are not supported yet.

### Currencies, P&L and limits

Amounts stay in each instrument's own currency; nothing is converted. Positions and
Day P&L are totalled per currency. A notional limit applies only to orders in the
limit's currency, so for a non-USD instrument without a limit in its currency only
quantity limits apply. The price band is not applied on this connection.

Commissions are booked as fees; see [Fees](/concepts/fees/).

### Known gaps

- After a long outage, fills from before the current day come back as totals, not as
  individual fills.
- A cancel while the gateway is disconnected is refused with a temporary error; it is
  never queued.
- Orders placed by other API clients are visible only if IB Gateway's Master API client
  ID equals the connection's API client id. Orders entered by hand in TWS are seen with
  client id 0 and adopted like any external order; see
  [Reconciliation](/concepts/reconciliation/).

## Credentials

For the TWS API connection, in the connection's credentials drawer:

| Field | What it is | Default |
| --- | --- | --- |
| Gateway address: host:port of your IB Gateway or TWS (e.g. ib-gateway:4004 paper / :4003 live, or 127.0.0.1:4002 for a local gateway) | `host:port` of your IB Gateway or TWS, e.g. `ib-gateway:4004` (paper), `ib-gateway:4003` (live) or `127.0.0.1:4002` for a local gateway. Required. | |
| API client id (0 also sees orders entered in TWS; to see other API clients' orders set IB Gateway's Master API client ID to this value) | The client id the OMS connects with. With 0 it also sees orders entered by hand in TWS. To see orders placed by other API clients, set IB Gateway's **Master API client ID** to the same value. | `0` |

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
broker connection in the Admin Panel with **Interactive Brokers** as the venue; its
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
