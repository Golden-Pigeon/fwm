#!/usr/bin/env python3
"""Include the unchanged source of MPL dependencies alongside binary releases."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"]))
output = Path(sys.argv[1]) / "dependency-sources"
output.mkdir(parents=True, exist_ok=True)
entries = []
for package in metadata["packages"]:
    if "MPL-2.0" not in (package.get("license") or ""):
        continue
    source = Path(package["manifest_path"]).parent
    name = f"{package['name']}-{package['version']}"
    shutil.copytree(source, output / name, ignore=shutil.ignore_patterns(".cargo-ok", ".cargo_vcs_info.json"))
    entries.append(f"- {name}: {package['source']}\n")
(output / "README.txt").write_text(
    "Unmodified MPL-2.0 dependency sources used by fwm.\n"
    "The release's DEPENDENCY_LICENSES.txt contains the license terms.\n\n" + "".join(entries),
    encoding="utf-8",
)
