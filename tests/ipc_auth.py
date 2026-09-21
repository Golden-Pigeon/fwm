#!/usr/bin/env python3
"""Linux cross-UID IPC regressions, isolated from real accounts and services.

Run with sudo after building fwm. Numeric fixture identities have no passwd/group
entries or running processes. Only fixture children lose privileges; no accounts
are created, and every fwm command has an explicit temporary configuration.
"""
import grp
import json
import os
from pathlib import Path
import pwd
import secrets
import select
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/fwm").resolve()
sys.argv = sys.argv[:1]

FAKE_DAEMON = r'''
import json, select, socket, struct, sys
received = 0
accepted = 0

def exact(stream, count):
    global received
    result = b""
    while len(result) < count:
        piece = stream.recv(count - len(result))
        received += len(piece)
        if not piece:
            raise EOFError()
        result += piece
    return result

with socket.socket(socket.AF_UNIX) as listener:
    listener.bind(sys.argv[1])
    listener.listen()
    print("ready", flush=True)
    while True:
        ready, _, _ = select.select([listener, sys.stdin], [], [], 10)
        if not ready or sys.stdin in ready:
            break
        stream, _ = listener.accept()
        accepted += 1
        with stream:
            stream.settimeout(2)
            try:
                length = struct.unpack(">I", exact(stream, 4))[0]
                if length > 1024 * 1024:
                    raise ValueError("oversized fixture request")
                request = json.loads(exact(stream, length))
                reply = json.dumps({"api_version": 1, "request_id": request["request_id"],
                                    "ok": True, "error": None,
                                    "data": {"daemon_instance_id": "counterfeit",
                                             "capabilities": ["cli_ux_v5"]}}).encode()
                stream.sendall(struct.pack(">I", len(reply)) + reply)
            except (EOFError, OSError):
                pass
print(json.dumps({"received": received, "accepted": accepted}), flush=True)
'''

FOREIGN_CLIENT = r'''
import json, socket, struct, sys
with socket.socket(socket.AF_UNIX) as stream:
    stream.settimeout(3)
    stream.connect(sys.argv[1])
    request = json.dumps({"api_version": 1, "request_id": "foreign",
                          "command": {"method": "shutdown"}}).encode()
    try:
        stream.sendall(struct.pack(">I", len(request)) + request)
        reply = stream.recv(4096)
    except (BrokenPipeError, ConnectionResetError):
        reply = b""
    print(json.dumps({"connected": True, "reply_bytes": len(reply)}))
'''


def identity(uid):
    return {"user": uid, "group": uid, "extra_groups": [], "umask": 0o077}


def unused_ids():
    used = {entry.pw_uid for entry in pwd.getpwall()}
    used.update(entry.gr_gid for entry in grp.getgrall())
    for status in Path("/proc").glob("[0-9]*/status"):
        try:
            for line in status.read_text().splitlines():
                if line.startswith(("Uid:", "Gid:")):
                    used.update(map(int, line.split()[1:]))
        except (FileNotFoundError, ProcessLookupError):
            pass
    while True:
        candidate = 1_000_000 + secrets.randbelow(1_000_000)
        if candidate in used or os.path.lexists(f"/tmp/fwm-{candidate}"):
            continue
        # Some directory services support lookups but omit account enumeration.
        try:
            pwd.getpwuid(candidate)
            continue
        except KeyError:
            pass
        try:
            grp.getgrgid(candidate)
            continue
        except KeyError:
            pass
        used.add(candidate)
        yield candidate


def endpoint(config, uid):
    direct = config / "state/daemon.sock"
    if len(os.fsencode(direct)) < 104:
        return direct
    digest = 0xcbf29ce484222325
    for byte in os.fsencode(config):
        digest = ((digest ^ byte) * 0x100000001b3) & ((1 << 64) - 1)
    return Path(f"/tmp/fwm-{uid}/{digest:016x}.sock")


class FakeDaemon:
    def __init__(self, path, uid, directory):
        self.result = None
        self.process = subprocess.Popen(
            [sys.executable, "-c", FAKE_DAEMON, str(path)], cwd=directory,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, **identity(uid))
        try:
            readable, _, _ = select.select([self.process.stdout], [], [], 5)
            if not readable or self.process.stdout.readline().strip() != "ready":
                raise AssertionError("counterfeit daemon failed to start")
        except BaseException:
            stop(self.process)
            raise

    def close(self):
        if self.result is None:
            try:
                output, error = self.process.communicate(input="\n", timeout=5)
            except BaseException:
                stop(self.process)
                raise
            if self.process.returncode != 0:
                raise AssertionError(error)
            self.result = json.loads(output)
        return self.result


