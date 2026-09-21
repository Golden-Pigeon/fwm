//! Doctor reports configuration drafts without starting any forwarding.
use std::{fs, net::TcpListener, process::Command};

use fwm_core::{
    model::{Config, ServerProfile},
    paths::Paths,
    store::Store,
};
use serde_json::Value;

struct Fixture {
    _directory: tempfile::TempDir,
    paths: Paths,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
        Self {
            _directory: directory,
            paths,
        }
    }

    fn run(&self, args: &[&str], success: bool) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(&self.paths.config_dir)
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if success {
            serde_json::from_slice(&output.stdout).unwrap()
        } else {
            assert!(output.stdout.is_empty());
            serde_json::from_slice(&output.stderr).unwrap()
        }
    }

    fn assert_stopped(&self) {
        assert_eq!(
            self.run(&["daemon", "status"], true)["daemon_running"],
            false
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(&self.paths.config_dir)
            .args(["daemon", "stop"])
            .output();
    }
}

#[test]
fn doctor_checks_invalid_draft_online_and_offline_without_overwriting_it() {
    let cli = Fixture::new();
    Store::new(cli.paths.clone())
        .commit(&Config::default())
        .unwrap();
    for online in [false, true] {
        if online {
            cli.run(&["daemon", "start"], true);
        }
        let original = "schema_version = [invalid draft";
        fs::write(&cli.paths.config_file, original).unwrap();
        let result = cli.run(&["doctor"], false);
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "check_failed");
        let config = &result["data"]["configuration"];
        assert_eq!(config["valid"], false);
        assert_eq!(config["using_applied_snapshot"], true);
        assert!(config["error"].as_str().unwrap().contains("TOML"));
        assert_eq!(
            fs::read_to_string(&cli.paths.config_file).unwrap(),
            original
        );
        if !online {
            cli.assert_stopped();
        }
    }
}

#[test]
fn doctor_distinguishes_valid_unapplied_draft_from_applied_configuration() {
    let cli = Fixture::new();
    let applied = Config::default();
    Store::new(cli.paths.clone()).commit(&applied).unwrap();
    let mut draft = applied.clone();
    draft.defaults.retry.max_delay_secs += 10;
    let original = toml::to_string_pretty(&draft).unwrap();
    fs::write(&cli.paths.config_file, &original).unwrap();
    let result = cli.run(&["doctor"], true);
    let config = &result["data"]["configuration"];
    assert_eq!(config["valid"], true);
    assert_eq!(config["unapplied_changes"], true);
    assert_eq!(config["using_applied_snapshot"], true);
    assert!(
        config["warning"]
            .as_str()
            .unwrap()
            .contains("config reload")
    );
    assert_eq!(
        fs::read_to_string(&cli.paths.config_file).unwrap(),
        original
    );
    cli.assert_stopped();
}

#[test]
fn first_run_doctor_handles_empty_and_broken_directories_without_daemon() {
    let cli = Fixture::new();
    let empty = cli.run(&["doctor"], true);
    assert_eq!(empty["data"]["configuration"]["valid"], true);
    cli.assert_stopped();
    fs::create_dir_all(&cli.paths.config_dir).unwrap();
    fs::write(&cli.paths.config_file, "invalid [toml").unwrap();
    let broken = cli.run(&["doctor"], false);
    assert_eq!(broken["data"]["configuration"]["valid"], false);
    assert_eq!(
        broken["data"]["configuration"]["using_applied_snapshot"],
        false
    );
    cli.assert_stopped();
}

#[test]
fn selected_doctor_and_server_check_report_bad_draft_as_one_json_failure() {
    let cli = Fixture::new();
    let closed_port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    server.user = Some("fixture".into());
    server.port = Some(closed_port);
    server.ssh_config = Some(cli.paths.config_dir.join("empty-ssh-config"));
    fs::write(server.ssh_config.as_ref().unwrap(), "").unwrap();
    let config = Config {
        servers: vec![server],
        ..Config::default()
    };
    Store::new(cli.paths.clone()).commit(&config).unwrap();
    fs::write(&cli.paths.config_file, "invalid [toml").unwrap();
    for args in [
        &["doctor", "--server", "dev"][..],
        &["server", "check", "dev"][..],
    ] {
        let result = cli.run(args, false);
        assert_eq!(result["error"]["code"], "check_failed");
        assert_eq!(result["data"]["configuration"]["valid"], false);
        assert_eq!(
            result["data"]["configuration"]["using_applied_snapshot"],
            true
        );
        cli.assert_stopped();
    }
}
