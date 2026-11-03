---
title: Running as a service
description: Keep the OMS running across logouts, crashes and reboots with systemd.
---

`oms` runs in the foreground and logs to stderr. On Linux, let systemd keep it running.
There is no installer for this yet — you write one small unit file.

## As your user

No root needed. The config is the one `oms init` wrote, in `~/.config/oms/`.

Create `~/.config/systemd/user/oms.service`:

```ini
[Unit]
Description=openoms
After=network-online.target

[Service]
WorkingDirectory=%h/.config/oms
ExecStart=%h/.local/bin/oms
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

`%h` is your home directory. `WorkingDirectory` puts the server next to its `oms.toml`
and `.env`.

Start it, and start it at every boot:

```sh
systemctl --user daemon-reload
systemctl --user enable --now oms
loginctl enable-linger "$USER"   # keep it running when you are logged out
```

Logs, and status:

```sh
journalctl --user -u oms -f
systemctl --user status oms
```

If the process dies, systemd starts it again after five seconds.

## As a system service

For a machine-wide install, run it as a dedicated user with the config in
`/etc/oms/oms.toml` ([config file locations](/openoms/configuration/file-locations/)):

```ini
[Unit]
Description=openoms
After=network-online.target postgresql.service

[Service]
User=oms
WorkingDirectory=/etc/oms
ExecStart=/usr/local/bin/oms
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

Save it as `/etc/systemd/system/oms.service`, then
`sudo systemctl daemon-reload && sudo systemctl enable --now oms`.

## Is it running?

`oms status` shows what runs on this machine and where: the OMS and the addresses
of its admin and trader UIs, then Postgres, the login provider, Kafka and each
broker stream.

```sh
oms status
```

```
config          /home/you/.config/oms/oms.toml

OMS             up              http://localhost:3001
  admin UI                      http://localhost:3001/cockpit/
  trader UI                     http://localhost:3001/trade/
Postgres        up              localhost:5432/ods
Login (OIDC)    up              http://localhost:8090/realms/oms
Kafka           not configured
Broker streams
  ALPACA        live            PAPER execution
  BINANCE       down            SPOT feed                            connection reset
```

It reads the same `oms.toml` and environment as the server, so run it as the same
user. Postgres, the login provider and Kafka are checked even when the OMS is down;
the broker streams come from the running OMS. It exits 1 when anything configured
is down, so a script or health check can use it.

## After an update

`oms update` replaces the binary but leaves the running process alone. Migrate, then
restart:

```sh
oms database migrate
systemctl --user restart oms
```

## Docker

No Docker image is published yet. Run the binary directly, as above.
