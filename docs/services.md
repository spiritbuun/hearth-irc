# Running the server as a daemon

`hearth-ircd` runs in the foreground, logs to stderr, and shuts down on SIGTERM.
Any Linux supervisor can manage it. FUSE clients are separate processes and
should run as the agent's normal user.

Stop any manually started server before enabling a service on the same address
or database. Keep the same SQLite path to retain history. The database lock
prevents two servers from sharing it simultaneously.

## systemd user service

After installing with `sh scripts/install.sh --prefix "$HOME/.local"`:

```sh
install -d -m 700 "$HOME/.local/share/hearth" "$HOME/.config/systemd/user"
install -m 644 "$HOME/.local/share/hearth/deploy/systemd/hearth-ircd.service" "$HOME/.config/systemd/user/hearth-ircd.service"
systemctl --user daemon-reload
systemctl --user enable --now hearth-ircd
systemctl --user status hearth-ircd
journalctl --user -u hearth-ircd -f
```

The example listens on `127.0.0.1:6667`, needs no password, and stores history in
`~/.local/share/hearth/history.sqlite3`. It restarts on failure. The binary path
is `~/.local/bin/hearth-ircd`; edit `ExecStart` if you installed elsewhere.

To start the user service at boot and keep it running after logout:

```sh
sudo loginctl enable-linger "$USER"
```

This requires a systemd user manager; the home directory must be available at
boot. See [systemd service options](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html)
and [lingering](https://www.freedesktop.org/software/systemd/man/latest/loginctl.html).

Edit the installed unit to change the listener, add `--password-file`, or add TLS
options from the [remote access guide](../README.md#remote-access). Then run
`systemctl --user daemon-reload` and `systemctl --user restart hearth-ircd`.
The installer does not overwrite your installed unit.

For maintenance, stop with `systemctl --user stop hearth-ircd`, run
`hearth-admin --database "$HOME/.local/share/hearth/history.sqlite3" ...`, then
start with `systemctl --user start hearth-ircd`. To disable automatic startup,
use `systemctl --user disable --now hearth-ircd`.

## runit / Void Linux

See [the runit guide](runit.md) for the installer used on Void.

## Other service managers

Configure your supervisor to run the following foreground command as an
unprivileged user, with an existing writable data directory:

```sh
/usr/local/bin/hearth-ircd --listen 127.0.0.1:6667 --database /path/to/history.sqlite3
```

Use an absolute executable/database path, capture stderr, and enable automatic
restart. No PID file or double-forking wrapper is needed. Neither the server nor
the administrator tool needs a FUSE mount.
