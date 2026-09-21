#!/usr/bin/env python3
"""Final independent CLI/API matrix sweep; every saved forward stays disabled."""
import importlib.util
import json
import pathlib
import subprocess
import tempfile

base = pathlib.Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("probe", base / "cli_api_probe.py")
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
binary = str(base.parents[1] / "target/release/fwm")
results = []

with tempfile.TemporaryDirectory(prefix="fwm-cm-", dir="/private/tmp") as directory:
    def run(arguments, success=True):
        output = subprocess.run([binary, "--config-dir", directory, "--json", *arguments],
                                capture_output=True, text=True, timeout=10)
        stream = output.stdout if output.returncode == 0 else output.stderr
        parsed = json.loads(stream)
        assert (output.returncode == 0) == success, (arguments, output.stdout, output.stderr)
        if not success:
            assert not output.stdout, (arguments, output.stdout)
            assert parsed["ok"] is False
        results.append({"args": arguments, "exit": output.returncode, "single_json": True,
                        "error_code": (parsed.get("error") or {}).get("code")})
        return parsed

    run(["add", "web", "--server", "dev", "--local", "--port", "32000", "--disabled"])
    run(["add", "batch", "--server", "dev", "--remote", "--src", "32001-32002", "--tgt", "8080", "--disabled"])
    config = run(["config", "export"])
    remote = [rule for rule in config["forwards"] if rule["kind"] == "remote"]
    assert all(rule["remote_cleanup"] == "verified" and rule["connection_mode"] == "dedicated" for rule in remote)
    original_ids = {rule["name"]: rule["id"] for rule in config["forwards"]}
    run(["edit", "web", "--tgt", "8081"])
    run(["edit", "web", "--group", "batch"])
    assert len(run(["group", "list"])["groups"][0]["members"]) == 3
    run(["edit", "batch", "--rename", "renamed"])
    renamed = run(["config", "export"])
    assert {rule["name"]: rule["id"] for rule in renamed["forwards"]} == original_ids
    assert all(rule["group"] == "renamed" for rule in renamed["forwards"])
    run(["edit", "renamed", "--ungroup"])
    assert run(["group", "list"])["groups"] == []
    run(["edit", "web", "--group", "solo"])
    run(["edit", "web", "--rename", "simple"])
    run(["server", "edit", "dev", "--rename", "renamed-server"])
    state = run(["status", "--server", "renamed-server"])
    assert len(state["forwards"]) == 3 and state["daemon_state"] == "stopped"
    logs = run(["logs", "web", "--tail", "1000"])
    assert {event["forward_name"] for event in logs["events"]} >= {"web", "simple"}
    before = run(["config", "export"])
    for arguments in [
        ["add", "--local", "--port", "32100", "--disabled"],
        ["add", "simple", "--server", "renamed-server", "--local", "--port", "32100", "--disabled"],
        ["edit", "missing", "--tgt", "80"],
        ["status", "missing"],
        ["remove", "missing"],
        ["up", "missing"],
        ["restart", "--group", "missing"],
        ["server", "edit", "missing", "--user", "u"],
        ["server", "remove", "missing"],
        ["add", "--server", "renamed-server", "--local", "--port", "32100", "--disabled", "--timeout", "1s"],
    ]:
        run(arguments, False)
    assert run(["config", "export"]) == before
    assert run(["daemon", "status"])["daemon_running"] is False
    assert not (pathlib.Path(directory) / "state/daemon.log").exists()

fixture = probe.Daemon(binary)
try:
    request = {"method": "put_server", "params": {"server": probe.server()}}
    first = fixture.call(request, request_id="once", revision=0)
    repeated = fixture.call(request, request_id="once", revision=0)
    assert repeated == first
    different = {"method": "put_server", "params": {"server": {**probe.server(), "user": "different"}}}
    assert fixture.call(different, request_id="once", revision=0)["error"]["code"] == "request_id_conflict"
    assert fixture.call(different, revision=0)["error"]["code"] == "revision_conflict"
    status = fixture.call({"method": "status"})["data"]
    events = fixture.call({"method": "events", "params": {"after": 0}})["data"]
    assert events["next_sequence"] == events["latest_sequence"]
    assert not events["has_more"]
    results.append({"case": "api_replay_conflict_and_small_event_cursor", "passed": True,
                    "status_fields": sorted(status),
                    "event_fields": sorted(events["events"][0])})
finally:
    fixture.close()

(base / "cli-api-round3-matrix.json").write_text(json.dumps(results, indent=2))
print(json.dumps({"checks": len(results), "failed": 0, "production_files_changed": False}, indent=2))
