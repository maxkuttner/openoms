---
title: Troubleshooting
description: Common startup and setup errors, and what to do about them.
---

| Symptom | Cause and fix |
|---|---|
| `refusing to start: OMS_ADMIN_PASSWORD is not set` | The bind address is not loopback and no cockpit password is configured. Set `admin_password` in `oms.toml` (or `OMS_ADMIN_PASSWORD`). |
| `password authentication failed for user "oms"` | The `oms` role's password doesn't match `oms.password` / `OMS_PASSWORD`. Fix the value, or reset it with `ALTER ROLE oms PASSWORD '…'`. |
| `role "oms" already exists` on init | The database is already provisioned. Use `oms database migrate`, or `oms database drop` to start over. |
| The server exits naming a migration | The database is behind the binary. Run `oms database migrate`. |
| `refusing to start: credentials are stored but none of them could be decrypted` | `master_key` doesn't match the key the credentials were stored with. See [Master key and backups](/openoms/operations/master-key-and-backups/). |
| `400 no account_id given and portfolio has no default account` | Set the portfolio's default account — see [your first paper trade](/openoms/getting-started/first-paper-trade/#4-set-up-a-trader). |
| `422 instrument not found` | The catalog has no such instrument. Load it with `oms setup sync-broker`. |
| The server can't connect, and the log says no config file was found | It is looking in the wrong place. The error lists every location searched — see [Config file locations](/openoms/configuration/file-locations/). |
| `/cockpit/` returns 404 on a source build | The cockpit bundle wasn't built before `cargo build`. See [Build from source](/openoms/getting-started/installation/#build-from-source). |
| Link error mentioning `-lssl` on macOS (source build) | `brew install openssl@3`, or set `OPENSSL_DIR`. |
