#!/usr/bin/env python3
"""Ordinary offline CLI mutations showing ambiguity of a deleted stable ID."""
import json
from pathlib import Path
import subprocess
import tempfile

base = Path(__file__).resolve().parent
binary = base.parents[1] / "target/debug/fwm"
calls = []
with tempfile.TemporaryDirectory(prefix="fwm-history-id-") as temporary:
    directory = Path(temporary)

    def cli(*args):
        process = subprocess.run([str(binary), "--config-dir", temporary, "--json", *args],
                                 capture_output=True, text=True, timeout=10)
        assert process.returncode == 0, (args, process.returncode, process.stderr)
        value = json.loads(process.stdout)
        calls.append({"args": args, "exit": process.returncode})
        return value

    def ids(value):
        return sorted({entry["event"]["forward_id"] for entry in value["events"]})

    cli("add", "first", "--server", "fixture", "--local", "--port", "34101", "--disabled")
    first = cli("config", "export")["forwards"][0]["id"]
    cli("remove", "first")
    before = cli("logs", first)
    assert ids(before) == [first]
    cli("add", first, "--server", "fixture", "--local", "--port", "34102", "--disabled")
    second = cli("config", "export")["forwards"][0]["id"]
    assert second != first
    cli("remove", first)
    after = cli("logs", first)
    assert ids(after) == sorted([first, second])
    assert not after["warnings"]
    # Unambiguous historical name and the second stable ID remain well behaved.
    assert ids(cli("logs", "first")) == [first]
    assert ids(cli("logs", second)) == [second]
    assert not cli("config", "export")["forwards"]
    assert not (directory / "state/daemon.sock").exists()
    result = {"case": "deleted_id_unioned_with_other_deleted_name", "first_id": first,
              "second_id": second, "before": before, "after": after,
              "normal_controls_passed": True, "calls": calls,
              "scope": "temporary offline CLI only; every added rule disabled; no daemon or network"}

(base / "deep-history-cli.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
print(json.dumps({"confirmed": result["case"], "unexpected_second_id": second,
                  "evidence": str(base / "deep-history-cli.json")}))
