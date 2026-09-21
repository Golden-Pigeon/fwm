//! Missing IDs in a first handwritten config survive separate CLI reads.
use std::{fs, net::TcpListener, process::Command};

use fwm_core::model::Config;
use serde_json::Value;

struct Fixture {
    directory: tempfile::TempDir,
    source: String,
}

impl Fixture {
    fn new(with_forward: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh = directory.path().join("empty-ssh-config");
        fs::write(&ssh, "").unwrap();
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut source = format!(
            "schema_version=3\n[[servers]]\nname='dev'\nhost='127.0.0.1'\nuser='fixture'\nport={port}\nssh_config={}\n",
            toml::Value::String(ssh.to_string_lossy().into_owned())
        );
        if with_forward {
            source.push_str("id='explicit-server'\n[[forwards]]\nname='web'\nserver_id='explicit-server'\nkind='local'\nlisten='127.0.0.1:31997'\ntarget='localhost:8080'\ndesired_state='stopped'\n");
        }
        fs::write(directory.path().join("config.toml"), &source).unwrap();
        Self { directory, source }
    }

    fn run(&self, args: &[&str], success: bool) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.directory.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(if success {
            &output.stdout
        } else {
            &output.stderr
        })
        .unwrap()
    }

    fn config(&self) -> Config {
        serde_json::from_value(self.run(&["config", "export"], true)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"], true);
    }
}

#[test]
fn missing_server_id_is_read_only_stable_and_can_be_used_by_first_offline_add() {
    let fixture = Fixture::new(false);
    let before = fixture.config();
    for _ in 0..2 {
        assert_eq!(fixture.config(), before);
        let doctor = fixture.run(&["doctor", "--server", "dev"], false);
        assert_eq!(doctor["data"]["configuration"]["unapplied_changes"], false);
        assert_eq!(
            doctor["data"]["configuration"]["using_applied_snapshot"],
            false
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.directory.path().join("config.toml")).unwrap(),
        fixture.source
    );
    assert!(!fixture.directory.path().join("state/applied.toml").exists());
    fixture.run(
        &[
            "add",
            "web",
            "--server",
            "dev",
            "--local",
            "--port",
            "31998",
            "--disabled",
        ],
        true,
    );
    let after = fixture.config();
    assert_eq!(after.servers[0].id, before.servers[0].id);
    assert_eq!(after.forwards[0].server_id, before.servers[0].id);
    assert_eq!(
        fixture.run(&["daemon", "status"], true)["daemon_running"],
        false
    );
}

#[test]
fn first_server_edit_and_later_rename_keep_the_generated_identity() {
    let fixture = Fixture::new(false);
    let before = fixture.config();
    fixture.run(&["server", "edit", "dev", "--rename", "renamed"], true);
    let after = fixture.config();
    assert_eq!(after.servers[0].name, "renamed");
    assert_eq!(after.servers[0].id, before.servers[0].id);
    assert_eq!(fixture.config(), after);
    assert_eq!(
        fixture.run(&["daemon", "status"], true)["daemon_running"],
        false
    );
}

#[test]
fn missing_forward_id_survives_first_edit_up_or_down() {
    for operation in ["edit", "up", "down"] {
        let fixture = Fixture::new(true);
        let before = fixture.config();
        let args = if operation == "edit" {
            vec!["edit", "web", "--rename", "renamed"]
        } else {
            vec![operation, "web"]
        };
        fixture.run(&args, true);
        let after = fixture.config();
        assert_eq!(after.servers[0].id, "explicit-server");
        assert_eq!(after.forwards[0].id, before.forwards[0].id);
        assert_eq!(after.forwards[0].server_id, "explicit-server");
        assert_eq!(fixture.config(), after);
    }
}
