use fwm_core::model::Config;
use serde_json::Value;
use std::{fs, path::Path, process::Command};

struct Fixture {
    root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::create_dir(root.path().join("b")).unwrap();
        fs::write(
            root.path().join("a/ssh.conf"),
            "Host dev\n HostName 127.0.0.1\n Port 1\n User fixture\n IdentityAgent ./agent.sock\n",
        )
        .unwrap();
        Self { root }
    }
    fn run(&self, cwd: &Path, args: &[&str], success: bool) -> Value {
        let result = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .current_dir(cwd)
            .arg("--config-dir")
            .arg(self.root.path().join("manager"))
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            success,
            "{args:?}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(if success {
            &result.stdout
        } else {
            &result.stderr
        })
        .unwrap()
    }
    fn config(&self) -> Config {
        serde_json::from_value(self.run(self.root.path(), &["config", "export"], true)).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.root.path().join("manager"))
            .args(["daemon", "stop"])
            .output();
    }
}

#[test]
fn saved_relative_ssh_paths_survive_chdir_and_equivalent_files_are_accepted() {
    let f = Fixture::new();
    let a = f.root.path().join("a");
    let b = f.root.path().join("b");
    f.run(
        &a,
        &[
            "server",
            "add",
            "dev",
            "--ssh",
            "dev",
            "--ssh-config",
            "ssh.conf",
            "--identity",
            "keys/work",
            "--known-hosts",
            "trusted",
        ],
        true,
    );
    let cfg = f.config();
    let profile = cfg.server("dev").unwrap();
    let canonical = fs::canonicalize(&a).unwrap();
    assert_eq!(
        profile.ssh_config.as_ref().unwrap(),
        &canonical.join("ssh.conf")
    );
    assert_eq!(profile.identity_files, [canonical.join("keys/work")]);
    assert_eq!(
        profile.known_hosts.as_ref().unwrap(),
        &canonical.join("trusted")
    );
    for (cwd, path) in [
        (&a, "./ssh.conf".to_string()),
        (
            &b,
            canonical.join("ssh.conf").to_string_lossy().into_owned(),
        ),
    ] {
        let result = f.run(
            cwd,
            &["server", "check", "dev", "--ssh-config", &path],
            false,
        );
        assert_eq!(result["error"]["code"], "check_failed");
        assert!(
            result["data"]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["error"]
                    .as_str()
                    .is_some_and(|message| message.contains("Connection refused")))
        );
    }
    f.run(
        &b,
        &["server", "edit", "dev", "--identity", "new-key"],
        true,
    );
    assert_eq!(
        f.config().server("dev").unwrap().identity_files,
        [fs::canonicalize(&b).unwrap().join("new-key")]
    );
    assert_eq!(
        f.run(&b, &["daemon", "status"], true)["daemon_running"],
        false
    );
}

#[test]
fn diagnostics_show_actual_process_agent_and_commands_for_the_same_instance() {
    let f = Fixture::new();
    let a = f.root.path().join("a");
    f.run(
        &a,
        &[
            "server",
            "add",
            "dev",
            "--ssh",
            "dev",
            "--ssh-config",
            "ssh.conf",
        ],
        true,
    );
    for online in [false, true] {
        if online {
            f.run(&a, &["daemon", "start"], true);
        }
        let result = f.run(&a, &["doctor", "--server", "dev"], false);
        let check = result["data"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["server"] == "dev")
            .unwrap();
        assert_eq!(
            check["authentication_context"]["process"],
            if online { "daemon" } else { "CLI" }
        );
        assert!(check["authentication_context"]["pid"].as_u64().unwrap() > 0);
        assert_eq!(
            check["authentication_context"]["agent_socket"],
            fs::canonicalize(&a)
                .unwrap()
                .join("agent.sock")
                .to_string_lossy()
                .as_ref()
        );
        let command = check["recovery"]["refresh_server"].as_str().unwrap();
        assert!(
            command.contains("--config-dir")
                && command.contains("manager")
                && command.contains("restart")
                && command.contains("dev")
        );
    }
}
