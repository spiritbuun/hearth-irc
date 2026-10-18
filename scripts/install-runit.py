#!/usr/bin/env python3
"""Install release binaries and a Void/runit service using an existing user's data."""
import argparse
import grp
import os
from pathlib import Path
import pwd
import shlex
import shutil
import signal
import socket
import subprocess
import time


def wait_for_irc(password, timeout=20):
    """Verify registration, not just that runit briefly spawned a process."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", 6667), timeout=1) as client:
                client.settimeout(1)
                nick = f"install-{os.getpid()}"
                authentication = f"PASS {password}\r\n" if password is not None else ""
                client.sendall((f"{authentication}NICK {nick}\r\n"
                                "USER check 0 * :Installation check\r\n").encode())
                data = b""
                while len(data) < 32768 and time.monotonic() < deadline:
                    chunk = client.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                    if f" 001 {nick} ".encode() in data:
                        client.sendall(b"QUIT :Installation check complete\r\n")
                        return
        except OSError:
            pass
        time.sleep(0.2)
    raise SystemExit("IRC registration failed; inspect ~/.local/share/hearth/service-log/current")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--user", required=True, help="unprivileged server/data owner")
    parser.add_argument("--replace-pid", type=int, help="existing manual server to stop")
    parser.add_argument("--password", action=argparse.BooleanOptionalAction, default=None,
                        help="require a password, or --no-password for loopback-only access; preserve existing setting by default")
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error("run this installer with sudo")
    account = pwd.getpwnam(args.user)
    if account.pw_uid == 0:
        parser.error("the server must run as an unprivileged user")
    home = Path(account.pw_dir)
    state = home / ".local/share/hearth"
    database = state / "history.sqlite3"
    password = home / ".config/hearth/server.pass"
    source = Path(__file__).resolve().parent.parent / "target/release"
    bundled = Path(__file__).resolve().parent.parent / "bin"
    if bundled.is_dir():
        source = bundled
    service = Path("/etc/sv/hearth-ircd")
    enabled = Path("/var/service/hearth-ircd")
    names = ("hearth-ircd", "hearth-admin")
    for name in names:
        if not os.access(source / name, os.X_OK):
            parser.error("first run: cargo build --release --locked --bins")
    if not database.is_file():
        parser.error("existing ~/.local/share/hearth/history.sqlite3 required")
    if not Path("/var/service").is_dir():
        parser.error("a running runit installation is required")
    service_user = f"{account.pw_name}:{grp.getgrgid(account.pw_gid).gr_name}"
    protected_config = "\n".join(f"{key}={shlex.quote(str(value))}" for key, value in {
        "HEARTH_USER": service_user,
        "HEARTH_STATE": state,
        "HEARTH_DATABASE": database,
        "HEARTH_PASSWORD_FILE": password,
        "HEARTH_LISTEN": "127.0.0.1:6667",
    }.items()) + "\n"
    open_config = protected_config.replace(
        f"HEARTH_PASSWORD_FILE={shlex.quote(str(password))}\n", "HEARTH_PASSWORD_FILE=''\n", 1)
    legacy_config = protected_config.replace(f"HEARTH_USER={shlex.quote(service_user)}\n",
                                   f"HEARTH_USER={account.pw_uid}:{account.pw_gid}\n", 1)
    existing_config = (service / "conf").read_text() if (service / "conf").is_file() else None
    if service.is_symlink() or (service.exists() and
            existing_config not in (protected_config, open_config, legacy_config)):
        parser.error("existing service configuration differs; preserve it and install binaries manually")
    require_password = args.password if args.password is not None else existing_config != open_config
    config = protected_config if require_password else open_config
    if require_password and not password.is_file():
        parser.error("~/.config/hearth/server.pass required; use --no-password for local access")
    if os.path.lexists(enabled) and (not enabled.is_symlink() or enabled.resolve() != service):
        parser.error("/var/service/hearth-ircd already points elsewhere")
    previous = None
    if args.replace_pid is not None:
        proc = Path(f"/proc/{args.replace_pid}")
        try:
            previous = os.pidfd_open(args.replace_pid)
            argv = (proc / "cmdline").read_bytes().rstrip(b"\0").split(b"\0")
        except (FileNotFoundError, ProcessLookupError):
            parser.error("replacement PID no longer exists; omit --replace-pid if the old server has stopped")
        expected = [b"--database", os.fsencode(database)]
        if (proc.stat().st_uid != account.pw_uid or not argv or
                Path(os.fsdecode(argv[0])).name != "hearth-ircd" or
                not any(argv[i:i + 2] == expected for i in range(len(argv) - 1))):
            parser.error("replacement PID is not this user's server for this database")

    # Validate privilege dropping before changing files or stopping a live server.
    subprocess.run(["chpst", "-u", service_user, "true"], check=True)

    Path("/usr/local/bin").mkdir(parents=True, exist_ok=True)
    for name in names:
        target = Path("/usr/local/bin") / name
        temporary = target.with_name(f".{name}.{os.getpid()}")
        shutil.copyfile(source / name, temporary)
        temporary.chmod(0o755)
        temporary.replace(target)
    service.mkdir(mode=0o755, parents=True, exist_ok=True)
    (service / "log").mkdir(mode=0o755, exist_ok=True)
    logdir = state / "service-log"
    logdir.mkdir(mode=0o700, exist_ok=True)
    os.chown(logdir, account.pw_uid, account.pw_gid)
    files = {
        "conf": (config, 0o644),
        "run": ('''#!/bin/sh
set -eu
exec 2>&1
. ./conf
umask 077
cd "$HEARTH_STATE"
set -- --listen "$HEARTH_LISTEN" --database "$HEARTH_DATABASE"
if [ -n "${HEARTH_PASSWORD_FILE:-}" ]; then
  set -- "$@" --password-file "$HEARTH_PASSWORD_FILE"
fi
exec chpst -u "$HEARTH_USER" /usr/local/bin/hearth-ircd \\
  "$@"
''', 0o755),
        "log/run": ('''#!/bin/sh
set -eu
. ../conf
umask 077
exec chpst -u "$HEARTH_USER" svlogd -tt "$HEARTH_STATE/service-log"
''', 0o755),
    }
    for name, (contents, mode) in files.items():
        target = service / name
        temporary = target.with_name(target.name + ".new")
        temporary.write_text(contents)
        temporary.chmod(mode)
        temporary.replace(target)
    if previous is not None:
        signal.pidfd_send_signal(previous, signal.SIGTERM)
        os.close(previous)
    if not enabled.exists():
        enabled.symlink_to(service)
    for _ in range(100):
        if all((service / path).exists() for path in
               ("supervise/control", "log/supervise/control")):
            break
        time.sleep(0.1)
    subprocess.run(["sv", "-w", "15", "restart", str(enabled / "log")], check=True)
    subprocess.run(["sv", "-w", "15", "restart", str(enabled)], check=True)
    wait_for_irc(password.read_text().strip() if require_password else None)
    subprocess.run(["sv", "status", str(enabled)], check=True)
    print("Installed hearth-ircd and hearth-admin in /usr/local/bin.")
    print("Service enabled at boot; configuration:", service / "conf")
    print("Logs:", logdir / "current")


if __name__ == "__main__":
    main()
