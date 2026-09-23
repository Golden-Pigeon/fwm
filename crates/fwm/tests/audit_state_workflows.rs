//! Regressions for audit U32–U36 through the public CLI and persisted history.
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

struct Fixture {
    directory: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fixture.ok(&[
            "server",
            "add",
            "dev",
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--user",
            "fixture",
        ]);
        fixture
    }
    fn path(&self) -> PathBuf {
        self.directory.path().into()
    }
    fn run(&self, args: &[&str]) -> Output {
        use tokio::io::AsyncReadExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                eprintln!("fixture CLI starting: {args:?}");
                let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fwm"))
                    .arg("--config-dir")
                    .arg(self.path())
                    .arg("--json")
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap();
                let mut stdout = child.stdout.take().unwrap();
                let mut stderr = child.stderr.take().unwrap();
                let out = tokio::spawn(async move {
                    let mut bytes = vec![];
                    stdout.read_to_end(&mut bytes).await.unwrap();
                    bytes
                });
                let err = tokio::spawn(async move {
                    let mut bytes = vec![];
                    stderr.read_to_end(&mut bytes).await.unwrap();
                    bytes
                });
                let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "CLI did not exit: {args:?}; daemon log: {:?}",
                            fs::read_to_string(self.path().join("state/daemon.log"))
                        )
                    })
                    .unwrap();
                eprintln!("fixture CLI exited: {args:?}: {status}");
                let stdout = tokio::time::timeout(std::time::Duration::from_secs(5), out)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("CLI exited but a descendant still owns stdout: {args:?}")
                    })
                    .unwrap();
                let stderr = tokio::time::timeout(std::time::Duration::from_secs(5), err)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("CLI exited but a descendant still owns stderr: {args:?}")
                    })
                    .unwrap();
                Output {
                    status,
                    stdout,
                    stderr,
                }
            })
    }
    fn ok(&self, args: &[&str]) -> Value {
        let result = self.run(args);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&result.stdout).unwrap()
    }
    fn add(&self, name: &str, port: &str, group: Option<&str>) {
        let mut args = vec![
            "add",
            name,
            "--server",
            "dev",
            "--local",
            "--port",
            port,
            "--disabled",
        ];
        if let Some(group) = group {
            args.extend(["--group", group]);
        }
        self.ok(&args);
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[test]
fn current_group_logs_cannot_be_shadowed_by_a_retired_rule_name() {
    let f = Fixture::new();
    f.add("task", "33000", Some("old"));
    f.ok(&["edit", "task", "--rename", "archived"]);
    f.add("new", "33001", Some("task"));
    let selected = f.ok(&["logs", "task"]);
    let explicit = f.ok(&["logs", "--group", "task"]);
    assert_eq!(selected, explicit);
    assert!(
        selected["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["forward_name"] == "new")
    );
    f.ok(&["edit", "old", "--rename", "renamed"]);
    assert_eq!(
        f.ok(&["logs", "renamed"]),
        f.ok(&["logs", "--group", "renamed"])
    );
}

#[test]
fn server_changes_remain_filterable_offline_online_after_rename_and_removal() {
    let f = Fixture::new();
    let id = f.ok(&["server", "list"])["servers"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    f.ok(&["server", "edit", "dev", "--port", "2"]);
    let offline = f.ok(&["logs", "--server", "dev"]);
    assert_eq!(offline["events"].as_array().unwrap().len(), 2);
    for event in offline["events"].as_array().unwrap() {
        assert_eq!(event["server_id"], id);
        assert_eq!(event["server_name"], "dev");
    }
    f.ok(&["daemon", "start"]);
    f.ok(&["server", "edit", "dev", "--rename", "prod"]);
    f.ok(&["server", "remove", "prod"]);
    f.ok(&["daemon", "stop"]);
    let history = f.ok(&["logs", "--server", &id]);
    let entries = history["events"].as_array().unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[2]["server_name"], "prod");
    assert_eq!(entries[3]["server_name"], "prod");
    assert_eq!(
        entries[0]["server_name"], "dev",
        "historical labels are immutable"
    );
}

#[test]
fn damaged_snapshot_has_a_safe_explicit_recovery_command_with_backups() {
    let f = Fixture::new();
    f.add("web", "33000", None);
    let config = f.ok(&["config", "export"]);
    let id = config["forwards"][0]["id"].clone();
    let candidate = f.path().join("config.toml");
    let snapshot = f.path().join("state/applied.toml");
    let original = fs::read(&candidate).unwrap();
    fs::write(&snapshot, "broken = [").unwrap();
    assert!(f.ok(&["config", "validate"])["valid"].as_bool().unwrap());
    let bad = f.run(&["config", "reload"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("config recover --from-candidate"));
    let recovered = f.ok(&["config", "recover", "--from-candidate"]);
    assert_eq!(recovered["saved"], true);
    let backup = PathBuf::from(recovered["data"]["backup_directory"].as_str().unwrap());
    assert_eq!(fs::read(backup.join("config.toml")).unwrap(), original);
    assert_eq!(
        fs::read_to_string(backup.join("applied.toml")).unwrap(),
        "broken = ["
    );
    assert_eq!(f.ok(&["config", "export"])["forwards"][0]["id"], id);
    assert_eq!(f.ok(&["status"])["forwards"][0]["desired_state"], "stopped");
    assert_eq!(f.ok(&["daemon", "status"])["daemon_running"], false);
}

#[test]
fn recovery_refuses_a_live_daemon_and_unreadable_intent_without_explicit_discard() {
    let f = Fixture::new();
    f.add("web", "33000", None);
    let snapshot = f.path().join("state/applied.toml");
    f.ok(&["daemon", "start"]);
    let before = fs::read(&snapshot).unwrap();
    assert!(
        !f.run(&["config", "recover", "--from-candidate"])
            .status
            .success()
    );
    assert_eq!(fs::read(&snapshot).unwrap(), before);
    f.ok(&["daemon", "stop"]);
    fs::write(&snapshot, "# fwm-control-overrides: broken\ninvalid = [").unwrap();
    let denied = f.run(&["config", "recover", "--from-candidate"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--discard-unreadable-intent"));
    let result = f.ok(&[
        "config",
        "recover",
        "--from-candidate",
        "--discard-unreadable-intent",
    ]);
    assert!(
        result["data"]["warning"]
            .as_str()
            .unwrap()
            .contains("explicitly discarded")
    );
    assert_eq!(f.ok(&["daemon", "status"])["daemon_running"], false);
}

#[cfg(unix)]
mod streaming {
    use super::*;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::UnixListener,
        process::{Child, Stdio},
        sync::mpsc,
        time::Duration,
    };
    struct Stream {
        child: Child,
        lines: mpsc::Receiver<Value>,
    }
    impl Stream {
        fn new(f: &Fixture, args: &[&str]) -> Self {
            let mut child = Command::new(env!("CARGO_BIN_EXE_fwm"))
                .arg("--config-dir")
                .arg(f.path())
                .arg("--json")
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let stdout = child.stdout.take().unwrap();
            let (sender, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    if let Ok(line) = line {
                        if let Ok(value) = serde_json::from_str(&line)
                            && sender.send(value).is_err()
                        {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            });
            Self { child, lines }
        }
        fn next(&self) -> Value {
            self.lines.recv_timeout(Duration::from_secs(8)).unwrap()
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
            let _ = self.child.wait();
        }
    }

    #[test]
    fn following_a_server_tracks_id_after_rename_without_changing_historical_labels() {
        let f = Fixture::new();
        f.add("web", "33000", None);
        let stream = Stream::new(&f, &["logs", "--server", "dev", "--follow", "--tail", "1"]);
        assert_eq!(stream.next()["server_name"], "dev");
        f.ok(&["server", "edit", "dev", "--rename", "prod"]);
        f.ok(&["edit", "web", "--tgt", "8081"]);
        let mut found = false;
        for _ in 0..2 {
            let event = stream.next();
            assert_eq!(event["server_name"], "prod");
            if event["forward_name"] == "web" {
                found = true;
            }
        }
        assert!(found);
        let first = f.ok(&["logs", "--server", "prod"]);
        assert_eq!(first["events"][0]["server_name"], "dev");
    }

    #[test]
    fn watch_survives_ipc_disconnect_and_resumes_live_updates() {
        let f = Fixture::new();
        let cfg = f.ok(&["config", "export"]);
        let path = f.path().join("state/daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = std::thread::spawn(move || {
            let mut views = 0;
            while views < 3 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(8)))
                    .unwrap();
                let mut header = [0; 4];
                socket.read_exact(&mut header).unwrap();
                let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                socket.read_exact(&mut bytes).unwrap();
                let req: Value = serde_json::from_slice(&bytes).unwrap();
                if req["command"]["method"] == "status_view" {
                    views += 1;
                    if views == 2 {
                        continue;
                    }
                }
                let data = match req["command"]["method"].as_str().unwrap() {
                    "status_view" => json!({"config":cfg,"snapshot":{
                        "daemon_instance_id":"mock","config_revision":cfg["revision"],"forwards":[]
                    }}),
                    _ => json!({"capabilities":["atomic_status_view"]}),
                };
                let response = json!({"api_version":1,"request_id":req["request_id"],"ok":true,"error":null,"data":data});
                let bytes = serde_json::to_vec(&response).unwrap();
                socket
                    .write_all(&(bytes.len() as u32).to_be_bytes())
                    .unwrap();
                socket.write_all(&bytes).unwrap();
            }
        });
        let stream = Stream::new(&f, &["status", "--watch"]);
        assert_eq!(stream.next()["daemon_state"], "running");
        let outage = stream.next();
        assert_eq!(outage["daemon_state"], "unavailable");
        assert_eq!(outage["runtime_available"], false);
        assert_eq!(stream.next()["daemon_state"], "running");
        drop(stream);
        task.join().unwrap();
        let _ = fs::remove_file(path);
    }
}
