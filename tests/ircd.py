#!/usr/bin/env python3
"""Real TCP/TLS protocol, persistence, authentication, and identity tests."""
import contextlib
import os
from pathlib import Path
import re
import socket
import sqlite3
import ssl
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
IRCD = ROOT / "target/debug/hearth-ircd"


class Server:
    def __init__(self, directory, *options, port=0):
        self.directory = Path(directory)
        self.database = self.directory / "events.sqlite3"
        self.options = options
        self.port = port
        self.proc = None

    def start(self):
        self.trace = (self.directory / "server.log").open("w+")
        self.proc = subprocess.Popen([
            str(IRCD), "--listen", f"127.0.0.1:{self.port}", "--database", str(self.database), *self.options,
        ], stdout=self.trace, stderr=self.trace)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            self.trace.seek(0)
            output = self.trace.read()
            match = re.search(r"listening on 127.0.0.1:(\d+)", output)
            if match:
                self.port = int(match[1])
                return self
            if self.proc.poll() is not None:
                raise RuntimeError(f"server failed: {output}")
            time.sleep(0.02)
        raise RuntimeError("server did not start")

    def stop(self, crash=False):
        if self.proc is not None:
            if self.proc.poll() is None:
                (self.proc.kill if crash else self.proc.terminate)()
            self.proc.wait(timeout=8)
            self.trace.close()
            self.proc = None

    def __enter__(self):
        try:
            return self.start()
        except BaseException:
            self.stop()
            raise

    def __exit__(self, *args):
        self.stop()


class Client:
    def __init__(self, server, nick=None, password=None, caps=False, context=None, hostname="localhost"):
        self.socket = socket.create_connection(("127.0.0.1", server.port), timeout=3)
        try:
            if context:
                self.socket = context.wrap_socket(self.socket, server_hostname=hostname)
            self.buffer = b""
            self.welcome = []
            if nick:
                commands = []
                if password is not None:
                    commands.append(f"PASS :{password}")
                if caps:
                    commands += ["CAP LS 302", "CAP REQ :echo-message server-time hearth-history"]
                commands += [f"NICK {nick}", f"USER agent 0 * :{nick} agent"]
                if caps:
                    commands.append("CAP END")
                self.send(*commands)
                self.welcome = self.until(" 422 ")
        except BaseException:
            self.socket.close()
            raise

    def send(self, *lines):
        self.socket.sendall(("\r\n".join(lines) + "\r\n").encode())

    def line(self):
        while b"\n" not in self.buffer:
            data = self.socket.recv(8192)
            if not data:
                raise EOFError("IRC connection closed")
            self.buffer += data
        line, self.buffer = self.buffer.split(b"\n", 1)
        return line.rstrip(b"\r").decode()

    def until(self, text):
        lines = []
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            line = self.line()
            lines.append(line)
            if line.startswith("PING"):
                self.send("PONG :hearth")
            if text in line:
                return lines
        raise AssertionError(f"did not receive {text!r}: {lines}")

    def join(self, channel="#general"):
        self.send(f"JOIN {channel}")
        return self.until(" 366 ")

    def history(self, after=0, limit=100):
        self.send(f"HEARTH HISTORY #general {after} {limit}")
        return self.until(" HEARTH END ")

    def close(self):
        self.socket.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def certificate(directory):
    cert, key = Path(directory) / "test.crt", Path(directory) / "test.key"
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        "-keyout", str(key), "-out", str(cert), "-subj", "/CN=localhost",
        "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1",
        "-addext", "basicConstraints=critical,CA:FALSE",
    ], check=True, capture_output=True, timeout=15)
    return cert, key


