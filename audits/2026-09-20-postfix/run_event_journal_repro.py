#!/usr/bin/env python3
"""Link a small audit program against existing debug artifacts; no repo build."""
import json
import pathlib
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[2]
deps = root / "target/debug/deps"
with tempfile.TemporaryDirectory(prefix="fwm-event-review-", dir="/private/tmp") as directory:
    binary = pathlib.Path(directory) / "event-review"
    command = ["rustc", "--edition=2024", "-A", "dead_code", str(pathlib.Path(__file__).with_name("event_journal_repro.rs")),
               "-L", f"dependency={deps}", "-o", str(binary)]
    for name in ["serde_json", "tracing", "tempfile"]:
        library = max(deps.glob(f"lib{name}-*.rlib"), key=lambda path: path.stat().st_mtime)
        command += ["--extern", f"{name}={library}"]
    # Different feature/test builds can coexist in this shared target. Select
    # a matching core/API pair without rebuilding or changing any source.
    complete = False
    last_error = ""
    for api in sorted(deps.glob("libfwm_api-*.rlib"), key=lambda path: path.stat().st_mtime, reverse=True):
        for core in sorted(deps.glob("libfwm_core-*.rlib"), key=lambda path: path.stat().st_mtime, reverse=True):
            result = subprocess.run(command + ["--extern", f"fwm_core={core}", "--extern", f"fwm_api={api}"],
                                    capture_output=True, text=True)
            if result.returncode == 0:
                complete = True
                break
            last_error = result.stderr
        if complete:
            break
    if not complete:
        raise RuntimeError(last_error)
    output = subprocess.check_output([str(binary)], text=True)
    value = json.loads(output)
    pathlib.Path(__file__).with_name("event-journal-round2.json").write_text(json.dumps(value, indent=2))
    print(json.dumps(value, indent=2))
