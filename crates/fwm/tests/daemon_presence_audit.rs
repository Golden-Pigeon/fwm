#[cfg(unix)]
use fwm_core::paths::Paths;
use serde_json::Value;
use std::process::Command;

#[test]
fn equivalent_profile_paths_identify_the_same_configuration_without_creating_it() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    std::fs::create_dir(&profile).unwrap();
    let invoke = |path: &std::path::Path| {
        let out = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(path)
            .args(["--json", "daemon", "status"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let direct = invoke(&profile);
    let dot = invoke(&profile.join("."));
    let parent = invoke(&profile.join("../profile"));
    assert_eq!(direct["config_dir"], dot["config_dir"]);
    assert_eq!(direct["config_dir"], parent["config_dir"]);
    assert_eq!(direct["daemon_running"], false);
    assert_eq!(std::fs::read_dir(&profile).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn unresponsive_owner_is_not_reported_stopped_or_replaced_and_stop_sends_shutdown() {
    use fs2::FileExt;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(root.path().into())).unwrap();
    paths.ensure_dirs().unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock_file)
        .unwrap();
    lock.lock_exclusive().unwrap();
    let listener = UnixListener::bind(&paths.ipc_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let methods = Arc::new(Mutex::new(Vec::<String>::new()));
    let thread_done = done.clone();
    let thread_methods = methods.clone();
    let task = std::thread::spawn(move || {
        let mut held = Vec::new();
        while !thread_done.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut header = [0; 4];
                    if socket.read_exact(&mut header).is_err() {
                        continue;
                    }
                    let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
                    if socket.read_exact(&mut bytes).is_err() {
                        continue;
                    }
                    let request: Value = serde_json::from_slice(&bytes).unwrap();
                    let method = request["command"]["method"].as_str().unwrap().to_owned();
                    thread_methods.lock().unwrap().push(method.clone());
                    if method == "shutdown" {
                        let body=serde_json::to_vec(&serde_json::json!({"api_version":1,"request_id":request["request_id"],"ok":true,"data":{},"error":null})).unwrap();
                        socket
                            .write_all(&(body.len() as u32).to_be_bytes())
                            .unwrap();
                        socket.write_all(&body).unwrap();
                        FileExt::unlock(&lock).unwrap();
                        break;
                    }
                    held.push(socket);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
    });
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(&paths.config_dir)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    };
    let result = std::panic::catch_unwind(|| {
        let status = invoke(&["daemon", "status"]);
        assert!(status.status.success());
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(status["daemon_state"], "unresponsive");
        assert!(status["daemon_running"].is_null());
        let start = invoke(&["daemon", "start"]);
        assert_eq!(start.status.code(), Some(5));
        let error: Value = serde_json::from_slice(&start.stderr).unwrap();
        assert_eq!(error["error"]["code"], "daemon_unresponsive");
        assert!(
            !paths.log_file.exists(),
            "must not launch a conflicting daemon"
        );
        let stop = invoke(&["daemon", "stop"]);
        assert!(
            stop.status.success(),
            "{}",
            String::from_utf8_lossy(&stop.stderr)
        );
        assert!(methods.lock().unwrap().iter().any(|m| m == "shutdown"));
    });
    done.store(true, Ordering::SeqCst);
    task.join().unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
