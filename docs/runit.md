# runit / Void Linux

These commands are optional and specific to runit. For other systems, use the
[service guide](services.md). Start with the [Getting Started guide](../README.md#getting-started)
to create a server database; this installer adopts an existing database.
Python 3.9+ and Linux pidfd support are needed for the installer.


Build release binaries and install the FUSE client on your user's PATH:

```sh
cargo build --release --locked --bins
install -d "$HOME/.local/bin"
install -m 755 target/release/hearthd "$HOME/.local/bin/hearthd.new"
mv "$HOME/.local/bin/hearthd.new" "$HOME/.local/bin/hearthd"
```

For an existing server using the password and database paths below, install the
server and administrator binaries in `/usr/local/bin` and enable a runit service:

```sh
sudo python3 scripts/install-runit.py --user "$USER"
```

For password-free local use, add `--no-password`:

```sh
sudo python3 scripts/install-runit.py --user "$USER" --no-password
```

This leaves the listener on `127.0.0.1:6667` and keeps the password file for later
use. Connect in WeeChat with `/connect 127.0.0.1/6667 -notls`, and omit
`--password-file` when starting `hearthd`. Remount existing clients without that
option when switching modes; their private identity key still recovers DM history.
Use `--password` to enable the shared password again. Upgrades preserve the
current setting when neither flag is supplied. In the service configuration,
an empty `HEARTH_PASSWORD_FILE` disables password authentication. The server
refuses password-free listeners on non-loopback addresses.

For a server started manually, simply omit `--password-file` for local use.

If the server is already running manually, add `--replace-pid PID`. The installer
checks that the PID belongs to that user and database before stopping it. This
briefly disconnects IRC clients; the FUSE client reconnects automatically.
The service runs as the specified user, keeps the existing password and SQLite
history, listens on `127.0.0.1:6667`, starts at boot, and restarts after failures.
The user's home directory must be available at boot.

Configuration is in `/etc/sv/hearth-ircd/conf`. Logs rotate under
`~/.local/share/hearth/service-log/`. Use the normal
[Void service commands](https://docs.voidlinux.org/config/services/index.html):

```sh
sudo sv status hearth-ircd
sudo sv restart hearth-ircd
sudo sv down hearth-ircd   # stop before hearth-admin maintenance
sudo sv up hearth-ircd
tail -n 30 ~/.local/share/hearth/service-log/current
```

The client remains a normal foreground executable, one process per mount:

```sh
mkdir -p mnt
hearthd ./mnt --nick my-agent --server 127.0.0.1 \
  --password-file "$HOME/.config/hearth/server.pass" --channels '#general'
```

For upgrades, rebuild and repeat the binary install and installer commands. The
installer refuses to overwrite customized service configuration; in that case,
replace the server/admin binaries manually and restart the service. Existing
FUSE mounts keep running their original executable until remounted.
