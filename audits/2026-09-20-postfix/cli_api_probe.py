#!/usr/bin/env python3
"""Read-only production-code audit using temporary profiles and private IPC.

No real SSH server, saved user profile, or login service is used. The only
running forward intentionally fails while parsing its private SSH config.
"""
import argparse
import json
import pathlib
import socket
import struct
import subprocess
import tempfile
import threading
import time
import uuid

CLOSE = object()


def receive(sock):
    def exact(count):
        result = b""
        while len(result) < count:
            part = sock.recv(count - len(result))
            if not part:
                raise EOFError("peer closed the framed response")
            result += part
        return result
    return json.loads(exact(struct.unpack(">I", exact(4))[0]))


def rpc(path, command, request_id=None, revision=None):
    payload = {"api_version": 1, "request_id": str(uuid.uuid4()) if request_id is None else request_id,
               "command": command}
    if revision is not None:
        payload["expected_revision"] = revision
    encoded = json.dumps(payload, separators=(",", ":")).encode()
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(4)
        connection.connect(str(path))
        connection.sendall(struct.pack(">I", len(encoded)) + encoded)
        return receive(connection)


class Daemon:
    def __init__(self, binary):
        self.tmp = tempfile.TemporaryDirectory(prefix="fwm-ca-", dir="/private/tmp")
        self.directory = pathlib.Path(self.tmp.name)
        self.socket = self.directory / "state/daemon.sock"
        self.process = subprocess.Popen([binary, "--config-dir", str(self.directory), "daemon", "run"],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                if self.call({"method": "ping"})["ok"]:
                    return
            except (OSError, EOFError):
                time.sleep(.02)
        raise RuntimeError("temporary daemon did not become ready")

    def call(self, command, **options):
        return rpc(self.socket, command, **options)

    def close(self):
        try:
            self.call({"method": "shutdown"})
            self.process.wait(timeout=5)
        except (OSError, EOFError, subprocess.TimeoutExpired):
            self.process.terminate()
            self.process.wait(timeout=5)
        self.tmp.cleanup()


def server(profile_id="server"):
    return {"id": profile_id, "name": "dev", "host": "127.0.0.1", "port": 1, "user": "fixture"}


def request_ids(binary):
    fixture = Daemon(binary)
    try:
        rows = []
        for request_id in ["", "r" * 129]:
            for command in [{"method": "ping"}, {"method": "doctor", "params": {"server": None}}]:
                reply = fixture.call(command, request_id=request_id)
                rows.append({"method": command["method"], "id_length": len(request_id),
                             "ok": reply["ok"], "error": reply["error"]})
        return rows
    finally:
        fixture.close()


def metadata_budgets(binary):
    fixture = Daemon(binary)
    try:
        saved = fixture.call({"method": "put_server", "params": {"server": server("s" * 10000)}})
        output = subprocess.run([binary, "--config-dir", str(fixture.directory), "--json", "logs", "--server", "dev"],
                                capture_output=True, text=True, timeout=5)
        logs = json.loads(output.stdout)
        events = fixture.call({"method": "events", "params": {"after": 0}})["data"]
        result = {"server_id_length": 10000, "save_ok": saved["ok"],
                  "retained_server_logs": len(logs["events"]), "log_warnings": logs["warnings"],
                  "live_server_events": len([event for event in events["events"] if event.get("server_id")])}
    finally:
        fixture.close()
    fixture = Daemon(binary)
    try:
        ssh = fixture.directory / "ssh_config"
        ssh.write_text("Host *\n " + "Unsupported" * 700 + " value\n")
        profile = server()
        profile["ssh_config"] = str(ssh)
        assert fixture.call({"method": "put_server", "params": {"server": profile}})["ok"]
        forward = {"id": "f" * 260500, "name": "web", "server_id": profile["id"],
                   "kind": "local", "listen": "127.0.0.1:31000", "target": "localhost:8080",
                   "desired_state": "running", "connection_mode": "shared", "remote_cleanup": "off"}
        saved = fixture.call({"method": "put_forward", "params": {"forward": forward}})
        result["large_forward_save_ok"] = saved["ok"]
        if not saved["ok"]:
            result["large_forward_error"] = saved["error"]
            return result
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            statuses = fixture.call({"method": "status"})["data"]["forwards"]
            if statuses and statuses[0]["state"] == "needs_attention":
                break
            time.sleep(.02)
        time.sleep(.1)  # Let the independent event journal consume that state event.
        backlog = fixture.call({"method": "events", "params": {"after": 0}})["data"]
        latest = backlog["latest_sequence"]
        last = fixture.call({"method": "events", "params": {"after": latest - 1}})["data"]
        result.update({"forward_id_length": len(forward["id"]), "runtime_state": statuses[0]["state"],
                       "runtime_error_length": len(statuses[0]["last_error"] or ""),
                       "events_latest": latest, "last_event_count": len(last["events"]),
                       "next_sequence": last["next_sequence"], "has_more": last["has_more"],
                       "resync_required": last["resync_required"]})
        return result
    finally:
        fixture.close()


class MockPeer:
    def __init__(self, responder):
        self.tmp = tempfile.TemporaryDirectory(prefix="fwm-cp-", dir="/private/tmp")
        self.directory = pathlib.Path(self.tmp.name)
        (self.directory / "state").mkdir()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.directory / "state/daemon.sock"))
        self.listener.listen()
        self.listener.settimeout(.1)
        self.stop = threading.Event()
        self.requests = []
        def serve():
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                with connection:
                    connection.settimeout(5)
                    try:
                        request = receive(connection)
                        self.requests.append(request["command"]["method"])
                        data = responder(request)
                        if data is CLOSE:
                            continue
                        response = {"api_version": 1, "request_id": request["request_id"],
                                    "ok": True, "error": None, "data": data}
                        data = json.dumps(response).encode()
                        connection.sendall(struct.pack(">I", len(data)) + data)
                    except (OSError, EOFError):
                        pass
        self.thread = threading.Thread(target=serve)
        self.thread.start()

    def close(self):
        self.stop.set()
        self.thread.join(timeout=2)
        self.listener.close()
        self.tmp.cleanup()


