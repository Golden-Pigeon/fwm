"""Deterministically change a temporary config between the two SSH resolves.

The first read is a FIFO. Before closing it, its writer replaces the pathname
with a regular config file. Only ephemeral loopback SSH fixtures are used.
"""
from pathlib import Path
import json
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
BRANCH = r'''
    if mode == "double_resolve" {
        use std::io::Write;
        let mut checking = profile.clone();
        checking.host = None;
        checking.port = None;
        checking.ssh_alias = Some("audit".into());
        let fifo = dir.join("switching-config");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        checking.ssh_config = Some(fifo.clone());
        let first = "Host audit\n HostName 127.0.0.1\n Port 1\n IdentityAgent none\n GlobalKnownHostsFile none\n";
        let second = first.replace("Port 1", &format!("Port {port}"));
        let writing = std::thread::spawn(move || {
            let mut writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
            writer.write_all(first.as_bytes()).unwrap();
            let replacement = fifo.with_extension("next");
            std::fs::write(&replacement, second).unwrap();
            std::fs::rename(&replacement, &fifo).unwrap();
            drop(writer); // EOF makes the first resolve parse its old snapshot.
        });
        let first_result = ssh::check_connection(&checking, &policy).await;
        writing.join().unwrap();
        assert!(matches!(first_result, Err(ssh::SshError::UnknownHostKey {..})), "{first_result:?}");
        let second_result = ssh::check_connection(&checking, &policy).await;
        assert!(second_result.is_ok(), "{second_result:?}");
        println!("{}", serde_json::json!({"first_check_error":first_result.unwrap_err().to_string(),"same_final_file_second_check_ok":true,"actual_port":port,"first_handler_port":1,"trace":&*trace.lock().unwrap()}));
        accept.abort();
        return;
    }
'''

with tempfile.TemporaryDirectory(prefix="fwm-resolve-snapshot-") as directory:
    directory = Path(directory)
    original = (Path(__file__).parent.parent / "ssh_engine_probe.rs").read_text()
    marker = '    let mut config=Config{servers:vec![profile.clone()],..Default::default()};'
    assert marker in original
    source = directory / "probe.rs"
    source.write_text(original.replace(marker, BRANCH + marker))
    dependencies = ROOT / "target/debug/deps"
    command = ["rustc", "--edition=2024", str(source), "-L", "dependency=" + str(dependencies),
               "-o", str(directory / "probe")]
    for name in ["fwm_core", "russh", "tokio", "serde_json"]:
        library = max(dependencies.glob("lib" + name + "-*.rlib"), key=lambda path: path.stat().st_mtime)
        command.extend(["--extern", name + "=" + str(library)])
    subprocess.run(command, check=True, cwd=ROOT)
    result = subprocess.run([str(directory / "probe"), "double_resolve", str(directory)],
                            capture_output=True, text=True, check=True, timeout=20)
    print(json.dumps({"exit":result.returncode,"stdout":result.stdout,"stderr":result.stderr},indent=2))
