---
title: Troubleshooting
description: Common startup and setup errors, and what to do about them.
---

| Symptom | Cause and fix |
|---|---|
| Nobody can sign in as an admin | Lost password, authenticator and recovery codes, or no admin exists yet. On the server run `oms auth recover-admin --email <admin>` (or `oms auth bootstrap-admin --email <you>` if there is none); under Docker, `docker exec <container> oms auth …`. See [Sign-in and users](/configuration/login/#recover-an-admin-from-the-host). |
| `refusing to start` mentioning `public_base_url` or https | Sign-in cookies need HTTPS unless everything is loopback. Put TLS in front and set `[auth] public_base_url` to the `https://` address. |
| `password authentication failed for user "oms"` | The `oms` role's password doesn't match `oms.password` / `OMS_PASSWORD`. Fix the value, or reset it with `ALTER ROLE oms PASSWORD '…'`. |
| `role "oms" already exists` on init | The database is already provisioned. Use `oms database migrate`, or `oms database drop` to start over. |
| SQL errors such as `column … does not exist` or `relation … does not exist` | The database is behind the binary — the server does not migrate on its own. Run `oms database migrate`; `oms database status` shows what is pending. |
| `refusing to start: credentials are stored but none of them could be decrypted` | `master_key` doesn't match the key the credentials were stored with. See [Master key and backups](/operations/master-key-and-backups/). |
| `400 no account_id given and portfolio has no default account` | Set the portfolio's default account in the cockpit. |
| `422 instrument not found` | The catalog has no such instrument. Load the broker's instruments from **Broker connections** in the cockpit (the **Instrument universe** column). |
| A load shows ✕ Failed or ◑ Partial | Open **Jobs** in the cockpit's top bar: the job's error and log say why. ◑ Partial "interrupted" means the OMS restarted mid-load — click **Load again**. |
| The server can't connect, and the log says no config file was found | It is looking in the wrong place. The error lists every location searched — see [Config file locations](/configuration/file-locations/). |
| `/cockpit/` returns 404 on a source build | The cockpit bundle wasn't built before `cargo build`. See [Getting started → From source](/getting-started/#1-install). |
| Link error mentioning `-lssl` on macOS (source build) | `brew install openssl@3`, or set `OPENSSL_DIR`. |
