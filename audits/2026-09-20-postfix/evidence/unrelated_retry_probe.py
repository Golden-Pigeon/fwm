"""Extend the existing local protocol fixture with an unrelated profile edit."""
from pathlib import Path
import json
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
with tempfile.TemporaryDirectory(prefix="fwm-unrelated-") as directory:
    directory = Path(directory)
    original = (Path(__file__).parent.parent / "ssh_engine_probe.rs").read_text()
    old = 'config.forwards[0].name="renamed-only".into();'
    new = 'let mut unrelated=profile.clone();unrelated.id="unrelated-profile".into();unrelated.name="unrelated".into();config.servers.push(unrelated);'
    assert old in original
    source = directory / "probe.rs"
    source.write_text(original.replace(old,new))
    dependencies = ROOT / "target/debug/deps"
    command = ["rustc", "--edition=2024", str(source), "-L", "dependency=" + str(dependencies),
               "-o", str(directory / "probe")]
    for name in ["fwm_core", "russh", "tokio", "serde_json"]:
        library = max(dependencies.glob("lib" + name + "-*.rlib"), key=lambda path: path.stat().st_mtime)
        command.extend(["--extern", name + "=" + str(library)])
    subprocess.run(command, check=True, cwd=ROOT)
    result = subprocess.run([str(directory / "probe"), "metadata_attention", str(directory)],
                            capture_output=True, text=True, check=True, timeout=15)
    assert "requests_before_rename=1" in result.stdout
    assert "requests_after_rename=2" in result.stdout
    print(json.dumps({"action":"add a different server profile with no rules", "exit":result.returncode,
                      "stdout":result.stdout,"stderr":result.stderr},indent=2))