class IrcdContract(unittest.TestCase):
    def setUp(self):
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory(dir=ROOT / "target", prefix="ircd-test-")))
        self.server = self.enterContext(Server(self.directory))

    def client(self, nick=None, **kwargs):
        return self.enterContext(Client(self.server, nick, **kwargs))

    def test_join_creates_channel_and_topics_are_shared(self):
        alice = self.client("alice", caps=True)
        bob = self.client("bob")
        self.assertIn("CHANLIMIT=#:32 TOPICLEN=350", " ".join(alice.welcome))
        self.assertIn(" 331 alice #expbench ", "\n".join(alice.join("#ExpBench")))
        alice.send("TOPIC #EXPBENCH :Experiments on agent-host — phase 1")
        update = alice.until(" TOPIC ")[-1]
        self.assertEqual(update, ":alice!agent@127.0.0.1 TOPIC #expbench :Experiments on agent-host — phase 1")
        joined = "\n".join(bob.join("#expbench"))
        self.assertIn(" 332 bob #expbench :Experiments on agent-host — phase 1", joined)
        self.assertRegex(joined, r" 333 bob #expbench alice!agent@127\.0\.0\.1 \d+")
        bob.send("LIST")
        self.assertIn(" 322 bob #expbench 2 :Experiments on agent-host — phase 1", "\n".join(bob.until(" 323 ")))
        alice.send("JOIN #expbench", "PING :already-joined")
        self.assertFalse(any("alice!agent@" in line and " JOIN " in line for line in alice.until(" PONG ")))
        bob.send("TOPIC #expbench :")
        self.assertEqual(bob.until(" TOPIC ")[-1], ":bob!agent@127.0.0.1 TOPIC #expbench :")
        self.assertEqual(alice.until(" TOPIC ")[-1], ":bob!agent@127.0.0.1 TOPIC #expbench :")
        alice.send("TOPIC #expbench")
        alice.until(" 331 ")
        alice.send("PRIVMSG #expbench :new channel message")
        self.assertIn("new channel message", bob.until(" PRIVMSG ")[-1])
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT channel,body FROM events").fetchone(), ("#expbench", "new channel message"))

    def test_topic_permissions_and_validation(self):
        member = self.client("member")
        outsider = self.client("outsider")
        member.join("#topic-test")
        outsider.send("TOPIC #topic-test :unauthorized")
        outsider.until(" 442 ")
        outsider.send("TOPIC #absent :no such channel")
        outsider.until(" 403 ")
        for channel in ("#", "#bad/path", "#" + "x" * 32):
            member.send(f"JOIN {channel}")
            member.until(" 476 ")
        for topic in ("x" * 351, "bad\x01topic", "é" * 176):
            member.send(f"TOPIC #topic-test :{topic}")
            member.until(" 417 ")
        # The byte limit accepts complete UTF-8 without truncation.
        member.send("TOPIC #topic-test :" + "é" * 175)
        self.assertTrue(member.until(" TOPIC ")[-1].endswith("é" * 175))
        outsider.send("TOPIC #topic-test")
        self.assertIn("é" * 175, "\n".join(outsider.until(" 333 ")))
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM channels").fetchone()[0], 4)

    def test_channels_and_topics_survive_empty_membership_and_crash(self):
        alice = self.client("alice")
        alice.join("#empty")
        alice.join("#persistent")
        alice.send("TOPIC #persistent :saved topic")
        alice.until(" TOPIC ")
        alice.send("TOPIC #persistent")
        metadata = alice.until(" 333 ")[-1].split("#persistent ", 1)[1]
        alice.send("JOIN 0", "PING :parted")
        alice.until(" PONG ")
        self.server.stop(crash=True)
        self.server.options = ("--channels", "#general")
        self.server.start()
        bob = self.client("bob")
        bob.send("LIST")
        listing = "\n".join(bob.until(" 323 "))
        self.assertIn("#empty 0 :", listing)
        self.assertIn("#persistent 0 :saved topic", listing)
        self.assertIn("#research 0 :", listing)  # Removing a seed is not deletion.
        joined = "\n".join(bob.join("#persistent"))
        self.assertIn(f" 333 bob #persistent {metadata}", joined)
        bob.send("TOPIC #persistent :")
        bob.until(" TOPIC ")
        self.server.stop(crash=True)
        self.server.start()
        reader = self.client("reader")
        self.assertIn(" 331 reader #persistent ", "\n".join(reader.join("#persistent")))

    def test_channel_and_topic_storage_failures_do_not_publish(self):
        alice = self.client("alice")
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_channel BEFORE INSERT ON channels BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        alice.send("JOIN #failed")
        self.assertFalse(any(" JOIN #failed" in line for line in alice.until(" 400 ")))
        alice.send("TOPIC #failed", "PRIVMSG #failed :not a member")
        alice.until(" 403 ")
        alice.until(" 404 ")
        with sqlite3.connect(self.server.database) as db:
            self.assertIsNone(db.execute("SELECT name FROM channels WHERE name='#failed'").fetchone())
            db.execute("DROP TRIGGER fail_channel")
        alice.join("#failed")
        alice.send("TOPIC #failed :original")
        alice.until(" TOPIC ")
        bob = self.client("bob")
        bob.join("#failed")
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_topic BEFORE UPDATE ON channels BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        alice.send("TOPIC #failed :unsaved")
        self.assertFalse(any(" TOPIC #failed" in line for line in alice.until(" 400 ")))
        bob.send("TOPIC #failed")
        reply = "\n".join(bob.until(" 333 "))
        self.assertIn("#failed :original", reply)
        self.assertNotIn("unsaved", reply)
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT topic FROM channels WHERE name='#failed'").fetchone()[0], "original")

    def test_v1_database_migration_keeps_history_and_server_identity(self):
        alice = self.client("alice", caps=True)
        alice.join("#legacy")
        alice.send("PRIVMSG #legacy :old database message")
        alice.until(" PRIVMSG ")
        server_id = re.search(r"SERVERID=(\w+)", " ".join(alice.welcome))[1]
        self.server.stop()
        with sqlite3.connect(self.server.database) as db:
            db.execute("DROP TABLE channels")
            db.execute("DROP TABLE dm_peers")
            db.execute("DROP INDEX events_channel_time")
            for column in ("command", "target", "sender"):
                db.execute(f"ALTER TABLE events DROP COLUMN {column}")
            db.execute("PRAGMA user_version=1")
        self.server.options = ("--channels", "#general")
        self.server.start()
        reader = self.client("reader", caps=True)
        self.assertIn(f"SERVERID={server_id}", " ".join(reader.welcome))
        reader.send("LIST")
        self.assertIn("#legacy 0 :", "\n".join(reader.until(" 323 ")))
        reader.join("#legacy")
        reader.send("HEARTH HISTORY #legacy 0 100")
        history = "\n".join(reader.until(" HEARTH END "))
        self.assertRegex(history, r"HEARTH EVENT #legacy 1 \S+ alice :old database message")
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("PRAGMA user_version").fetchone()[0], 3)

    def history_caps(self, client):
        client.send("CAP REQ :batch message-tags server-time draft/chathistory away-notify")
        client.until(" ACK ")

    def chat_history(self, client, request):
        client.send("CHATHISTORY " + request)
        return client.until(" BATCH -")

    def test_standard_history_queries_batches_and_actions(self):
        alice = self.client("alice", caps=True)
        alice.join()
        self.history_caps(alice)
        echoes = []
        for i in range(5):
            body = f"message {i}" if i != 2 else "\x01ACTION waves\x01"
            alice.send(f"PRIVMSG #general :{body}")
            echoes.append(alice.until(" PRIVMSG ")[-1])
            time.sleep(0.004)
        ids = [re.search(r"msgid=([^; ]+)", line)[1] for line in echoes]
        timestamp = re.search(r"time=([^; ]+)", echoes[2])[1]
        latest = self.chat_history(alice, "LATEST #general * 2")
        self.assertIn(" chathistory #general", latest[0])
        self.assertIn("message 3", latest[1])
        self.assertIn("message 4", latest[2])
        self.assertIn("batch=", latest[1])
        before = self.chat_history(alice, f"BEFORE #general msgid={ids[2]} 10")
        self.assertEqual(sum(" PRIVMSG " in line for line in before), 2)
        after = self.chat_history(alice, f"AFTER #general timestamp={timestamp} 10")
        self.assertEqual(sum(" PRIVMSG " in line for line in after), 2)
        between = self.chat_history(alice, f"BETWEEN #general msgid={ids[4]} msgid={ids[0]} 2")
        self.assertIn("ACTION waves", "\n".join(between))
        self.assertIn("message 3", "\n".join(between))
        around = self.chat_history(alice, f"AROUND #general msgid={ids[2]} 3")
        self.assertEqual(sum(" PRIVMSG " in line for line in around), 3)
        self.assertIn("ACTION waves", "\n".join(around))
        targets = self.chat_history(alice, "TARGETS timestamp=2000-01-01T00:00:00.000Z timestamp=2100-01-01T00:00:00.000Z 100")
        self.assertIn("draft/chathistory-targets", targets[0])
        self.assertIn("CHATHISTORY TARGETS #general", "\n".join(targets))
        alice.join("#empty-history")
        self.assertEqual(len(self.chat_history(alice, "LATEST #empty-history * 100")), 2)
        alice.send("CHATHISTORY LATEST #unjoined * 10")
        alice.until("FAIL CHATHISTORY INVALID_TARGET")
        alice.send("CHATHISTORY AFTER #general msgid=wrong 10")
        alice.until("FAIL CHATHISTORY INVALID_PARAMS")

    def test_away_and_invites_without_operator_privileges(self):
        alice, bob = self.client("alice"), self.client("bob")
        alice.join()
        bob.join()
        self.history_caps(alice)
        bob.send("AWAY :running a long experiment")
        bob.until(" 306 ")
        self.assertIn("AWAY :running a long experiment", alice.until(" AWAY ")[-1])
        alice.send("WHOIS bob")
        self.assertIn(" 301 alice bob :running a long experiment", "\n".join(alice.until(" 318 ")))
        alice.send("WHO #general")
        self.assertIn("bob G :", "\n".join(alice.until(" 315 ")))
        alice.join("#invited")
        alice.send("INVITE bob #invited")
        alice.until(" 341 ")
        self.assertIn("INVITE bob :#invited", bob.until(" INVITE ")[-1])
        bob.join("#invited")
        alice.send("INVITE bob #invited")
        alice.until(" 443 ")
        bob.send("AWAY")
        bob.until(" 305 ")
        self.assertTrue(alice.until(" AWAY")[-1].endswith(" AWAY"))

    def test_dms_survive_restart_and_nickname_reuse_does_not_grant_history(self):
        key_a, key_b = "a" * 64, "b" * 64
        alice = self.client("alice", password="/" + key_a, caps=True)
        bob = self.client("bob", password="/" + key_b, caps=True)
        alice.send("PRIVMSG bob :private persisted")
        alice.until(" PRIVMSG ")
        bob.until(" PRIVMSG ")
        bob.send("NICK renamed")
        bob.until(" NICK ")
        alice.until(" NICK ")  # DM peers receive nickname changes without a channel.
        self.history_caps(bob)
        self.assertIn("private persisted", "\n".join(self.chat_history(bob, "LATEST alice * 100")))
        bob.close()
        alice.until(" QUIT ")
        alice.send("PRIVMSG renamed :while offline")
        alice.until(" PRIVMSG ")
        self.server.stop(crash=True)
        self.server.start()
        impostor = self.client("bob", caps=True)
        self.history_caps(impostor)
        impostor.send("CHATHISTORY LATEST alice * 100")
        impostor.until("FAIL CHATHISTORY INVALID_TARGET")
        restored = self.client("restored", password="/" + key_b, caps=True)
        self.history_caps(restored)
        records = "\n".join(self.chat_history(restored, "LATEST alice * 100"))
        self.assertIn("PRIVMSG restored :private persisted", records)
        self.assertIn("PRIVMSG restored :while offline", records)
        restored.send("HEARTH PEERS * 100")
        self.assertNotIn(key_a, "\n".join(restored.until(" PEERSEND ")))

    def test_administrator_preview_prune_backup_destroy_and_lock(self):
        admin = ROOT / "target/debug/hearth-admin"
        def run(*args):
            return subprocess.run([str(admin), "--database", str(self.server.database), *args], capture_output=True, text=True, timeout=10)
        blocked = run("stats")
        self.assertNotEqual(blocked.returncode, 0)
        self.assertIn("database is in use", blocked.stderr)
        alice = self.client("alice", caps=True)
        alice.join("#prunable")
        alice.send("PRIVMSG #prunable :delete me")
        alice.until(" PRIVMSG ")
        self.server.stop()
        with sqlite3.connect(self.server.database) as db:
            before = db.execute("SELECT value FROM metadata WHERE key='history_epoch'").fetchone()[0]
        preview = run("prune", "--before", "2100-01-01T00:00:00Z", "--channel", "#prunable")
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertIn("preview only", preview.stdout)
        backup = self.directory / "backup.sqlite3"
        self.assertEqual(run("backup", str(backup)).returncode, 0)
        applied = run("destroy-channel", "#prunable", "--apply")
        self.assertEqual(applied.returncode, 0, applied.stderr)
        self.assertEqual(run("vacuum").returncode, 0)
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM events").fetchone()[0], 0)
            self.assertIsNone(db.execute("SELECT name FROM channels WHERE name='#prunable'").fetchone())
            self.assertNotEqual(db.execute("SELECT value FROM metadata WHERE key='history_epoch'").fetchone()[0], before)
        with sqlite3.connect(backup) as db:
            self.assertEqual(db.execute("SELECT body FROM events").fetchone()[0], "delete me")

    def test_pruning_preserves_surviving_message_ids_and_high_water_mark(self):
        alice = self.client("alice", caps=True)
        alice.join()
        alice.send("PRIVMSG #general :older")
        alice.until(" PRIVMSG ")
        time.sleep(0.004)
        alice.send("PRIVMSG #general :keep")
        echo = alice.until(" PRIVMSG ")[-1]
        msgid = re.search(r"msgid=([^; ]+)", echo)[1]
        timestamp = re.search(r"time=([^; ]+)", echo)[1]
        self.server.stop()
        result = subprocess.run([str(ROOT / "target/debug/hearth-admin"), "--database", str(self.server.database), "prune", "--before", timestamp, "--apply"], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.server.start()
        reader = self.client("reader", caps=True)
        self.history_caps(reader)
        reader.join()
        history = "\n".join(self.chat_history(reader, "LATEST #general * 100"))
        self.assertNotIn(":older", history)
        self.assertIn(f"msgid={msgid}", history)
        reader.send("PRIVMSG #general :next")
        self.assertIn("hearth/id=3", reader.until(" PRIVMSG ")[-1])

    def test_dm_storage_failure_never_delivers_or_persists_partial_metadata(self):
        alice, bob = self.client("alice", caps=True), self.client("bob", caps=True)
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_peer BEFORE INSERT ON dm_peers BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        alice.send("PRIVMSG bob :must roll back")
        self.assertNotIn(" PRIVMSG bob ", "\n".join(alice.until(" 400 ")))
        bob.send("PING :barrier")
        self.assertNotIn("must roll back", "\n".join(bob.until(" PONG ")))
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM events").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM dm_peers").fetchone()[0], 0)

    def test_plain_client_join_messages_and_whois_location(self):
        alice = self.client("claude-opus-2343")
        bob = self.client("bob")
        alice.join()
        bob.join()
        alice.send("HEARTH LOCATION agent-host :/workspace/my expbench")
        alice.until(" HEARTH LOCATION ")
        bob.send("WHOIS claude-opus-2343")
        whois = "\n".join(bob.until(" 318 "))
        self.assertIn("claude-opus-2343 agent 127.0.0.1", whois)
        self.assertIn("Reported host: agent-host (self-reported)", whois)
        self.assertIn("Working directory (self-reported): /workspace/my expbench", whois)
        self.assertNotIn("192.168", whois)
        alice.send("PRIVMSG #general :hello human")
        lines = bob.until(" PRIVMSG ")
        self.assertEqual(lines[-1], ":claude-opus-2343!agent@127.0.0.1 PRIVMSG #general :hello human")
        self.assertFalse(lines[-1].startswith("@"))
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT body,host_label,workdir,peer FROM events").fetchone(), ("hello human", "agent-host", "/workspace/my expbench", "127.0.0.1"))

    def test_services_reserve_names_and_route_account_free_commands(self):
        alice = self.client("alice", caps=True)
        for nick in ("NickServ", "CHANSERV"):
            alice.send(f"NICK {nick}")
            alice.until(" 433 ")
        alice.send("PRIVMSG nIcKsErV :HELP")
        alice.until("REGISTER and IDENTIFY are unnecessary")
        alice.send("PRIVMSG NickServ :NICK renamed")
        self.assertIn(" NICK :renamed", alice.until(" NICK ")[-1])
        alice.send("PRIVMSG NickServ :REGISTER should-not-be-stored", "PRIVMSG NickServ :IDENTIFY another-secret", "PING :service-barrier")
        replies = "\n".join(alice.until(" PONG "))
        self.assertIn("No accounts are required or created", replies)
        self.assertNotIn("should-not-be-stored", replies)
        self.assertNotIn("another-secret", replies)
        alice.send("WHOIS ChanServ")
        self.assertIn("ChanServ service hearth", "\n".join(alice.until(" 318 ")))
        alice.send("PRIVMSG ChanServ :REGISTER #service-channel ignored-password")
        alice.until("no ownership or channel password was registered")
        alice.send("PRIVMSG ChanServ :SET #service-channel TOPIC service created topic")
        self.assertIn("TOPIC #service-channel :service created topic", alice.until(" TOPIC ")[-1])
        outsider = self.client("outsider")
        outsider.send("PRIVMSG ChanServ :TOPIC #service-channel forbidden")
        outsider.until(" 442 ")
        alice.send("PRIVMSG ChanServ :CLEAR #service-channel TOPIC")
        self.assertTrue(alice.until(" TOPIC ")[-1].endswith("#service-channel :"))
        alice.send("NOTICE NickServ :HELP", "PING :no-notice-loop")
        self.assertNotIn("NOTICE renamed", "\n".join(alice.until(" PONG ")))
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM events").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT topic FROM channels WHERE name='#service-channel'").fetchone()[0], "")

    def test_cap_echo_message_ids_and_recovery_after_crash(self):
        alice = self.client("alice", caps=True)
        alice.join()
        alice.send("PRIVMSG #general :persisted before echo")
        echo = alice.until(" PRIVMSG ")[-1]
        self.assertIn("hearth/id=1", echo)
        self.assertIn("@time=", echo)
        old_id = re.search(r"SERVERID=(\w+)", " ".join(alice.welcome))[1]
        self.server.stop(crash=True)
        self.server.start()
        reader = self.client("reader", caps=True)
        self.assertIn(f"SERVERID={old_id}", " ".join(reader.welcome))
        reader.join()
        history = "\n".join(reader.history())
        self.assertIn("HEARTH EVENT #general 1 ", history)
        self.assertIn("alice :persisted before echo", history)
        reader.send("PRIVMSG #general :after restart")
        self.assertIn("hearth/id=2", reader.until(" PRIVMSG ")[-1])

    def test_pass_required_and_gates_history_whois_and_nick_reservations(self):
        self.server.stop()
        secret = self.directory / "password"
        secret.write_text("test shared secret\n")
        self.server.options = ("--password-file", str(secret))
        self.server.start()
        for commands in [("NICK alice", "USER agent 0 * :Agent"), ("PASS wrong", "WHOIS alice"), ("HEARTH HISTORY #general 0 100",)]:
            with Client(self.server) as bad:
                bad.send(*commands)
                self.assertIn(" 464 ", "\n".join(bad.until(" 464 ")))
        good = self.client("alice", password="test shared secret")
        good.join()
        good.send("PASS :test shared secret")
        self.assertIn(" 462 ", "\n".join(good.until(" 462 ")))
        self.server.trace.flush()
        self.assertNotIn("test shared secret", (self.directory / "server.log").read_text())

    def test_cap_negotiation_delays_welcome_and_unknown_caps_nak(self):
        client = self.client()
        client.send("CAP LS 302", "NICK alice", "USER agent 0 * :Agent", "CAP REQ :not-a-capability", "PING :barrier")
        lines = client.until(" PONG ")
        self.assertFalse(any(" 001 " in line for line in lines))
        self.assertTrue(any(" NAK :not-a-capability" in line for line in lines))
        client.send("CAP END")
        self.assertTrue(any(" 001 " in line for line in client.until(" 422 ")))

    def test_nickname_case_collision_and_change(self):
        alice = self.client("Alice")
        bob = self.client("Bob")
        alice.join()
        bob.join()
        bob.send("NICK aLiCe")
        bob.until(" 433 ")
        alice.send("NICK Alice2")
        self.assertIn(":Alice!agent@127.0.0.1 NICK :Alice2", bob.until(" NICK "))
        bob.send("WHOIS Alice2")
        self.assertIn("Alice2 agent 127.0.0.1", "\n".join(bob.until(" 318 ")))

    def test_history_pagination_and_channel_membership(self):
        alice = self.client("alice", caps=True)
        alice.send("HEARTH HISTORY #general 0 100")
        alice.until(" 442 ")
        alice.join()
        for i in range(5):
            alice.send(f"PRIVMSG #general :event {i}")
            alice.until(" PRIVMSG ")
        first = alice.history(limit=2)
        self.assertEqual(sum(" HEARTH EVENT " in line for line in first), 2)
        self.assertTrue(first[-1].endswith("#general 2 1"))
        last = alice.history(after=2, limit=100)
        self.assertEqual(sum(" HEARTH EVENT " in line for line in last), 3)
        self.assertTrue(last[-1].endswith("#general 5 0"))
        alice.send("HEARTH HISTORY #general 999 10")
        alice.until(" 400 ")

    def test_malformed_oversize_spoofed_and_unjoined_input(self):
        alice = self.client("alice", caps=True)
        alice.send("PRIVMSG #general :not joined")
        alice.until(" 404 ")
        alice.join()
        alice.send(":bob!other@forged PRIVMSG #general :fake", "PRIVMSG #general :bad\x00value", "PRIVMSG #general :" + "x" * 351)
        for _ in range(3):
            alice.until(" 417 ")
        self.assertFalse(any(" HEARTH EVENT " in line for line in alice.history()))
        alice.socket.sendall(b"X" * 513)
        with self.assertRaises((EOFError, ConnectionResetError)):
            alice.line()

    def test_storage_failure_is_never_broadcast(self):
        alice = self.client("alice", caps=True)
        alice.join()
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_message BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        alice.send("PRIVMSG #general :must not appear")
        failure = alice.until(" 400 ")
        self.assertFalse(any(" PRIVMSG #general :must not appear" in line for line in failure))
        self.assertFalse(any(" HEARTH EVENT " in line for line in alice.history()))

    def test_tls_verifies_certificate_and_requires_password(self):
        self.server.stop()
        cert, key = certificate(self.directory)
        secret = self.directory / "password"
        secret.write_text("tls-test-password\n")
        self.server.options = ("--tls-cert", str(cert), "--tls-key", str(key), "--password-file", str(secret))
        self.server.start()
        context = ssl.create_default_context(cafile=str(cert))
        with Client(self.server, "tls-agent", password="tls-test-password", context=context) as client:
            client.join()
            client.send("PING :encrypted")
            self.assertIn("encrypted", client.until(" PONG ")[-1])
        with self.assertRaises(ssl.SSLCertVerificationError):
            Client(self.server, context=ssl.create_default_context())
        with self.assertRaises(ssl.SSLCertVerificationError):
            Client(self.server, context=context, hostname="wrong-host")

    def test_remote_listener_requires_shared_password(self):
        failed = subprocess.run([str(IRCD), "--listen", "0.0.0.0:0", "--database", str(self.directory / "other.sqlite3")], capture_output=True, timeout=5)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn(b"require --password-file", failed.stderr)


if __name__ == "__main__":
    subprocess.run(["cargo", "build", "--locked", "--bins"], cwd=ROOT, check=True)
    unittest.main(verbosity=2)
