#!/usr/bin/env python3
"""Compile copied engine source and execute memory-only deep lifecycle probes."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
AUDIT = Path(__file__).resolve().parent
SOURCE = ROOT / "crates/fwm-core/src"
DEPENDENCIES = ROOT / "target/debug/deps"
with tempfile.TemporaryDirectory(prefix="fwm-deep-memory-", dir="/private/tmp") as temporary:
    temporary = Path(temporary)
    (temporary / "empty_ssh_config").write_text("Host *\n  IdentitiesOnly yes\n")
    copied = temporary / "src"
    shutil.copytree(SOURCE, copied)
    shutil.copyfile(AUDIT / "deep-engine-probe.rs", copied / "engine/deep_engine_probe.rs")
    with (copied / "engine/mod.rs").open("a") as stream:
        stream.write('\nmod deep_engine_probe;\npub async fn deep_memory_probe() -> serde_json::Value { deep_engine_probe::run().await }\n')
    with (copied / "lib.rs").open("a") as stream:
        stream.write('\n#[tokio::main(flavor="current_thread")]\nasync fn main() { println!("{}", engine::deep_memory_probe().await); }\n')
    binary = temporary / "probe"
    command = ["rustc", "--edition=2024", "--crate-name", "fwm_deep_memory_audit", str(copied / "lib.rs"), "-L", f"dependency={DEPENDENCIES}", "-o", str(binary), "-A", "dead_code"]
    names = ["anyhow", "async_trait", "chrono", "directories", "fs2", "glob", "data_encoding", "hmac", "sha1", "rand", "russh", "serde", "serde_json", "thiserror", "tokio", "tokio_util", "toml", "tempfile", "tracing", "uuid"]
    libraries = {}
    for name in names:
        candidates = list(DEPENDENCIES.glob(f"lib{name}-*.rlib")) or list(DEPENDENCIES.glob(f"lib{name}-*.dylib"))
        library = max(candidates, key=lambda path: path.stat().st_mtime)
        libraries[name] = str(library)
        command.extend(["--extern", f"{name}={library}"])
    build = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if build.returncode:
        raise RuntimeError(build.stderr)
    run = subprocess.run([str(binary)], cwd=temporary, capture_output=True, text=True, timeout=30)
    if run.returncode:
        raise RuntimeError(run.stderr + run.stdout)
    result = json.loads(run.stdout)
    result["build_libraries"] = libraries
    result["source_sha256"] = {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in SOURCE.rglob("*") if path.is_file()}
    result["stderr"] = run.stderr
    result["exit_code"] = run.returncode
    (AUDIT / "deep-engine-result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"exit_code":run.returncode,"normal_controls":result["normal_controls"],"matrix_cases":len(result["state_task_matrix"]),"shutdown_fault_cases":result["shutdown_fault_cases"]},indent=2))
