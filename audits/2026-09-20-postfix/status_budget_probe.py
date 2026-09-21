#!/usr/bin/env python3
"""A supported 512-rule batch with ordinary IDs; SSH parsing fails before I/O."""
import importlib.util
import json
import pathlib
import subprocess
import time

base = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("probe", base / "cli_api_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
binary = str(base.parents[1] / "target/release/fwm")
fixture = probe.Daemon(binary)
try:
    ssh = fixture.directory / "ssh_config"
    ssh.write_text("Host *\n " + "Unsupported" * 700 + " value\n")
    profile = probe.server()
    profile["ssh_config"] = str(ssh)
    forwards = [{"id": f"rule-{index}", "name": f"web-{index}", "server_id": "server",
                 "kind": "dynamic", "listen": f"127.0.0.1:{33000+index}", "desired_state": "running",
                 "connection_mode": "shared", "remote_cleanup": "off"} for index in range(512)]
    saved = fixture.call({"method": "create_forwards", "params": {"server": profile, "forwards": forwards}})
    assert saved["ok"], saved["error"]
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        status = fixture.call({"method": "status"})
        if not status["ok"]:
            break
        time.sleep(.02)
    one = subprocess.run([binary, "--config-dir", str(fixture.directory), "--json", "status", "web-0"],
                         capture_output=True, text=True, timeout=8)
    result = {"batch_size": len(forwards), "save_ok": saved["ok"],
              "config_json_bytes": len(json.dumps(saved["data"]["config"]).encode()),
              "status_ok": status["ok"], "status_error": status["error"],
              "single_rule_cli_exit": one.returncode,
              "single_rule_stdout": one.stdout, "single_rule_stderr": one.stderr}
finally:
    fixture.close()
(base / "status-budget-round3.json").write_text(json.dumps(result, indent=2))
print(json.dumps(result, indent=2))
