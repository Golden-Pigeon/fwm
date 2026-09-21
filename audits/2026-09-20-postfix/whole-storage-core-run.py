"""Compile a temporary copy of Store with audit-only pure-file tests."""
from pathlib import Path
import hashlib
import json
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "crates/fwm-core/src"


def main():
    with tempfile.TemporaryDirectory(prefix="fwm-whole-storage-core-", dir="/private/tmp") as tmp:
        directory = Path(tmp)
        shutil.copytree(SOURCE / "store", directory / "store")
        source = (SOURCE / "store.rs").read_text()
        (directory / "store.rs").write_text(source + '\n#[cfg(test)]\n#[path = "whole-storage-core.rs"]\nmod deep_review;\n')
        shutil.copy2(Path(__file__).with_name("whole-storage-core.rs"), directory / "whole-storage-core.rs")
        (directory / "main.rs").write_text('pub use fwm_core::{model, paths, ssh};\nmod store;\n')
        deps = ROOT / "target/debug/deps"
        command = ["rustc", "--edition=2024", "--test", str(directory / "main.rs"),
                   "-L", "dependency=" + str(deps), "-o", str(directory / "review"), "-A", "dead_code"]
        libraries = ["anyhow", "fwm_core", "serde", "serde_json", "uuid", "tempfile", "toml", "tracing", "sha1"]
        for name in libraries:
            library = max(deps.glob("lib" + name + "-*.rlib"), key=lambda p: p.stat().st_mtime)
            command.extend(["--extern", name + "=" + str(library)])
        compile_result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=60)
        if compile_result.returncode:
            raise RuntimeError(compile_result.stderr)
        result = subprocess.run([str(directory / "review"), "store::deep_review::", "--nocapture", "--test-threads=1"],
                                cwd=ROOT, capture_output=True, text=True, timeout=60)
        sources = [SOURCE / "store.rs", *sorted((SOURCE / "store").rglob("*.rs"))]
        output = {"scope": "pure local Store/FileOps fixtures; no daemon, API, SSH or services",
                  "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr,
                  "source_sha256": {str(p.relative_to(ROOT)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources}}
    output["temporary_directory_cleaned"] = True
    Path(__file__).with_name("whole-storage-core-results.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(result.stdout)
    if result.returncode:
        print(result.stderr)
        raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
