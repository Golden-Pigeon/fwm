use super::*;
use crate::cli::{
    add,
    args::{AddArgs, Cli, Command as CliCommand},
};
use fwm_api::protocol::StatusSnapshot;
use fwm_core::model::RuntimeState;
use fwm_core::model::{ConnectionMode, ForwardStatus, RemoteCleanup, ServerProfile};

fn arguments(name: &str, ports: &str) -> AddArgs {
    let cli = Cli::try_parse_from([
        "fwm",
        "add",
        name,
        "--server",
        "dev",
        "--local",
        "--port",
        ports,
        "--disabled",
    ])
    .unwrap();
    let CliCommand::Add(args) = cli.command else {
        panic!("expected add")
    };
    args
}

fn config() -> Config {
    let mut server = ServerProfile::new("dev");
    server.host = Some("example.test".into());
    Config {
        servers: vec![server],
        ..Default::default()
    }
}

fn create(config: &Config, name: &str, ports: &str) -> Result<Vec<ForwardSpec>> {
    let args = arguments(name, ports);
    let tunnels = parse::tunnels(args.local.as_deref(), None, None, None, &args.ports, None)?;
    Ok(add::plan(config, &args, tunnels, Vec::new())?.forwards)
}

fn edit_rule(config: &Config, arguments: &[&str]) -> Result<ForwardSpec> {
    let mut argv = vec!["fwm", "edit", "web"];
    argv.extend(arguments);
    let CliCommand::Edit(args) = Cli::try_parse_from(argv)?.command else {
        unreachable!()
    };
    edited(config, &args)
}

