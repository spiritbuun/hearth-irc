#!/usr/bin/env python3
"""Real Linux/FUSE contract tests; fails explicitly if mounting is unavailable."""
import contextlib
import errno
import fcntl
import mmap
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest
from concurrent.futures import ThreadPoolExecutor

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/debug/hearthd"
APPEND = os.O_WRONLY | os.O_APPEND


def command(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, timeout=15, **kwargs)


@contextlib.contextmanager
def mounted(*options, nick="tester"):
    # Keep the test mount inside the workspace so agent writable-root rules can
    # allow these exact operations. Every mount gets its own daemon and identity.
    with tempfile.TemporaryDirectory(prefix="fuse-test-", dir=ROOT / "target") as tmp:
        base = Path(tmp)
        mount = base / "mnt"
        mount.mkdir()
        with (base / "callbacks.log").open("w+") as trace:
            proc = subprocess.Popen(
                [str(BINARY), str(mount), "--trace", "--nick", nick, *options],
                stdout=trace, stderr=trace,
            )
            try:
                deadline = time.monotonic() + 10
                while not os.path.ismount(mount):
                    if proc.poll() is not None or time.monotonic() > deadline:
                        trace.seek(0)
                        raise RuntimeError("FUSE mount failed:\n" + trace.read())
                    time.sleep(0.02)
                # Also waits for the daemon's FUSE_INIT handshake.
                command("cat", str(mount / "status"))
                yield mount, proc, trace
            finally:
                if os.path.ismount(mount):
                    command("fusermount3", "-u", str(mount))
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.terminate()
                    proc.wait(timeout=5)
                if os.path.ismount(mount):
                    raise RuntimeError(f"refusing to remove still-mounted test directory: {mount}")


@contextlib.contextmanager
def descriptor(path, flags):
    fd = os.open(path, flags)
    try:
        yield fd
    finally:
        os.close(fd)


