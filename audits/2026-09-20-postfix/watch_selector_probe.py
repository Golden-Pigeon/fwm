#!/usr/bin/env python3
import importlib.util
import json
import pathlib
import select
import signal
import subprocess
import time

base = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("probe", base / "cli_api_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
binary = str(base.parents[1] / "target/release/fwm")

def respond(request):
    method = request["command"]["method"]
    if method == "ping":
        return {"capabilities": ["cli_ux_v5"]}
    if method == "get_config":
        return probe.CLOSE
    raise AssertionError(method)

result = []
for selector in ["missing", "web", None]:
    peer = probe.MockPeer(respond)
    process = None
    try:
        saved = '''schema_version = 3
[[servers]]
id = "server"
name = "dev"
host = "127.0.0.1"
[[forwards]]
id = "rule"
name = "web"
server_id = "server"
kind = "dynamic"
listen = "127.0.0.1:31000"
desired_state = "stopped"
'''
        (peer.directory / "config.toml").write_text(saved)
        (peer.directory / "state/applied.toml").write_text(saved)
        selection = [selector] if selector is not None else []
        process = subprocess.Popen([binary, "--config-dir", str(peer.directory), "--json", "status", *selection, "--watch"],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        snapshots = []
        deadline = time.monotonic() + 4
        while len(snapshots) < 2 and time.monotonic() < deadline and process.poll() is None:
            ready, _, _ = select.select([process.stdout], [], [], .2)
            if ready:
                line = process.stdout.readline()
                if line:
                    snapshots.append(json.loads(line))
        result.append({"selector": selector, "still_running_after_two_updates": process.poll() is None,
                       "updates": snapshots, "requests": peer.requests[:]})
    finally:
        if process is not None:
            if process.poll() is None:
                process.send_signal(signal.SIGINT)
            process.communicate(timeout=4)
        peer.close()
(base / "watch-selector-round3.json").write_text(json.dumps(result, indent=2))
print(json.dumps(result, indent=2))