fn relay_config() -> (Config, ForwardSpec) {
    let mut a = ServerProfile::new("host-a");
    a.host = Some("127.0.0.1".into());
    let mut c = ServerProfile::new("host-c");
    c.host = Some("127.0.0.1".into());
    let relay = ForwardSpec {
        id: "rule-1".into(),
        name: "web".into(),
        group: None,
        server_id: a.id.clone(),
        tunnel: Tunnel::Relay {
            listen: "127.0.0.1:8080".parse().unwrap(),
            target: "localhost:9000".parse().unwrap(),
            target_server_id: c.id.clone(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Dedicated,
        remote_cleanup: RemoteCleanup::Verified,
    };
    (
        Config {
            servers: vec![a, c],
            forwards: vec![relay.clone()],
            ..Default::default()
        },
        relay,
    )
}

#[test]
fn relay_edits_patch_one_side_and_keep_the_other() {
    let (config, relay) = relay_config();
    let source_only = edit_rule(&config, &["--src", "8081"]).unwrap();
    assert_eq!(source_only.tunnel.listen().to_string(), "127.0.0.1:8081");
    assert_eq!(
        source_only.tunnel.target().unwrap().to_string(),
        "localhost:9000"
    );
    let target_only = edit_rule(&config, &["--tgt", "9001"]).unwrap();
    assert_eq!(target_only.tunnel.listen(), relay.tunnel.listen());
    assert_eq!(
        target_only.tunnel.target().unwrap().to_string(),
        "localhost:9001"
    );
    let address = edit_rule(&config, &["--src", "0.0.0.0:8080"]).unwrap();
    assert_eq!(address.tunnel.listen().to_string(), "0.0.0.0:8080");
    let host = edit_rule(&config, &["--tgt", "db.internal:9000"]).unwrap();
    assert_eq!(
        host.tunnel.target().unwrap().to_string(),
        "db.internal:9000"
    );
    let both = edit_rule(&config, &["--port", "7000"]).unwrap();
    assert_eq!(both.tunnel.listen().to_string(), "127.0.0.1:7000");
    assert_eq!(both.tunnel.target().unwrap().to_string(), "localhost:7000");
    // Metadata edits keep the relay tunnel and cleanup untouched.
    let rename = edit_rule(&config, &["--rename", "a-to-c"]).unwrap();
    assert!(matches!(rename.tunnel, Tunnel::Relay { .. }));
    assert_eq!(rename.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(rename.connection_mode, ConnectionMode::Dedicated);
}

#[test]
fn relay_edits_reject_type_conversions_shared_mode_and_foreign_flags() {
    let (relay_cfg, _) = relay_config();
    for arguments in [
        vec!["--local"],
        vec!["--remote"],
        vec!["--dynamic", "1080"],
        vec!["--remote-dynamic", "7897"],
        vec!["--connection-mode", "shared"],
    ] {
        let error = edit_rule(&relay_cfg, &arguments)
            .err()
            .unwrap_or_else(|| panic!("{arguments:?} unexpectedly succeeded"))
            .to_string();
        assert!(
            error.contains("relay") || error.contains("shared"),
            "{arguments:?}: {error}"
        );
    }
    // Relay-only flags are explicit errors on other rule types; the unified
    // --tgt [HOST:]PORT grammar, however, works for every fixed forward.
    let mut local_config = config();
    let mut dev = local_config.servers[0].clone();
    dev.name = "dev".into();
    local_config.forwards.push(ForwardSpec {
        id: "rule-1".into(),
        name: "web".into(),
        group: None,
        server_id: dev.id.clone(),
        tunnel: Tunnel::Local {
            listen: "127.0.0.1:3000".parse().unwrap(),
            target: "localhost:3000".parse().unwrap(),
        },
        desired_state: DesiredState::Running,
        connection_mode: ConnectionMode::Shared,
        remote_cleanup: RemoteCleanup::Off,
    });
    let error = edit_rule(&local_config, &["--tgt-server", "host-c"])
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("relay"), "{error}");
    let moved = edit_rule(&local_config, &["--tgt", "db.internal:5432"]).unwrap();
    assert_eq!(
        moved.tunnel.target().unwrap().to_string(),
        "db.internal:5432"
    );
    assert_eq!(moved.tunnel.listen().to_string(), "127.0.0.1:3000");
}

#[test]
fn editing_direction_updates_cleanup_and_retains_explicit_off() {
    let mut config = config();
    config.forwards = create(&config, "web", "3000").unwrap();
    let remote = edit_rule(&config, &["--remote", "--port", "3000"]).unwrap();
    assert_eq!(remote.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(remote.connection_mode, ConnectionMode::Dedicated);
    config.forwards[0] = remote;
    let metadata = edit_rule(
        &config,
        &["--rename", "renamed", "--connection-mode", "shared"],
    )
    .unwrap();
    assert_eq!(metadata.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(metadata.connection_mode, ConnectionMode::Dedicated);
    let local = edit_rule(&config, &["--local", "--port", "3000"]).unwrap();
    assert_eq!(local.remote_cleanup, RemoteCleanup::Off);
    let dynamic = edit_rule(&config, &["--dynamic", "1080"]).unwrap();
    assert_eq!(dynamic.remote_cleanup, RemoteCleanup::Off);
    config.forwards[0] = edit_rule(
        &config,
        &["--remote-cleanup", "off", "--connection-mode", "shared"],
    )
    .unwrap();
    let metadata = edit_rule(&config, &["--rename", "renamed"]).unwrap();
    assert_eq!(metadata.remote_cleanup, RemoteCleanup::Off);
    assert_eq!(metadata.connection_mode, ConnectionMode::Shared);
    let remote = edit_rule(&config, &["--remote-cleanup", "verified"]).unwrap();
    assert_eq!(remote.remote_cleanup, RemoteCleanup::Verified);
    assert_eq!(remote.connection_mode, ConnectionMode::Dedicated);
}

#[test]
fn editing_cannot_enable_verified_cleanup_for_nonremote_directions() {
    let mut config = config();
    config.forwards = create(&config, "web", "3000").unwrap();
    for arguments in [
        vec!["--remote-cleanup", "verified"],
        vec!["--dynamic", "1080", "--remote-cleanup", "verified"],
    ] {
        assert!(
            edit_rule(&config, &arguments)
                .unwrap_err()
                .to_string()
                .contains("requires a remote forward")
        );
    }
    let remote = edit_rule(
        &config,
        &["--remote", "--port", "3000", "--remote-cleanup", "off"],
    )
    .unwrap();
    assert_eq!(remote.remote_cleanup, RemoteCleanup::Off);
}

#[test]
fn names_single_and_batch_rules_after_deduplication() {
    let config = config();
    assert_eq!(create(&config, "web", "3000,3000").unwrap()[0].name, "web");
    let forwards = create(&config, "web", "3002,3000-3001").unwrap();
    assert_eq!(
        forwards
            .iter()
            .map(|rule| rule.name.as_str())
            .collect::<Vec<_>>(),
        ["web-3000", "web-3001", "web-3002"]
    );
    assert!(
        forwards
            .iter()
            .all(|rule| rule.desired_state == DesiredState::Stopped)
    );
}

#[test]
fn validates_every_generated_name_and_rejects_existing_names_without_changes() {
    let mut config = config();
    config.forwards = create(&config, "web-3002", "8000").unwrap();
    let original = config.clone();
    assert!(
        create(&config, "web", "3000-3002")
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert_eq!(config, original);
    assert!(create(&config, &"x".repeat(96), "3000-3001").is_err());
}

#[test]
fn waits_for_created_ids_despite_renames_and_ignores_unrelated_rules() {
    let forwards = create(&config(), "web", "3000-3001").unwrap();
    let selected_id = forwards[0].id.clone();
    let mut snapshot = StatusSnapshot {
        daemon_instance_id: "daemon".into(),
        config_revision: 1,
        forwards: forwards
            .into_iter()
            .map(|rule| ForwardStatus {
                target_server: None,
                id: rule.id,
                name: rule.name,
                group: rule.group,
                server: "dev".into(),
                kind: rule.tunnel.kind().into(),
                listen: rule.tunnel.listen().to_string(),
                target: rule.tunnel.target().map(ToString::to_string),
                desired_state: DesiredState::Running,
                state: RuntimeState::Established,
                retry_count: 0,
                next_retry_unix_ms: None,
                last_error: None,
                active_connections: 0,
            })
            .collect(),
    };
    snapshot.forwards[0].name = "renamed".into();
    snapshot.forwards[1].name = "web-3000".into();
    snapshot.forwards[1].state = RuntimeState::NeedsAttention;
    retain_selected(&mut snapshot, &[selected_id]);
    assert_eq!(snapshot.forwards.len(), 1);
    assert_eq!(snapshot.forwards[0].name, "renamed");
    assert_eq!(snapshot.forwards[0].state, RuntimeState::Established);
}