class MountedContract(unittest.TestCase):
    def setUp(self):
        self.mount, self.proc, self.trace = self.enterContext(mounted())
        self.log = self.mount / "channels/general.log"

    def append(self, data):
        with descriptor(self.log, APPEND) as fd:
            self.assertEqual(os.write(fd, data), len(data))

    def messages(self):
        return [line.split("] tester: ", 1)[1] for line in self.log.read_text().splitlines()]

    def fails(self, codes, function, *args):
        with self.assertRaises(OSError) as failure:
            function(*args)
        self.assertIn(failure.exception.errno, codes)

    def test_shell_and_python_readers(self):
        listing = command("ls", "-l", str(self.log.parent)).stdout
        self.assertIn(b"general.log", listing)
        self.assertEqual(command("cat", str(self.log)).stdout, b"")
        command("sh", "-c", 'printf "first message\\n" >> "$1"', "sh", str(self.log))
        with self.log.open("a") as writer:
            writer.write("hello from Python\n")
        for i in range(40):
            self.append(f"message {i}\n".encode())
        history = self.log.read_bytes()
        self.assertEqual(command("cat", str(self.log)).stdout, history)
        self.assertEqual(command("tail", "-n", "20", str(self.log)).stdout, b"".join(history.splitlines(keepends=True)[-20:]))
        self.assertEqual(self.log.stat().st_size, len(history))
        self.assertEqual(int(command("stat", "-c", "%s", str(self.log)).stdout), len(history))
        self.assertEqual(self.messages()[:2], ["first message", "hello from Python"])

    def test_split_lines_crlf_and_utf8(self):
        with descriptor(self.log, APPEND) as fd:
            os.write(fd, b"hello ")
            self.assertEqual(self.log.read_bytes(), b"")
            os.write(fd, b"world\none\ntwo\r")
            os.write(fd, b"\n\n\r\n")
            os.write(fd, b"\xc3")
            os.write(fd, b"\xa9\n")
        self.assertEqual(self.messages(), ["hello world", "one", "two", "é"])

    def test_snapshot_seek_and_new_open(self):
        originals = [f"original {i}" for i in range(30)]
        self.append(("\n".join(originals) + "\n").encode())
        before = self.log.read_bytes()
        with descriptor(self.log, os.O_RDONLY) as old:
            self.append(b"new\n")
            with descriptor(self.log, os.O_RDONLY) as new:
                self.assertEqual(os.read(new, 65536), self.log.read_bytes())
                self.assertEqual(os.read(old, 65536), before)
                self.assertEqual(os.read(old, 10), b"")
                self.assertEqual(os.pread(old, 7, 3), before[3:10])
                os.lseek(old, 0, os.SEEK_SET)
                self.assertEqual(os.read(old, 65536), before)
                # Linux can omit fh for fstat, giving the current inode size.
                # Snapshot EOF comes from read/SEEK_END, not fstat alone.
                self.assertIn(os.fstat(old).st_size, [len(before), self.log.stat().st_size])
                self.assertEqual(os.lseek(old, 0, os.SEEK_END), len(before))
                os.lseek(old, 0, os.SEEK_SET)
                tail = command("tail", "-n", "20", stdin=old).stdout
                self.assertEqual(tail, b"".join(before.splitlines(keepends=True)[-20:]))
        self.assertEqual(self.messages(), originals + ["new"])

    def test_tail_while_history_grows(self):
        self.append(("\n".join(f"seq {i}" for i in range(30)) + "\n").encode())

        def writer():
            for i in range(30, 300):
                self.append(f"seq {i}\n".encode())
                time.sleep(0.0005)

        def reader():
            for _ in range(30):
                output = command("tail", "-n", "20", str(self.log)).stdout.decode()
                numbers = [int(line.rsplit("seq ", 1)[1]) for line in output.splitlines()]
                self.assertEqual(len(numbers), 20)
                self.assertEqual(numbers, list(range(numbers[0], numbers[0] + 20)))

        with ThreadPoolExecutor(max_workers=4) as pool:
            jobs = [pool.submit(writer), *(pool.submit(reader) for _ in range(3))]
            for job in jobs:
                job.result()

    def test_concurrent_writers_do_not_interleave(self):
        def writer(i):
            with descriptor(self.log, APPEND) as fd:
                os.write(fd, f"writer-{i}: ".encode())
                time.sleep(0.002)
                os.write(fd, b"complete\n")
        with ThreadPoolExecutor(max_workers=12) as pool:
            list(pool.map(writer, range(60)))
        self.assertCountEqual(self.messages(), [f"writer-{i}: complete" for i in range(60)])

    def test_edit_truncate_rename_and_unlink_fail(self):
        self.append(b"keep me\n")
        before = self.log.read_bytes()
        for flags in [os.O_WRONLY, os.O_RDWR, os.O_RDWR | os.O_APPEND, os.O_WRONLY | os.O_TRUNC, APPEND | os.O_TRUNC]:
            self.fails([errno.EPERM, errno.EOPNOTSUPP], os.open, self.log, flags)
        truncated = subprocess.run(["sh", "-c", ': > "$1"', "sh", str(self.log)], capture_output=True, timeout=10)
        self.assertNotEqual(truncated.returncode, 0)
        self.fails([errno.EPERM], os.truncate, self.log, 0)
        self.fails([errno.EPERM, errno.EACCES], self.log.write_text, "editor replacement\n")
        self.fails([errno.EPERM, errno.EACCES], self.log.unlink)
        self.fails([errno.EPERM, errno.EACCES], self.log.rename, self.log.with_name("renamed.log"))
        self.fails([errno.EPERM, errno.EACCES], os.link, self.log, self.log.with_name("link.log"))
        self.fails([errno.EPERM, errno.EACCES], self.log.with_name("temp.log").write_text, "editor temporary file")
        with descriptor(self.log, APPEND) as fd:
            self.fails([errno.EPERM], os.ftruncate, fd, 0)
        self.assertEqual(self.log.read_bytes(), before)

    def test_clearing_append_flag_does_not_enable_overwrite(self):
        with descriptor(self.log, APPEND) as fd:
            fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) & ~os.O_APPEND)
            self.fails([errno.EPERM], os.write, fd, b"bad\n")
        self.assertEqual(self.messages(), [])

    def test_append_after_seek_cannot_modify_history(self):
        self.append(b"first\n")
        with descriptor(self.log, APPEND) as fd:
            os.lseek(fd, 0, os.SEEK_SET)
            os.write(fd, b"second\n")
        self.assertEqual(self.messages(), ["first", "second"])

    def test_partial_close_is_reported_and_discarded(self):
        fd = os.open(self.log, APPEND)
        os.write(fd, b"incomplete")
        self.fails([errno.EINVAL], os.fsync, fd)
        self.fails([errno.EINVAL], os.close, fd)
        # FUSE_RELEASE is asynchronous relative to close's return.
        deadline = time.monotonic() + 2
        while "discarded_partial_lines: 1\n" not in (self.mount / "status").read_text():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.01)
        self.assertEqual(self.messages(), [])

    def test_duplicate_close_does_not_complete_partial(self):
        with descriptor(self.log, APPEND) as fd:
            os.write(fd, b"part")
            duplicate = os.dup(fd)
            self.fails([errno.EINVAL], os.close, duplicate)
            os.write(fd, b"ial\n")
            os.close(os.dup(fd))
        self.assertEqual(self.messages(), ["partial"])

    def test_fsync_does_not_claim_persistence(self):
        for flag in [os.O_SYNC, os.O_DSYNC]:
            self.fails([errno.EOPNOTSUPP], os.open, self.log, APPEND | flag)
        with descriptor(self.log, APPEND) as fd:
            os.write(fd, b"accepted\n")
            self.fails([errno.EOPNOTSUPP], os.fsync, fd)
        self.assertEqual(self.messages(), ["accepted"])

    def test_bad_batch_rejected_without_partial_message(self):
        with descriptor(self.log, APPEND) as fd:
            for data, code in [(b"valid\nbad\0\n", errno.EINVAL), (b"x" * 401 + b"\n", errno.EMSGSIZE), (b"\xff\n", errno.EILSEQ), (b"embedded\rCR\n", errno.EINVAL)]:
                self.fails([code], os.write, fd, data)
            os.write(fd, b"recovered\n")
        self.assertEqual(self.messages(), ["recovered"])

    def test_large_write_spanning_kernel_callbacks(self):
        lines = [f"batch {i}" for i in range(10000)]
        data = ("\n".join(lines) + "\n").encode()
        self.assertGreater(len(data), 65536)
        self.append(data)
        self.assertEqual(self.messages(), lines)

    def test_status_and_readme_are_read_only_snapshots(self):
        status = self.mount / "status"
        with descriptor(status, os.O_RDONLY) as fd:
            before = os.read(fd, 65536)
            self.append(b"update\n")
            self.assertEqual(os.pread(fd, 65536, 0), before)
        self.assertIn("messages: 1\n", status.read_text())
        for path in [status, self.mount / "README"]:
            self.fails([errno.EACCES, errno.EPERM], os.open, path, APPEND)
        self.assertIn("newline required", (self.mount / "README").read_text())

    def test_shared_mmap_rejected(self):
        self.append(b"no shared page cache\n")
        with descriptor(self.log, os.O_RDONLY) as fd:
            self.fails([errno.ENODEV, errno.EINVAL, errno.EACCES], mmap.mmap, fd, 0, mmap.MAP_SHARED, mmap.PROT_READ)

    def test_private_mmap_cannot_write_history(self):
        # Linux permits MAP_PRIVATE even with FOPEN_DIRECT_IO. We do not
        # promise snapshot semantics for this unsupported read path; private
        # writes must still never alter history or send a message.
        self.append(b"original\n")
        before = self.log.read_bytes()
        with descriptor(self.log, os.O_RDONLY) as fd:
            with mmap.mmap(fd, 0, flags=mmap.MAP_PRIVATE, prot=mmap.PROT_READ | mmap.PROT_WRITE) as private:
                private[0] = ord("!")
        self.assertEqual(self.log.read_bytes(), before)

    def test_sigterm_unmounts_cleanly(self):
        self.proc.send_signal(signal.SIGTERM)
        self.assertEqual(self.proc.wait(timeout=5), 0)
        self.assertFalse(os.path.ismount(self.mount))

    def test_sqlite_history_grows_past_previous_limit_and_keeps_snapshots(self):
        self.append(b"first\n")
        before=self.log.read_bytes()
        with descriptor(self.log, os.O_RDONLY) as old:
            block=(b"x" * 390 + b"\n") * 100
            for _ in range(230):
                self.append(block)
            self.assertGreater(self.log.stat().st_size, 8 * 1024 * 1024)
            self.assertEqual(os.read(old, 65536), before)
            with descriptor(self.log, os.O_RDONLY) as fresh:
                end=os.lseek(fresh, 0, os.SEEK_END)
                self.assertGreater(end, 8 * 1024 * 1024)
                self.assertTrue(os.pread(fresh, 10, end-10).endswith(b"x\n"))


if __name__ == "__main__":
    subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True)
    unittest.main(verbosity=2)