def config(group="old", revision=1):
    return {"schema_version": 3, "revision": revision, "servers": [server()],
            "forwards": [{"id": "rule", "name": "web", "group": group, "server_id": "server",
                          "kind": "local", "listen": "127.0.0.1:31000", "target": "localhost:8080",
                          "desired_state": "running", "connection_mode": "shared", "remote_cleanup": "off"}]}


def snapshot(group="old", revision=2):
    return {"daemon_instance_id": "mock", "config_revision": revision,
            "forwards": [{"id": "rule", "name": "web", "group": group, "server": "dev", "kind": "local",
                          "listen": "127.0.0.1:31000", "target": "localhost:8080", "desired_state": "running",
                          "state": "established", "retry_count": 0, "next_retry_unix_ms": None,
                          "last_error": None, "active_connections": 0}]}


def revision_race(binary):
    reads = 0
    def respond(request):
        nonlocal reads
        method = request["command"]["method"]
        if method == "ping":
            return {"capabilities": ["cli_ux_v5"]}
        if method == "get_config":
            reads += 1
            return config("old", 1) if reads == 1 else config("new", 3)
        if method == "status":
            return snapshot("old", 2)
        raise AssertionError(method)
    peer = MockPeer(respond)
    try:
        output = subprocess.run([binary, "--config-dir", str(peer.directory), "--json", "status", "--group", "new"],
                                capture_output=True, text=True, timeout=8)
        return {"exit": output.returncode, "requests": peer.requests,
                "stdout": json.loads(output.stdout) if output.stdout else None, "stderr": output.stderr}
    finally:
        peer.close()


def huge_timeout(binary):
    def respond(request):
        method = request["command"]["method"]
        if method == "ping":
            return {"capabilities": ["cli_ux_v5"]}
        if method == "get_config":
            return config()
        if method == "set_desired":
            return {"revision": 2, "message": "saved", "config": config(revision=2)}
        if method == "status":
            return snapshot()
        raise AssertionError(method)
    peer = MockPeer(respond)
    try:
        output = subprocess.run([binary, "--config-dir", str(peer.directory), "--json", "up", "web",
                                 "--timeout", "18446744073709551615ms"], capture_output=True, text=True, timeout=8)
        return {"exit": output.returncode, "requests": peer.requests,
                "stdout": output.stdout[:2000], "stderr": output.stderr[:2000]}
    finally:
        peer.close()


if __name__ == "__main__":
    arguments = argparse.ArgumentParser()
    arguments.add_argument("--binary", default="target/release/fwm")
    arguments.add_argument("--output", default="audits/2026-09-20-postfix/cli-api-round1.json")
    args = arguments.parse_args()
    binary = str(pathlib.Path(args.binary).resolve())
    findings = {"request_ids": request_ids(binary), "metadata_budgets": metadata_budgets(binary),
                "revision_race": revision_race(binary), "huge_timeout": huge_timeout(binary)}
    pathlib.Path(args.output).write_text(json.dumps(findings, ensure_ascii=False, indent=2))
    print(json.dumps(findings, ensure_ascii=False, indent=2))
