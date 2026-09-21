#!/usr/bin/env python3
"""Private Unix IPC regressions; no SSH or user service is used."""
import json
import pathlib
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest

BINARY = str(pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/fwm").resolve())
sys.argv = sys.argv[:1]
SAVED = '''schema_version = 3
[[servers]]
id = "server"
name = "dev"
host = "127.0.0.1"
[[forwards]]
id = "rule"
name = "web"
server_id = "server"
group = "apps"
kind = "dynamic"
listen = "127.0.0.1:31000"
desired_state = "stopped"
'''


def exact(stream, count):
    result = b""
    while len(result) < count:
        piece = stream.recv(count - len(result))
        if not piece:
            raise EOFError()
        result += piece
    return result


class Peer:
    def __init__(self, reply):
        self.temp = tempfile.TemporaryDirectory(prefix="fwm-pq-", dir="/tmp")
        self.path = pathlib.Path(self.temp.name)
        (self.path / "state").mkdir()
        (self.path / "config.toml").write_text(SAVED)
        (self.path / "state/applied.toml").write_text(SAVED)
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(self.path / "state/daemon.sock"))
        self.socket.listen()
        self.socket.settimeout(.1)
        self.done = threading.Event()
        self.reply = reply
        self.thread = threading.Thread(target=self.serve)
        self.thread.start()

    def serve(self):
        while not self.done.is_set():
            try:
                stream, _ = self.socket.accept()
            except (socket.timeout, OSError):
                continue
            with stream:
                try:
                    stream.settimeout(2)
                    request = json.loads(exact(stream, struct.unpack(">I", exact(stream, 4))[0]))
                    data = {"daemon_instance_id": "test", "capabilities": ["cli_ux_v5", "atomic_status_view"]} if request["command"]["method"] == "ping" else self.reply(request)
                    if data is None:
                        continue
                    response = json.dumps({"api_version": 1, "request_id": request["request_id"], "ok": True, "data": data, "error": None}).encode()
                    stream.sendall(struct.pack(">I", len(response)) + response)
                except (OSError, EOFError):
                    pass

    def close(self):
        self.done.set()
        self.thread.join(timeout=3)
        self.socket.close()
        self.temp.cleanup()

    def command(self, *args):
        return [BINARY, "--config-dir", str(self.path), "--json", "status", *args]


@unittest.skipUnless(hasattr(socket, "AF_UNIX"), "Unix IPC only")
class Queries(unittest.TestCase):
    def test_initial_watch_outage_preserves_saved_rows_and_rejects_missing_selectors(self):
        peer = Peer(lambda request: None)
        try:
            for selector in [[], ["web"], ["rule"], ["--group", "apps"], ["--server", "dev"]]:
                with self.subTest(selector=selector):
                    process = subprocess.Popen(peer.command(*selector, "--watch"), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    try:
                        ready, _, _ = select.select([process.stdout], [], [], 8)
                        self.assertTrue(ready, "watch did not report saved configuration")
                        view = json.loads(process.stdout.readline())
                        self.assertEqual(view["daemon_state"], "unavailable")
                        self.assertFalse(view["runtime_available"])
                        self.assertEqual([row["id"] for row in view["forwards"]], ["rule"])
                        self.assertEqual(view["forwards"][0]["state"], "unverified")
                    finally:
                        if process.poll() is None:
                            process.send_signal(signal.SIGINT)
                        try:
                            process.communicate(timeout=4)
                        except subprocess.TimeoutExpired:
                            process.terminate()
                            process.communicate(timeout=4)
                            self.fail("watch did not exit on Ctrl-C")
            for selector in [["missing"], ["--group", "missing"], ["--server", "missing"]]:
                with self.subTest(selector=selector):
                    result = subprocess.run(peer.command(*selector, "--watch"), capture_output=True, text=True, timeout=8)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("not_found", result.stderr + result.stdout)
        finally:
            peer.close()

    def test_real_daemon_maximum_batch_has_readable_full_and_selected_status(self):
        with tempfile.TemporaryDirectory(prefix="fwm-pb-", dir="/tmp") as directory:
            path = pathlib.Path(directory)
            ssh = path / "ssh.conf"
            ssh.write_text("Host *\n " + "Unsupported" * 700 + " value\n")
            process = subprocess.Popen([BINARY, "--config-dir", directory, "daemon", "run"],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

            def rpc(command):
                with socket.socket(socket.AF_UNIX) as stream:
                    stream.settimeout(5)
                    stream.connect(str(path / "state/daemon.sock"))
                    payload = json.dumps({"api_version": 1, "request_id": str(time.monotonic_ns()), "command": command}).encode()
                    stream.sendall(struct.pack(">I", len(payload)) + payload)
                    return json.loads(exact(stream, struct.unpack(">I", exact(stream, 4))[0]))

            try:
                deadline = time.monotonic() + 10
                while True:
                    try:
                        self.assertTrue(rpc({"method": "ping"})["ok"])
                        break
                    except (OSError, EOFError):
                        if time.monotonic() > deadline:
                            self.fail("temporary daemon failed to start")
                        time.sleep(.02)
                server = {"id": "server", "name": "dev", "host": "127.0.0.1", "port": 1,
                          "user": "fixture", "ssh_config": str(ssh)}
                forwards = [{"id": f"rule-{i}", "name": f"web-{i}", "server_id": "server",
                             "kind": "local", "listen": f"127.0.0.1:{31000+i}", "target": "localhost:8080",
                             "desired_state": "running", "connection_mode": "shared", "remote_cleanup": "off"}
                            for i in range(512)]
                saved = rpc({"method": "create_forwards", "params": {"server": server, "forwards": forwards}})
                self.assertTrue(saved["ok"], saved.get("error"))
                deadline = time.monotonic() + 5
                while True:
                    reply = rpc({"method": "status"})
                    self.assertTrue(reply["ok"], reply.get("error"))
                    statuses = reply["data"]["forwards"]
                    if all(row["state"] == "needs_attention" for row in statuses):
                        break
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.02)
                self.assertEqual(len(statuses), 512)
                self.assertIn("truncated", statuses[0]["last_error"])
                for selector in [[], ["web-0"]]:
                    result = subprocess.run([BINARY, "--config-dir", directory, "--json", "status", *selector],
                                            capture_output=True, text=True, timeout=8)
                    self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                    view = json.loads(result.stdout)
                    self.assertEqual(len(view["forwards"]), 1 if selector else 512)
                    if selector:
                        self.assertGreater(len(view["forwards"][0]["last_error"]), 7000)
                        self.assertNotIn("query this rule alone", view["forwards"][0]["last_error"])
            finally:
                try:
                    rpc({"method": "shutdown"})
                    process.wait(timeout=8)
                except (OSError, EOFError, subprocess.TimeoutExpired):
                    process.terminate()
                    process.wait(timeout=5)

    def test_mixed_configuration_and_snapshot_revision_is_rejected(self):
        peer = Peer(lambda request: {
            "config": {"schema_version": 3, "revision": 3, "servers": [], "forwards": []},
            "snapshot": {"daemon_instance_id": "test", "config_revision": 2, "forwards": []},
        })
        try:
            result = subprocess.run(peer.command(), capture_output=True, text=True, timeout=8)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("revision_conflict", result.stderr + result.stdout)
        finally:
            peer.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
