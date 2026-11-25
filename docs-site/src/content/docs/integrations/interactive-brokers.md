---
title: Interactive Brokers
description: Trade Interactive Brokers through the TWS API (recommended) or an experimental FIX session.
---

There are two ways to reach Interactive Brokers. The TWS API connection is the
recommended one. The [FIX session](#fix-session-experimental) is experimental.

## TWS API connection

The OMS runs IB Gateway for you in a Docker container, logs in with your IBKR
username and password, and trades through the TWS API. Instruments come from IBKR's
own contract search, so you can import what you want to trade. When the OMS itself
runs in a container, run IB Gateway as its own service instead; see
[IBKR in Docker](#ibkr-in-docker).

### What you need

- Docker, running on the machine that runs the OMS. The OMS starts and watches the
  container; it never needs you to run IB Gateway yourself. Or an IB Gateway you run
  yourself, see [IBKR in Docker](#ibkr-in-docker).
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
| connecting | IB Gateway at ib-gateway:4004 not reachable: ... | Only with a [Gateway address](#ibkr-in-docker): the gateway service is not up or still logging in (or did not answer within 15 seconds). The OMS keeps trying. |
| down | IB Gateway at ... is logged into a live account but this connection is PAPER (or the reverse) | Only with a Gateway address: the gateway's `TRADING_MODE` does not match the connection's environment. The OMS stops retrying; fix it and save the credentials again. |

### IBKR in Docker

When the OMS runs in a container (the [Docker Compose install](/getting-started/installation/),
or any container platform), it cannot start IB Gateway: there is no Docker inside its
container. Run IB Gateway yourself, however you like, and give the connection its address.

1. Run IB Gateway (or TWS) where the OMS can reach it, logged into your IBKR account,
   with the API enabled. One option is the image the OMS itself uses,
   `ghcr.io/gnzsnz/ib-gateway`, as a service on the same private network as the OMS
   with `TWS_USERID`, `TWS_PASSWORD` and `TRADING_MODE` (`paper` or `live`) set; it
   serves the API on port 4004 (paper) or 4003 (live).
2. In the admin UI, open the IBKR TWS connection's credentials and set **Gateway
   address** to `host:port`, e.g. `ib-gateway:4004`. Leave username and password empty:
   the login lives in your gateway, not in the OMS.

- **Two-factor.** The gateway's login may need approval in the IBKR Mobile app. Until it
  is logged in, the connection shows `connecting` with `IB Gateway at … not reachable`.
- **Daily restart.** IBKR restarts its gateways once a day. The OMS reconnects when the
  gateway is back; nothing to do.
- **Wrong password.** Repeated failed logins can lock the account: give your gateway a
  capped restart policy, not "always", and stop it before fixing a bad login.
- **One environment per gateway.** A PAPER connection takes port 4004 and a LIVE one
  4003 when you use the ports above; saving the other one is refused. On connecting,
  the OMS also checks the accounts the gateway is logged into (paper account ids start
  with `D`): a PAPER connection on a live account, or a LIVE connection on a paper one,
  goes `down` with `IB Gateway at … is logged into a live account but this connection
  is PAPER` (or the reverse) and stops retrying until you fix it and save the
  credentials again.
- **Switching an existing connection to your own gateway.** On a host where the OMS ran
  IB Gateway itself, its old container keeps running. Remove it by hand:
  `docker rm -f oms-ibgw-<connection>` (`docker ps` lists it).

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
| IBKR username | Your IBKR login (for paper, the paper username from Client Portal). Leave empty with a Gateway address. | |
| Password | Its password. It reaches IB Gateway only through a short-lived file, never a command line or log. Leave empty with a Gateway address. | |
| Gateway address | `host:port` of an IB Gateway you run yourself, e.g. `ib-gateway:4004` (paper) or `ib-gateway:4003` (live); see [IBKR in Docker](#ibkr-in-docker). Set, the OMS never starts a container. Clear it to go back to the OMS-run gateway, which needs the username and password again. | empty |
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
