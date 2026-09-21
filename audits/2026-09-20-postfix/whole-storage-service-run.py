"""Compile production adapters with a temporary fake-manager audit module."""
from pathlib import Path
import json
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "crates/fwm/src/platform"


def source_paths(text):
    return re.sub(r'#\[path = "([^"]+)"\]', lambda m: '#[path = "' + str(SOURCE / m[1]) + '"]', text)


def main():
    with tempfile.TemporaryDirectory(prefix="fwm-whole-service-", dir="/private/tmp") as tmp:
        directory = Path(tmp)
        fixture = directory / "service_tests.rs"
        fixture.write_text(source_paths((SOURCE / "service_tests.rs").read_text())
                           + '\n#[path = "' + str(Path(__file__).with_name("whole-storage-service.rs")) + '"]\nmod whole_review;\n')
        service = source_paths((SOURCE / "service.rs").read_text()).replace(str(SOURCE / "service_tests.rs"), str(fixture))
        (directory / "service.rs").write_text(service)
        (directory / "main.rs").write_text('#[path = "service.rs"]\nmod service;\n')
        deps = ROOT / "target/debug/deps"
        command = ["rustc", "--edition=2024", "--test", str(directory / "main.rs"),
                   "-L", "dependency=" + str(deps), "-o", str(directory / "review"), "-A", "dead_code"]
        for name in ["anyhow", "fwm_core", "serde", "serde_json", "uuid", "tempfile", "directories", "libc", "fs2"]:
            library = max(deps.glob("lib" + name + "-*.rlib"), key=lambda p: p.stat().st_mtime)
            command.extend(["--extern", name + "=" + str(library)])
        compiled = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=60)
        if compiled.returncode:
            raise RuntimeError(compiled.stderr)
        result = subprocess.run([str(directory / "review"), "whole_review", "--nocapture", "--test-threads=1"],
                                cwd=ROOT, capture_output=True, text=True, timeout=30)
    output = {"scope": "definition strings, temporary files, existing FakeExecutor; no native manager",
              "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr,
              "temporary_directory_cleaned": True}
    Path(__file__).with_name("whole-storage-service-results.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(result.stdout)
    if result.returncode:
        print(result.stderr)
        raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
