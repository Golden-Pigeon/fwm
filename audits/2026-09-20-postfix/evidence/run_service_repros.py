"""Compile current production adapters plus existing fake-manager fixtures.

The generated harness lives in a temporary directory. No production/test file
is changed and no command reaches the native OS service manager.
"""
from pathlib import Path
import os
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / "crates/fwm/src/platform"


def source_paths(text):
    return re.sub(r'#\[path = "([^"]+)"\]', lambda m: '#[path = "' + str(SOURCE / m[1]) + '"]', text)


with tempfile.TemporaryDirectory(prefix="fwm-postfix-service-") as directory:
    directory = Path(directory)
    fixtures = directory / "service_tests.rs"
    fixtures.write_text(source_paths((SOURCE / "service_tests.rs").read_text())
                        + '\n#[path = "' + str(Path(__file__).with_name("service_repros.rs")) + '"]\nmod postfix_review;\n')
    service = source_paths((SOURCE / "service.rs").read_text())
    service = service.replace(str(SOURCE / "service_tests.rs"), str(fixtures))
    (directory / "service.rs").write_text(service)
    (directory / "main.rs").write_text('#[path = "service.rs"]\nmod service;\n')
    dependencies = ROOT / "target/debug/deps"
    command = ["rustc", "--edition=2024", "--test", str(directory / "main.rs"),
               "-L", "dependency=" + str(dependencies), "-o", str(directory / "review"), "-A", "dead_code"]
    for name in ["anyhow", "fwm_core", "serde", "serde_json", "uuid", "tempfile", "directories", "libc", "fs2"]:
        library = max(dependencies.glob("lib" + name + "-*.rlib"), key=lambda path: path.stat().st_mtime)
        command.extend(["--extern", name + "=" + str(library)])
    subprocess.run(command, check=True, cwd=ROOT)
    subprocess.run([str(directory / "review"), "postfix_review", "--nocapture"], check=True, cwd=ROOT)