def stop(process):
    if process.poll() is None:
        process.terminate()
    try:
        process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate(timeout=5)


@unittest.skipUnless(sys.platform.startswith("linux") and os.geteuid() == 0,
                     "requires Linux and root to run children as separate fixture UIDs")
class IpcAuthentication(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="fwm-ia-", dir="/tmp"))
        self.addCleanup(shutil.rmtree, self.root)
        self.root.chmod(0o755)
        self.binary = self.root / "fwm"
        shutil.copyfile(BINARY, self.binary)
        self.binary.chmod(0o755)
        candidates = unused_ids()
        while True:
            self.victim = next(candidates)
            self.runtime = Path(f"/tmp/fwm-{self.victim}")
            try:
                self.runtime.mkdir(mode=0o700)
                break
            except FileExistsError:
                continue
        # This exact directory was created exclusively above; never clean up an
        # existing user's runtime directory, even if an identity collides.
        self.addCleanup(shutil.rmtree, self.runtime)
        self.attacker = next(candidates)

    def profile(self, name):
        config = self.root / name
        config.mkdir(mode=0o700)
        (config / "state").mkdir(mode=0o700)
        for path in [config, config / "state"]:
            os.chown(path, self.victim, self.victim)
        return config

    def cli(self, config, *command):
        return subprocess.run(
            [str(self.binary), "--config-dir", str(config), "--json", *command],
            cwd=self.root, capture_output=True, text=True, timeout=5,
            **identity(self.victim))

    def fake(self, trusted_metadata=False):
        config = self.profile("long-profile-" * 12)
        address = endpoint(config, self.victim)
        self.assertEqual(address.parent, self.runtime)
        os.chown(self.runtime, self.attacker, self.attacker)
        self.runtime.chmod(0o755)
        peer = FakeDaemon(address, self.attacker, self.root)
        self.addCleanup(peer.close)
        # Both forms are connectable by the victim. The second deliberately
        # makes filesystem ownership pass while retaining the real foreign peer.
        address.chmod(0o666)
        if trusted_metadata:
            os.chown(self.runtime, self.victim, self.victim)
            os.chown(address, self.victim, self.victim)
            address.chmod(0o600)
        return config, peer

    def assert_counterfeit_rejected(self, config, peer):
        result = self.cli(config, "daemon", "status")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("ipc_authentication_failed", result.stdout + result.stderr)
        observed = peer.close()
        self.assertEqual(observed["received"], 0, observed)
        return observed

    def test_foreign_precreated_short_socket_directory_cannot_impersonate_daemon(self):
        config, peer = self.fake()
        observed = self.assert_counterfeit_rejected(config, peer)
        self.assertEqual(observed["accepted"], 0, observed)

    def test_owned_path_does_not_substitute_for_authenticating_connected_peer(self):
        config, peer = self.fake(trusted_metadata=True)
        observed = self.assert_counterfeit_rejected(config, peer)
        self.assertGreaterEqual(observed["accepted"], 1, observed)

    def test_foreign_shutdown_is_rejected_and_same_user_ping_still_works(self):
        config = self.profile("live")
        address = endpoint(config, self.victim)
        log = self.root / "daemon.log"
        with log.open("w") as output:
            daemon = subprocess.Popen(
                [str(self.binary), "--config-dir", str(config), "daemon", "run"],
                cwd=self.root, stdout=output, stderr=output, **identity(self.victim))
        self.addCleanup(stop, daemon)
        deadline = time.monotonic() + 10
        while True:
            result = self.cli(config, "daemon", "status")
            if result.returncode == 0 and json.loads(result.stdout)["daemon_running"]:
                break
            self.assertIsNone(daemon.poll(), log.read_text())
            self.assertLess(time.monotonic(), deadline, result.stdout + result.stderr)
            time.sleep(0.02)
        for path in [config, config / "state"]:
            path.chmod(0o755)
        address.chmod(0o666)
        result = subprocess.run(
            [sys.executable, "-c", FOREIGN_CLIENT, str(address)], cwd=self.root,
            capture_output=True, text=True, timeout=5, **identity(self.attacker))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(json.loads(result.stdout), {"connected": True, "reply_bytes": 0})
        result = self.cli(config, "daemon", "status")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIs(json.loads(result.stdout)["daemon_running"], True)
        self.assertIsNone(daemon.poll(), log.read_text())


if __name__ == "__main__":
    unittest.main(verbosity=2)
