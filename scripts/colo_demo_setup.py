"""Sets up the co-located demo through the OMS admin API (see colo-demo.sh).

Usage: colo_demo_setup.py DIR KEYS BASE_URL STRATEGY
Secrets are read from files and never printed.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

DIR, KEYS, BASE, STRATEGY = sys.argv[1:5]
ADMIN = open(os.path.join(DIR, "adminpw")).read().strip()
SYMBOL = "BTCUSDT"
ASSETS = ("BTC", "USDT", "BNB")


def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("Authorization", f"Bearer {ADMIN}")
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            text = r.read().decode()
    except urllib.error.HTTPError as e:
        sys.exit(f"{method} {path} -> {e.code}: {e.read().decode()[:500]}")
    return json.loads(text) if text else None


def secret(name):
    value = open(os.path.join(KEYS, name)).read().strip()
    return value + "\n" if name.endswith(".pem") else value


def save(name, value):
    fd = os.open(os.path.join(DIR, name), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        json.dump(value, f)


def step(text):
    print(f"  {text}", flush=True)


# The OMS's own Binance testnet connection loads the instrument catalog. The
# FIX fields are Binance's defaults; the REST transport ignores them.
call("PUT", "/admin/broker-connections/binance-paper/credentials", {
    "api_key": secret("api-key"),
    "private_key": secret("ed25519-private.pem"),
    "host": "fix-oe.testnet.binance.vision",
    "port": "9000",
    "sender_comp_id": "OMS",
    "target_comp_id": "SPOT",
})
job = call("POST", "/admin/broker-connections/binance-paper/universe/load")["job_id"]
step("loading Binance's instruments…")
for _ in range(120):
    status = call("GET", f"/admin/jobs/{job}").get("status")
    if status == "succeeded":
        break
    if status in ("failed", "interrupted"):
        sys.exit("loading Binance's instruments failed; see the OMS log")
    time.sleep(1)
instrument = None
for _ in range(60):
    found = call("GET", f"/admin/instruments?search={SYMBOL}&limit=20") or []
    instrument = next((i for i in found if i.get("symbol") == SYMBOL), None)
    if instrument:
        break
    time.sleep(1)
if not instrument:
    sys.exit(f"{SYMBOL} is not in the catalog; see the OMS log")

owner = call("POST", "/admin/principals", {"code": "demo-trader", "principal_type": "HUMAN", "status": "ACTIVE", "display_name": "Demo trader"})
service = call("POST", "/admin/principals", {"code": "mm1-runtime", "principal_type": "SERVICE", "status": "ACTIVE", "display_name": "MM1 runtime"})
portfolio = call("POST", "/admin/portfolios", {"code": STRATEGY, "name": "MM1 market maker", "status": "ACTIVE", "base_currency": "USDT"})
account = call("POST", "/admin/accounts", {
    "code": "mm1-binance-testnet",
    "broker_connection_code": "binance-paper",
    "external_account_ref": "testnet-mm1",
    "status": "ACTIVE",
    "portfolio_id": portfolio["id"],
})
step("principals, portfolio and account created")

call("POST", "/admin/strategies", {
    "code": STRATEGY,
    "name": "MM1 market maker",
    "portfolio_id": portfolio["id"],
    "owner_principal_id": owner["id"],
    "principal_id": service["id"],
    "sender_comp_id": "MM1",
    "target_comp_id": "OMS",
    "mode": "COLOCATED",
    "follow_environment": "PAPER",
    "dead_man_secs": 5,
    "max_orders_per_sec": 5,
    "max_order_quantity": 0.01,
    "max_position_quantity": 0.05,
    "max_order_notional": 2000,
    "max_position_notional": 5000,
    "price_band_pct": 5,
})
call("PUT", f"/admin/strategies/{STRATEGY}/follow-credentials", {"api_key": secret("api-key"), "private_key": secret("ed25519-private.pem")})
call("PUT", f"/admin/strategies/{STRATEGY}/connections", [{"broker_code": "BINANCE", "account_id": account["id"]}])
token = call("POST", "/admin/trading-tokens", {"principal_id": service["id"], "portfolio_id": portfolio["id"], "label": "mm1 runtime link"})
save("token.json", {"token": token["token"]})
step(f"co-located strategy {STRATEGY} created with its follow key and trading token")

held = {b["asset"]: float(b["free"]) + float(b["locked"]) for b in json.load(open(os.path.join(DIR, "balances.json")))["balances"]}
deployment = call("POST", f"/admin/strategies/{STRATEGY}/deployments", {
    "account_id": account["id"],
    "venue": "BINANCE",
    "environment": "PAPER",
    "external_account_ref": "testnet-mm1",
    "authority_secs": 10,
    "market_data_max_age_secs": 5,
    "heartbeat_secs": 1,
    "allocation_quote_asset": "USDT",
    "allocation_max_notional": 5000,
    "allocation_max_position_qty": 0.05,
    "allocation_fee_allowance_bps": 10,
    "instruments": [{"instrument_id": instrument["id"], "quantity": 0, "cost_basis": 0}],
    "assets": [{"asset": a, "quantity": held.get(a, 0.0)} for a in ASSETS],
})
save("ids.json", {"deployment": deployment["id"], "account": account["id"]})
step(f"deployment {deployment['id']} registered at epoch {deployment['epoch']}")
