#!/usr/bin/env python3
"""Inject one writer scheduling point in a temporary copy; never edit production."""
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile

base = Path(__file__).resolve().parent
root = base.parents[1]
deps = root / "target/debug/deps"
source = root / "crates/fwm-core/src/history/storage.rs"
original = source.read_text()
needle = "    let active = open_optional(path)?;"
assert original.count(needle) == 1
with tempfile.TemporaryDirectory(prefix="fwm-history-rotation-") as temporary:
    directory = Path(temporary)
    storage = directory / "storage.rs"
    storage.write_text(original.replace(needle, needle + "\n    crate::rotate_between_opens(path)?;"))
    fixture = directory / "main.rs"
    fixture.write_text((base / "deep-history-rotation.rs").read_text().replace("MODULE_PATH", str(storage)))
    binary = directory / "probe"
    command = ["rustc", "--edition=2024", str(fixture), "-L", f"dependency={deps}", "-o", str(binary)]
    for name in ["fwm_core", "serde_json", "tempfile"]:
        path = max(deps.glob(f"lib{name}-*.rlib"), key=lambda p: p.stat().st_mtime)
        command.extend(["--extern", f"{name}={path}"])
    build = subprocess.run(command, capture_output=True, text=True, timeout=60)
    assert build.returncode == 0, build.stderr
    result = {"cases": [json.loads(subprocess.check_output([str(binary), str(rotations)], text=True, timeout=30))
                        for rotations in [0, 1, 2]]}
    result["storage_sha256"] = hashlib.sha256(source.read_bytes()).hexdigest()
    (base / "deep-history-rotation.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
