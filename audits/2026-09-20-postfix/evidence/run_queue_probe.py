from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
with tempfile.TemporaryDirectory(prefix="fwm-queue-build-") as directory:
    binary = Path(directory) / "probe"
    dependencies = ROOT / "target/debug/deps"
    command = ["rustc", "--edition=2024", str(Path(__file__).with_name("queue_cancel_probe.rs")),
               "-L", "dependency=" + str(dependencies), "-o", str(binary)]
    for name in ["fwm_core", "tokio", "serde_json", "tempfile"]:
        library = max(dependencies.glob("lib" + name + "-*.rlib"), key=lambda path: path.stat().st_mtime)
        command.extend(["--extern", name + "=" + str(library)])
    subprocess.run(command, check=True, cwd=ROOT)
    subprocess.run([str(binary)], check=True, cwd=ROOT)
