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

## After an update

`oms update` replaces the binary but leaves the running process alone. Migrate, then
restart:

```sh
oms database migrate
systemctl --user restart oms
```

## Docker

No Docker image is published yet. Run the binary directly, as above.
