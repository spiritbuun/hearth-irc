#!/usr/bin/env python3
"""End-to-end: independent FUSE mounts, ordinary IRC, TLS, and server restart."""
import errno
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import unittest

from ircd import ROOT, Server, Client, certificate
from mounted import APPEND, descriptor, mounted


def wait_for(predicate, description, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.025)
    raise AssertionError(f"timed out: {description}")


class RemoteContract(unittest.TestCase):
    def setUp(self):
        self.processes = {}
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory(dir=ROOT / "target", prefix="remote-test-")))
        self.server = self.enterContext(Server(self.directory))

    def mount(self, nick, *options, ready=True):
        mount, proc, trace = self.enterContext(mounted(
            "--server", "localhost", "--port", str(self.server.port),
            "--host-label", "agent-host", "--workdir", str(self.directory), *options, nick=nick,
        ))
        self.processes[nick] = proc
        if ready:
            try:
                wait_for(lambda: "connection: ready\n" in (mount / "status").read_text(), "mount ready")
            except AssertionError:
                trace.seek(0)
                raise AssertionError(trace.read())
        return mount

    def append(self, mount, text):
        with descriptor(mount / "channels/general.log", APPEND) as fd:
            os.write(fd, (text + "\n").encode())

    def messages(self, mount):
        return (mount / "channels/general.log").read_text()

    def control(self, mount, command):
        with descriptor(mount / "control", APPEND) as fd:
            os.write(fd, (command + "\n").encode())

    def channel_ready(self, mount, channel):
        wait_for(lambda: f"channel: {channel} joined=true ready=true " in (mount / "status").read_text(), f"{channel} ready")

    def events(self, mount):
        return (mount / "events").read_text()

    def test_persistent_dm_files_actions_away_invites_and_nickname_reuse(self):
        key_a, key_b = self.directory / "alice.key", self.directory / "bob.key"
        alice = self.mount("alice", "--identity-file", str(key_a))
        bob = self.mount("bob", "--identity-file", str(key_b))
        self.append(alice, "/msg bob first direct message")
        wait_for(lambda: (bob / "users/alice.log").exists() and "first direct message" in (bob / "users/alice.log").read_text(), "DM received")
        wait_for(lambda: (alice / "users/bob.log").exists() and "first direct message" in (alice / "users/bob.log").read_text(), "DM echo and history")
        stable = next((alice / "users").glob("bob~*.log"))
        old = self.enterContext(descriptor(stable, os.O_RDONLY))
        before = os.read(old, 65536)
        self.control(bob, "NICK bobby")
        wait_for(lambda: (alice / "users/bobby.log").exists(), "DM peer nickname updated")
        stable_new = next((alice / "users").glob("bobby~*.log"))
        self.assertEqual(stable.stat().st_ino if stable.exists() else os.fstat(old).st_ino, stable_new.stat().st_ino)
        stranger = self.client("bob", caps=True)
        with descriptor(stable_new, APPEND) as fd:
            os.write(fd, b"reply to the same identity\n/me waves\n")
        wait_for(lambda: "reply to the same identity" in (bob / "users/alice.log").read_text(), "stable DM recipient")
        wait_for(lambda: "* alice waves" in (bob / "users/alice.log").read_text(), "DM action")
        self.assertEqual(os.pread(old, 65536, 0), before)
        stranger.send("PING :barrier")
        self.assertNotIn("reply to the same identity", "\n".join(stranger.until(" PONG ")))
        self.append(alice, "/me tests channel actions")
        wait_for(lambda: "* alice tests channel actions\n" in self.messages(bob), "channel action")
        self.append(bob, "/away running an experiment")
        wait_for(lambda: "away: running an experiment\n" in (bob / "status").read_text(), "away set")
        self.append(alice, "/join #invite-test")
        self.channel_ready(alice, "#invite-test")
        self.append(alice, "/invite bobby #invite-test")
        wait_for(lambda: "INVITE bobby :#invite-test" in self.events(bob), "invite received")
        self.assertFalse((bob / "channels/invite-test.log").exists())
        self.processes["bob"].terminate()
        self.processes["bob"].wait(timeout=5)
        wait_for(lambda: "bobby!agent@127.0.0.1 QUIT" in self.events(alice), "peer offline")
        with descriptor(stable_new, APPEND) as fd:
            os.write(fd, b"offline private message\n")
        wait_for(lambda: "offline private message" in stable_new.read_text(), "offline DM committed")
        restored = self.mount("restored", "--identity-file", str(key_b))
        wait_for(lambda: (restored / "users/alice.log").exists() and "offline private message" in (restored / "users/alice.log").read_text(), "DM replay with new nickname")
        self.assertEqual((restored / "users/alice.log").read_text().count("first direct message"), 1)
        self.assertNotIn(key_b.read_text().strip(), (restored / "status").read_text())

    def test_multiple_channels_replay_independently_and_rejoin_after_crash(self):
        human = self.client(caps=True)
        human.join("#research")
        for channel, text in [("#general", "first general"), ("#research", "first research"), ("#general", "second general")]:
            human.send(f"PRIVMSG {channel} :{text}")
            human.until(" PRIVMSG ")
        alice = self.mount("alice", "--channels", "#General,#Research")
        research = alice / "channels/research.log"
        self.assertIn("first research", research.read_text())
        self.assertNotIn("general", research.read_text())
        self.assertNotIn("research", self.messages(alice))
        old = self.enterContext(descriptor(research, os.O_RDONLY))
        before = research.read_bytes()
        with descriptor(research, APPEND) as fd:
            os.write(fd, b"agent research\n")
        wait_for(lambda: "alice: agent research" in research.read_text(), "second channel echo")
        self.assertEqual(os.read(old, 65536), before)
        self.server.stop(crash=True)
        wait_for(lambda: "connection: ready\n" not in (alice / "status").read_text(), "offline")
        self.server.start()
        human = self.client("human2", caps=True)
        human.join("#research")
        for channel in ("#general", "#research"):
            human.send(f"PRIVMSG {channel} :missed {channel}")
            human.until(" PRIVMSG ")
        self.channel_ready(alice, "#research")
        self.channel_ready(alice, "#general")
        wait_for(lambda: "missed #research" in research.read_text(), "research replay")
        wait_for(lambda: "missed #general" in self.messages(alice), "general replay")
        self.assertEqual(research.read_text().count("first research"), 1)
        self.assertEqual(self.messages(alice).count("first general"), 1)

    def test_dynamic_join_topics_members_part_and_rejoin(self):
        alice = self.mount("alice")
        self.control(alice, "JOIN #New-Room")
        self.channel_ready(alice, "#new-room")
        log = alice / "channels/new-room.log"
        topic = alice / "channels/new-room.topic"
        members = alice / "channels/new-room.members"
        inode = log.stat().st_ino
        self.assertEqual(topic.read_text(), "\n")
        self.assertEqual(members.read_text(), "alice\n")
        old = self.enterContext(descriptor(topic, os.O_RDONLY))
        with descriptor(topic, APPEND) as fd:
            os.write(fd, "Experiments — agent-host\n".encode())
        wait_for(lambda: topic.read_text() == "Experiments — agent-host\n", "topic confirmed")
        self.assertEqual(os.read(old, 65536), b"\n")
        human = self.client()
        joined = "\n".join(human.join("#new-room"))
        self.assertIn("Experiments — agent-host", joined)
        wait_for(lambda: "human\n" in members.read_text(), "member joins")
        human.send("NICK renamed")
        human.until(" NICK ")
        wait_for(lambda: "renamed\n" in members.read_text() and "human\n" not in members.read_text(), "member renames")
        human.send("TOPIC #new-room :human topic")
        human.until(" TOPIC ")
        wait_for(lambda: topic.read_text() == "human topic\n", "remote topic")
        with descriptor(topic, APPEND) as fd:
            os.write(fd, b"\n")
        wait_for(lambda: topic.read_text() == "\n", "clear topic")
        self.control(alice, "LIST")
        wait_for(lambda: "#new-room\t2\t" in (alice / "rooms").read_text(), "discovered channel")
        self.control(alice, "PART #new-room")
        wait_for(lambda: "channel: #new-room joined=false ready=false " in (alice / "status").read_text(), "part confirmed")
        with descriptor(log, APPEND) as fd:
            with self.assertRaises(OSError) as error:
                os.write(fd, b"parted\n")
            self.assertEqual(error.exception.errno, errno.ENOTCONN)
        human.send("PRIVMSG #new-room :while parted")
        self.control(alice, "JOIN #new-room")
        self.channel_ready(alice, "#new-room")
        wait_for(lambda: "while parted" in log.read_text(), "parted history replayed")
        self.assertEqual(log.stat().st_ino, inode)
        human.send("QUIT :done")
        wait_for(lambda: members.read_text() == "alice\n", "quit removes member")

    def test_nick_commands_services_direct_messages_and_whois(self):
        alice = self.mount("alice")
        human = self.client()
        self.append(alice, "/nick new-agent")
        wait_for(lambda: "nickname: new-agent\n" in (alice / "status").read_text(), "nickname changed")
        self.append(alice, "after rename")
        wait_for(lambda: "new-agent: after rename" in self.messages(alice), "renamed echo")
        self.assertNotIn("/nick", self.messages(alice))
        self.append(alice, "/msg NickServ HELP")
        wait_for(lambda: "NickServ!service@hearth NOTICE new-agent :Commands:" in self.events(alice), "NickServ reply")
        self.append(alice, "/ns STATUS")
        wait_for(lambda: "STATUS new-agent 0" in self.events(alice), "NickServ alias")
        self.append(alice, "/msg NickServ IDENTIFY never-log-this-secret")
        wait_for(lambda: "No accounts are required or created" in self.events(alice), "account-free reply")
        self.assertNotIn("never-log-this-secret", self.events(alice))
        self.append(alice, "/msg ChanServ REGISTER #service-room")
        self.channel_ready(alice, "#service-room")
        self.append(alice, "/cs TOPIC #service-room :service topic")
        wait_for(lambda: (alice / "channels/service-room.topic").read_text() == "service topic\n", "service topic")
        self.append(alice, "/msg human private hello")
        self.assertIn("PRIVMSG human :private hello", human.until(" PRIVMSG human ")[-1])
        human.send("PRIVMSG new-agent :private reply")
        wait_for(lambda: "PRIVMSG new-agent :private reply" in self.events(alice), "direct reply")
        self.append(alice, "/whois new-agent")
        wait_for(lambda: "Working directory (self-reported): " + str(self.directory) in self.events(alice), "WHOIS location")
        self.append(alice, "/nick human")
        wait_for(lambda: "server rejected request (433)" in (alice / "status").read_text(), "nickname collision")
        self.assertIn("connection: ready\n", (alice / "status").read_text())
        self.assertIn("nickname: new-agent\n", (alice / "status").read_text())
        self.append(alice, "//literal slash")
        wait_for(lambda: "new-agent: /literal slash\n" in self.messages(alice), "slash escape")
        self.server.stop(crash=True)
        wait_for(lambda: "connection: ready\n" not in (alice / "status").read_text(), "offline")
        self.server.start()
        self.channel_ready(alice, "#service-room")
        self.append(alice, "reconnected with new nick")
        wait_for(lambda: "new-agent: reconnected with new nick" in self.messages(alice), "nickname restored")

    def test_command_batch_validation_and_topic_rejection(self):
        alice = self.mount("alice")
        with descriptor(alice / "control", APPEND) as fd:
            with self.assertRaises(OSError) as error:
                os.write(fd, b"JOIN #must-not-exist\nPASS forbidden\n")
            self.assertEqual(error.exception.errno, errno.EINVAL)
        self.control(alice, "WHOIS missing-agent")
        wait_for(lambda: "server rejected request (401)" in (alice / "status").read_text(), "query error")
        self.assertFalse((alice / "channels/must-not-exist.log").exists())
        self.assertIn("connection: ready\n", (alice / "status").read_text())
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_topic BEFORE UPDATE ON channels BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        with descriptor(alice / "channels/general.topic", APPEND) as fd:
            os.write(fd, b"unsaved topic\n")
        wait_for(lambda: "server rejected request (400)" in (alice / "status").read_text(), "topic error")
        self.assertEqual((alice / "channels/general.topic").read_text(), "\n")
        self.append(alice, "still connected")
        wait_for(lambda: "alice: still connected" in self.messages(alice), "continued after rejection")

    def client(self, nick="human", **kwargs):
        client = self.enterContext(Client(self.server, nick, **kwargs))
        client.join()
        return client

    def test_two_mounts_and_human_exchange_without_self_duplicates(self):
        alice, bob = self.mount("alice"), self.mount("bob")
        human = self.client()
        old = self.enterContext(descriptor(bob / "channels/general.log", os.O_RDONLY))
        self.append(alice, "hello from file")
        self.assertIn("hello from file", human.until(" PRIVMSG ")[-1])
        wait_for(lambda: "alice: hello from file\n" in self.messages(bob), "message at second mount")
        human.send("PRIVMSG #general :human reply")
        self.append(bob, "bob reply")
        for mount in [alice, bob]:
            wait_for(lambda: "human: human reply\n" in self.messages(mount) and "bob: bob reply\n" in self.messages(mount), "all messages visible")
            self.assertEqual(self.messages(mount).count("alice: hello from file\n"), 1)
            self.assertEqual(self.messages(mount).count("bob: bob reply\n"), 1)
        self.assertEqual(os.read(old, 65536), b"")
        human.send("WHOIS alice")
        whois = "\n".join(human.until(" 318 "))
        self.assertIn("Reported host: agent-host", whois)
        self.assertIn(str(self.directory), whois)
        wait_for(lambda: "acknowledged_outbound: 1\n" in (alice / "status").read_text(), "server acceptance visible")

    def test_history_after_remount_and_server_crash(self):
        alice = self.mount("alice")
        self.append(alice, "before crash")
        wait_for(lambda: "alice: before crash\n" in self.messages(alice), "committed echo")
        self.server.stop(crash=True)
        wait_for(lambda: "connection: ready\n" not in (alice / "status").read_text(), "disconnect visible")
        with descriptor(alice / "channels/general.log", APPEND) as fd:
            with self.assertRaises(OSError) as error:
                os.write(fd, b"offline message\n")
            self.assertEqual(error.exception.errno, errno.ENOTCONN)
        self.server.start()
        human = self.client()
        human.send("PRIVMSG #general :while agent reconnects")
        wait_for(lambda: "connection: ready\n" in (alice / "status").read_text(), "reconnected")
        wait_for(lambda: "human: while agent reconnects\n" in self.messages(alice), "missed history replayed")
        bob = self.mount("bob")
        self.assertEqual(self.messages(alice), self.messages(bob))
        self.assertEqual(self.messages(alice).count("before crash"), 1)
        self.assertNotIn("offline message", self.messages(alice))

    def test_history_pagination_recovers_more_than_one_page(self):
        human = self.client(caps=True)
        for i in range(125):
            human.send(f"PRIVMSG #general :history {i}")
            human.until(" PRIVMSG ")
            if i > 60:
                time.sleep(0.021)
        alice = self.mount("alice")
        records = self.messages(alice).splitlines()
        self.assertEqual(len(records), 125)
        self.assertTrue(records[0].endswith("history 0"))
        self.assertTrue(records[-1].endswith("history 124"))

    def test_tls_password_and_reported_location(self):
        self.server.stop()
        cert, key = certificate(self.directory)
        secret = self.directory / "password"
        secret.write_text("remote test password\n")
        self.server.options = ("--tls-cert", str(cert), "--tls-key", str(key), "--password-file", str(secret))
        self.server.start()
        alice = self.mount("alice", "--tls", "--tls-ca", str(cert), "--password-file", str(secret))
        self.append(alice, "encrypted file message")
        wait_for(lambda: "alice: encrypted file message\n" in self.messages(alice), "TLS message committed")
        with sqlite3.connect(self.server.database) as db:
            self.assertEqual(db.execute("SELECT host_label,workdir FROM events").fetchone(), ("agent-host", str(self.directory)))
        bad = self.mount("untrusted", "--tls", "--password-file", str(secret), ready=False)
        wait_for(lambda: "reconnects: 1\n" in (bad / "status").read_text(), "untrusted certificate rejected")
        self.assertNotIn("connection: ready\n", (bad / "status").read_text())

    def test_server_rejection_is_reported_without_optimistic_log(self):
        alice = self.mount("alice")
        with sqlite3.connect(self.server.database) as db:
            db.execute("CREATE TRIGGER fail_message BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'simulated disk error'); END;")
        self.append(alice, "rejected by storage")
        wait_for(lambda: "server rejected request (400)" in (alice / "status").read_text(), "rejection reported")
        self.assertNotIn("rejected by storage", self.messages(alice))
        self.assertIn("canceled_outbound: 1\n", (alice / "status").read_text())
        self.assertIn("uncertain_outbound: 0\n", (alice / "status").read_text())

    def test_replaced_database_does_not_silently_reuse_event_ids(self):
        alice = self.mount("alice")
        self.append(alice, "old database")
        wait_for(lambda: "old database" in self.messages(alice), "first database")
        self.server.stop()
        self.server.database = self.directory / "replacement.sqlite3"
        self.server.start()
        wait_for(lambda: "database identity changed" in (alice / "status").read_text(), "database identity mismatch")
        self.assertNotIn("connection: ready\n", (alice / "status").read_text())
        self.assertIn("old database", self.messages(alice))


if __name__ == "__main__":
    subprocess.run(["cargo", "build", "--locked", "--bins"], cwd=ROOT, check=True)
    unittest.main(verbosity=2)
