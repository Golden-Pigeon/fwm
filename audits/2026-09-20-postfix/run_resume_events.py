#!/usr/bin/env python3
"""Run only the local functional event audit, linked to existing workspace artifacts."""
import hashlib
import itertools
import json
from pathlib import Path
import subprocess
import tempfile

base = Path(__file__).resolve().parent
root = base.parents[1]
deps = root / "target/debug/deps"
names = ["fwm_core", "fwm_api", "serde_json", "tracing", "tempfile", "tokio"]
libraries = {
    name: sorted(deps.glob(f"lib{name}-*.rlib"), key=lambda p: p.stat().st_mtime, reverse=True)
    for name in names
}
assert all(libraries.values()), "Build fwm-core and fwm-api before running this audit"
with tempfile.TemporaryDirectory(prefix="fwm-resume-events-") as directory:
    binary = Path(directory) / "events"
    command = ["rustc", "--edition=2024", "-A", "dead_code", str(base / "resume_events.rs"),
               "-L", f"dependency={deps}", "-o", str(binary)]
    # Core/API/Tokio may have multiple feature-compatible artifact sets.
    for core, api, tokio in itertools.product(libraries["fwm_core"], libraries["fwm_api"], libraries["tokio"]):
        selected = {name: paths[0] for name, paths in libraries.items()}
        selected.update(fwm_core=core, fwm_api=api, tokio=tokio)
        externs = [part for name, path in selected.items() for part in ["--extern", f"{name}={path}"]]
        build = subprocess.run(command + externs, text=True, capture_output=True, timeout=60)
        if build.returncode == 0:
            break
    else:
        raise RuntimeError(build.stderr)
    result = json.loads(subprocess.check_output([str(binary)], text=True, timeout=15))
    result["production_module_sha256"] = {
        str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
        for path in [root / "crates/fwm/src/daemon/events.rs", root / "crates/fwm-core/src/engine/state.rs"]
    }
    (base / "resume-events.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"cases": len(result["cases"]), "assertions_passed": result["assertions_passed"],
                      "output": str(base / "resume-events.json")}))
