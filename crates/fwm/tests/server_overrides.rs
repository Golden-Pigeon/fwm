//! Persisted CLI overrides must round-trip and restore actual SSH inheritance.
use fwm_core::{model::Config, ssh};
use serde_json::Value;
use std::{
    fs,
    process::{Command, Output},
};

struct Fixture {
    directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let ssh_config = directory.path().join("ssh_config");
        fs::write(
            &ssh_config,
            "Host dev\n HostName 127.0.0.1\n User inherited\n Port 2222\n IdentityFile ~/inherited-key\n UserKnownHostsFile ~/inherited-hosts\n ProxyJump inherited-jump\n IdentityAgent none\n GlobalKnownHostsFile none\n",
        )
        .unwrap();
        let fixture = Self { directory };
        fixture.ok(&[
            "server",
            "add",
            "dev",
            "--ssh",
            "dev",
            "--ssh-config",
            ssh_config.to_str().unwrap(),
            "--user",
            "override",
            "--port",
            "2200",
            "--identity",
            "override-key",
            "--known-hosts",
            "override-hosts",
            "--proxy-jump",
            "none",
        ]);
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(self.directory.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn config(&self) -> Config {
        serde_json::from_value(self.ok(&["config", "export"])).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

#[test]
fn unset_persists_and_restores_effective_ssh_settings() {
    let fixture = Fixture::new();
    let original = fixture.config().servers.remove(0);
    let effective = ssh::resolve(&original).unwrap();
    assert_eq!(effective.user, "override");
    assert_eq!(effective.port, 2200);
    assert!(
        effective.proxy_jump.is_empty(),
        "server add --proxy-jump none must disable inherited jumps"
    );
    fixture.ok(&[
        "server",
        "edit",
        "dev",
        "--unset",
        "user,port,identity,known-hosts",
        "--unset",
        "proxy-jump",
    ]);
    let inherited = fixture.config().servers.remove(0);
    assert_eq!(inherited.id, original.id);
    assert!(inherited.user.is_none());
    assert!(inherited.port.is_none());
    assert!(inherited.identity_files.is_empty());
    assert!(inherited.known_hosts.is_none());
    assert!(inherited.proxy_jump.is_empty());
    assert_eq!(inherited.ssh_config, original.ssh_config);
    let effective = ssh::resolve(&inherited).unwrap();
    assert_eq!(effective.user, "inherited");
    assert_eq!(effective.port, 2222);
    assert_eq!(effective.proxy_jump, ["inherited-jump"]);
    assert_eq!(effective.identity_files.len(), 1);
    assert!(effective.identity_files[0].ends_with("inherited-key"));
    assert!(effective.known_hosts.ends_with("inherited-hosts"));
    fixture.ok(&["server", "edit", "dev", "--proxy-jump", "none"]);
    fixture.ok(&["daemon", "restart"]);
    let disabled = fixture.config().servers.remove(0);
    assert_eq!(disabled.proxy_jump, ["none"]);
    assert!(ssh::resolve(&disabled).unwrap().proxy_jump.is_empty());
    fixture.ok(&["server", "edit", "dev", "--unset", "ssh-config"]);
    assert!(fixture.config().servers[0].ssh_config.is_none());
}

#[test]
fn invalid_overrides_and_conflicting_unset_never_change_saved_config() {
    let fixture = Fixture::new();
    let before = fixture.config();
    for (field, value) in [
        ("user", "alice"),
        ("port", "22"),
        ("identity", "key"),
        ("ssh-config", "config"),
        ("known-hosts", "known_hosts"),
        ("proxy-jump", "none"),
    ] {
        let flag = format!("--{field}");
        let output = fixture.run(&["server", "edit", "dev", &flag, value, "--unset", field]);
        assert!(!output.status.success(), "accepted set+unset {field}");
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot be combined")
        );
        assert_eq!(fixture.config(), before);
    }
    for flag in [
        "--user",
        "--identity",
        "--ssh-config",
        "--known-hosts",
        "--proxy-jump",
    ] {
        let output = fixture.run(&["server", "edit", "dev", flag, ""]);
        assert!(!output.status.success(), "accepted empty {flag}");
        assert!(output.stdout.is_empty());
        assert_eq!(fixture.config(), before);
    }
    let output = fixture.run(&["server", "edit", "dev", "--proxy-jump", "none,bastion"]);
    assert!(!output.status.success());
    assert_eq!(fixture.config(), before);
}

#[test]
fn validate_rejects_invalid_overrides_in_hand_edited_config() {
    let fixture = Fixture::new();
    let original = fixture.config();
    for invalid_user in ["", " ", "a\nb"] {
        let mut draft = original.clone();
        draft.servers[0].user = Some(invalid_user.into());
        fs::write(
            fixture.directory.path().join("config.toml"),
            toml::to_string(&draft).unwrap(),
        )
        .unwrap();
        let output = fixture.run(&["config", "validate"]);
        assert!(
            !output.status.success(),
            "validated invalid user {invalid_user:?}"
        );
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert!(error["error"]["message"].as_str().unwrap().contains("user"));
    }
}
