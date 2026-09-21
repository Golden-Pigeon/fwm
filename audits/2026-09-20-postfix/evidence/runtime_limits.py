"""Reproduce long Unix IPC paths and unbounded startup diagnostics in temp dirs."""
import json
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
BINARY = ROOT / "target/release/fwm"
OUTPUT = Path(__file__).parent

with tempfile.TemporaryDirectory(prefix="fwm-path-", dir="/private/tmp") as directory:
    config = Path(directory) / ("profile-" + "a" * 95)
    config.mkdir()
    (config / "config.toml").write_text("schema_version = 3\nrevision = 0\nservers = []\nforwards = []\n")
    result = subprocess.run([str(BINARY), "--config-dir", str(config), "--json", "daemon", "start"],
                            capture_output=True, text=True, timeout=20)
    status = subprocess.run([str(BINARY), "--config-dir", str(config), "--json", "daemon", "status"],
                            capture_output=True, text=True, timeout=5)
    evidence = {"socket_path_bytes": len(str(config / "state/daemon.sock").encode()),
                "start_exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr,
                "daemon_log": (config / "state/daemon.log").read_text(), "status": status.stdout}
    assert result.returncode != 0 and "path" in evidence["daemon_log"]
    (OUTPUT / "long-config-path.json").write_text(json.dumps(evidence, indent=2) + "\n")

with tempfile.TemporaryDirectory(prefix="fwm-log-", dir="/private/tmp") as directory:
    config = Path(directory)
    (config / "state").mkdir()
    (config / "config.toml").write_text("schema_version=3\n" + "x" * 200000 + " = [\n")
    log = config / "state/daemon.log"
    rows = []
    for attempt in range(1, 14):
        # The same append-only sink used by background::spawn and LaunchAgent.
        with log.open("ab") as output:
            result = subprocess.run([str(BINARY), "--config-dir", str(config), "daemon", "run"],
                                    stdout=output, stderr=output, timeout=10)
        rows.append({"attempt": attempt, "exit": result.returncode, "log_bytes": log.stat().st_size})
    evidence = {"config_bytes": (config / "config.toml").stat().st_size, "rows": rows,
                "rotation_files": [file.name for file in (config / "state").glob("daemon.log*")],
                "error_prefix": log.read_text()[:160]}
    assert log.stat().st_size > 5_000_000 and evidence["rotation_files"] == ["daemon.log"]
    (OUTPUT / "startup-log.json").write_text(json.dumps(evidence, indent=2) + "\n")
