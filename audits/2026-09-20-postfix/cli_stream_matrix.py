#!/usr/bin/env python3
"""Final stream/API rejection sweep without production changes or SSH calls."""
import importlib.util
import json
import pathlib
import select
import signal
import socket
import struct
import subprocess
import tempfile
import time
import uuid

base = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("probe", base / "cli_api_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
binary = str(base.parents[1] / "target/release/fwm")
results = []


def next_value(process, deadline=5):
    until = time.monotonic() + deadline
    while time.monotonic() < until:
        ready, _, _ = select.select([process.stdout], [], [], .1)
        if ready:
            line = process.stdout.readline()
            if line:
                return json.loads(line)
        assert process.poll() is None, process.stderr.read()
    raise TimeoutError("expected stream output")


reads = 0
phase = 0
def respond(request):
    global reads, phase
    method = request["command"]["method"]
    if method == "ping":
        return {"capabilities": ["cli_ux_v5"]}
    if method == "get_config":
        reads += 1
        phase = min(reads, 4)
        if phase == 1:
            return probe.CLOSE
        value = probe.config(revision=phase)
        if phase >= 3:
            value["forwards"][0]["name"] = "renamed"
        if phase >= 4:
            value["forwards"] = []
        return value
    if method == "status":
        value = probe.snapshot(revision=phase)
        if phase >= 3:
            value["forwards"][0]["name"] = "renamed"
        if phase >= 4:
            value["forwards"] = []
        return value
    raise AssertionError(method)

peer = probe.MockPeer(respond)
process = None
try:
    process = subprocess.Popen([binary, "--config-dir", str(peer.directory), "--json", "status", "web", "--watch"],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    values = [next_value(process) for _ in range(4)]
    assert values[0]["daemon_state"] == "unavailable"  # Previously confirmed initial-fallback case.
    assert values[1]["forwards"][0]["name"] == "web"
    assert values[2]["forwards"][0]["name"] == "renamed"
    assert values[3]["forwards"] == []
    assert process.poll() is None
    results.append({"case": "watch_recovers_and_keeps_id_then_survives_deletion", "passed": True})
finally:
    if process is not None:
        process.send_signal(signal.SIGINT)
        process.communicate(timeout=4)
    peer.close()

with tempfile.TemporaryDirectory(prefix="fwm-cs-", dir="/private/tmp") as directory:
    def cli(arguments):
        output = subprocess.run([binary, "--config-dir", directory, "--json", *arguments],
                                capture_output=True, text=True, timeout=8)
        assert output.returncode == 0, (arguments, output.stderr)
        return json.loads(output.stdout)
    cli(["add", "web", "--server", "dev", "--local", "--port", "32000", "--disabled"])
    config = cli(["config", "export"])
    old_id = config["forwards"][0]["id"]
    server_id = config["servers"][0]["id"]
    follow = subprocess.Popen([binary, "--config-dir", directory, "--json", "logs", "web", "--follow", "--tail", "1"],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        assert next_value(follow)["event"]["forward_id"] == old_id
        cli(["edit", "web", "--rename", "renamed"])
        assert next_value(follow)["forward_name"] == "renamed"
        cli(["remove", "renamed"])
        assert next_value(follow)["event"]["forward_id"] == old_id
        cli(["add", "web", "--server", "dev", "--local", "--port", "32001", "--disabled"])
        new_id = cli(["config", "export"])["forwards"][0]["id"]
        marker = {"daemon_instance_id": str(uuid.uuid4()), "server_id": server_id, "server_name": "dev",
                  "forward_name": "renamed", "group": None,
                  "event": {"sequence": 1, "timestamp_ms": int(time.time()*1000), "forward_id": old_id,
                            "server_id": server_id, "message": "late old object"}}
        with (pathlib.Path(directory)/"state/events.jsonl").open("a") as output:
            output.write(json.dumps(marker) + "\n")
        value = next_value(follow)
        assert value["event"]["message"] == "late old object"
        assert value["event"]["forward_id"] != new_id
        results.append({"case": "follow_rename_delete_and_reused_name_keeps_original_id", "passed": True})
    finally:
        follow.send_signal(signal.SIGINT)
        follow.communicate(timeout=4)
    assert cli(["daemon", "status"])["daemon_running"] is False

daemon = probe.Daemon(binary)
try:
    for value in [b"{", json.dumps({"api_version": 1, "request_id": "bad", "command": {"method": "unknown"}}).encode()]:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(3)
            connection.connect(str(daemon.socket))
            connection.sendall(struct.pack(">I", len(value)) + value)
            assert connection.recv(1) == b""
        assert daemon.call({"method": "ping"})["ok"]
    for length in [0, 1024*1024+1]:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(3)
            connection.connect(str(daemon.socket))
            connection.sendall(struct.pack(">I", length))
            assert connection.recv(1) == b""
        assert daemon.call({"method": "ping"})["ok"]
    assert daemon.call({"method": "get_config"})["data"]["revision"] == 0
    results.append({"case": "malformed_unknown_zero_and_oversized_frames_close_without_mutating_or_crashing", "passed": True})
finally:
    daemon.close()

(base/"cli-api-round4-streams.json").write_text(json.dumps(results, indent=2))
print(json.dumps({"cases": results, "new_findings": 0}, indent=2))
