#!/usr/bin/env python3
"""Read-only local CLI review; no daemon, IPC peer, or SSH subprocesses."""
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
BINARY = ROOT / "target/debug/fwm"
RESULTS = []

CONFIG = '''schema_version = 3
revision = 7
[[servers]]
id = "server-current"
name = "dev"
host = "example.invalid"
[[forwards]]
id = "rule-current"
name = "web"
group = "batch"
server_id = "server-current"
kind = "dynamic"
listen = "127.0.0.1:34001"
desired_state = "stopped"
connection_mode = "shared"
remote_cleanup = "off"
'''


def entry(seq, rule_id, name, group, server_id="server-current", server_name="dev"):
    return {
        "daemon_instance_id": "fixture",
        "event": {"sequence": seq, "timestamp_ms": 1234 + seq,
                  "forward_id": rule_id, "server_id": server_id,
                  "message": f"fixture-{seq}"},
        "forward_name": name, "server_id": server_id,
        "server_name": server_name, "group": group,
    }


with tempfile.TemporaryDirectory(prefix="fwm-resume-cli-", dir="/private/tmp") as temporary:
    directory = Path(temporary)
    (directory / "state").mkdir()
    (directory / "config.toml").write_text(CONFIG)
    records = [
        entry(1, "rule-retired", "web", "old", "server-retired", "dev"),
        entry(2, "rule-current", "earlier-web", "old"),
        entry(3, "rule-current", "web", "batch"),
        entry(4, "rule-moved", "moved", "batch"),
        entry(5, "rule-moved", "moved", "other"),
        entry(6, None, None, None),
    ]
    (directory / "state/events.jsonl").write_text("".join(json.dumps(value) + "\n" for value in records))
    before = {str(path.relative_to(directory)): path.read_bytes()
              for path in directory.rglob("*") if path.is_file()}

    def run(case, arguments, expected_exit=0, expected_sequences=None, expected_names=None):
        output = subprocess.run([str(BINARY), "--config-dir", str(directory), "--json", *arguments],
                                text=True, capture_output=True, timeout=8)
        assert output.returncode == expected_exit, (case, output.returncode, output.stderr)
        value = json.loads(output.stdout if expected_exit == 0 else output.stderr)
        result = {"case": case, "args": arguments, "exit": output.returncode}
        if expected_sequences is not None:
            actual = [record["event"]["sequence"] for record in value["events"]]
            assert actual == expected_sequences, (case, actual, expected_sequences)
            result["sequences"] = actual
        if expected_names is not None:
            actual = [record["name"] for record in value["forwards"]]
            assert actual == expected_names, (case, actual, expected_names)
            result["names"] = actual
            assert value["daemon_state"] == "stopped"
            assert value["runtime_available"] is False
        if expected_exit:
            result["error_code"] = value["error"]["code"]
        result["passed"] = True
        RESULTS.append(result)

    run("current_rule_name_over_reused_historical_name", ["logs", "web"], expected_sequences=[2, 3])
    run("historical_rule_name_tracks_stable_id", ["logs", "earlier-web"], expected_sequences=[2, 3])
    run("retired_rule_id_selects_retained_history", ["logs", "rule-retired"], expected_sequences=[1])
    run("current_group_shorthand_uses_event_time_membership", ["logs", "batch"], expected_sequences=[3, 4])
    run("explicit_group_matches_shorthand", ["logs", "--group", "batch"], expected_sequences=[3, 4])
    run("historical_group_remains_queryable", ["logs", "old"], expected_sequences=[1, 2])
    run("current_server_name_over_reused_historical_name", ["logs", "--server", "dev"], expected_sequences=[2, 3, 4, 5, 6])
    run("retired_server_id_selects_retained_history", ["logs", "--server", "server-retired"], expected_sequences=[1])
    run("tail_applies_after_filter", ["logs", "--group", "batch", "--tail", "1"], expected_sequences=[4])
    run("tail_zero_returns_no_existing_records", ["logs", "--tail", "0"], expected_sequences=[])
    run("missing_history_selector_is_empty_success", ["logs", "missing"], expected_sequences=[])
    run("offline_status_preserves_saved_rule", ["status", "web"], expected_names=["web"])
    run("offline_group_status", ["status", "--group", "batch"], expected_names=["web"])
    run("offline_status_missing_selector_rejected", ["status", "missing"], expected_exit=2)
    run("status_query_selectors_are_exclusive", ["status", "web", "--server", "dev"], expected_exit=2)
    run("logs_query_selectors_are_exclusive", ["logs", "--server", "dev", "--group", "batch"], expected_exit=2)
    run("negative_tail_is_rejected", ["logs", "--tail", "-1"], expected_exit=2)
    after = {str(path.relative_to(directory)): path.read_bytes()
             for path in directory.rglob("*") if path.is_file()}
    assert before == after, "read-only commands changed fixture files"
    assert not list(directory.rglob("*.sock")), "unexpected daemon socket"

result = {"scope": "read-only CLI over temporary saved config/history; no service or network fixtures",
          "binary": str(BINARY), "passed": len(RESULTS), "failed": 0,
          "fixture_files_unchanged": True, "daemon_socket_created": False, "cases": RESULTS}
destination = Path(__file__).with_name("resume-cli-local.json")
destination.write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps({"passed": len(RESULTS), "failed": 0, "evidence": str(destination)}))
