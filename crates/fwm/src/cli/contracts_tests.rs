//! CLI contracts which must hold across individual subcommand implementations.
use super::{
    args::{CleanupMode, Cli, Command, Mode},
    cleanup,
    completion::CompletionError,
    error_code, input, json_error, server_selection,
};
use fwm_core::model::{
    Config, ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
};
use serde_json::json;
use std::{ffi::OsString, path::Path, time::Duration};

#[test]
fn error_categories_survive_context_without_turning_operational_failures_into_bad_input() {
    for (code, exit) in [
        ("host_key", 3),
        ("authentication", 3),
        ("needs_attention", 3),
        ("trust_required", 3),
        ("check_failed", 3),
        ("wait_timeout", 4),
        ("daemon_unavailable", 5),
        ("ipc_timeout", 5),
        ("revision_conflict", 2),
    ] {
        let error = anyhow::Error::new(crate::client::ClientError {
            code: code.into(),
            message: "inner failure".into(),
        })
        .context("outer command");
        assert_eq!(error_code(&error), (code, exit));
        assert!(json_error(&error).is_none());
    }
    let io = anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "private file",
    ))
    .context("reading configuration");
    assert_eq!(error_code(&io), ("io_error", 5));
    assert_eq!(
        error_code(&anyhow::anyhow!("bad request")),
        ("invalid_request", 2)
    );
}

#[test]
fn completion_errors_keep_the_final_saved_state_payload_through_context_and_json_conversion() {
    for (code, exit) in [
        ("wait_timeout", 4),
        ("needs_attention", 3),
        ("check_failed", 3),
        ("trust_required", 3),
        ("wait_failed", 5),
    ] {
        let expected = json!({"ok": false, "error": {"code": code, "message": "not ready"}, "data": {"saved": true, "ready": false, "revision": 42, "runtime": {"forwards": []}}});
        let error = anyhow::Error::new(CompletionError {
            code: code.into(),
            message: "not ready".into(),
            result: expected.clone(),
        })
        .context("starting selected rules");
        assert_eq!(error_code(&error), (code, exit));
        assert_eq!(json_error(&error), Some(expected.clone()));
        let mut detached = json_error(&error).unwrap();
        detached["data"]["saved"] = json!(false);
        assert_eq!(json_error(&error), Some(expected));
    }
}

fn wait_arguments(command: &str, value: &str) -> Result<Duration, clap::Error> {
    let mut args = vec!["fwm", command];
    if command == "add" {
        args.extend(["--server", "dev", "--local", "--port", "3000"]);
    } else {
        args.push("web");
    }
    args.extend(["--wait", "--timeout", value]);
    Ok(match Cli::try_parse_from(args)?.command {
        Command::Add(args) => args.timeout.unwrap(),
        Command::Up(args) | Command::Restart(args) => args.timeout.unwrap(),
        _ => unreachable!(),
    })
}

#[test]
fn all_wait_commands_use_the_same_duration_units_and_preserve_subsecond_timeouts() {
    for command in ["add", "up", "restart"] {
        for (text, milliseconds) in [
            ("1ms", 1),
            ("500ms", 500),
            ("20", 20_000),
            ("20s", 20_000),
            ("2m", 120_000),
        ] {
            assert_eq!(
                wait_arguments(command, text).unwrap(),
                Duration::from_millis(milliseconds)
            );
        }
    }
}

#[test]
fn all_wait_commands_reject_invalid_or_overflowing_durations_before_execution() {
    for command in ["add", "up", "restart"] {
        for value in [
            "",
            "0",
            "0ms",
            "0s",
            "0m",
            "-1",
            "1.5s",
            "1h",
            "NaN",
            "18446744073709551616ms",
            "18446744073709552s",
            "307445734561826m",
        ] {
            assert!(
                wait_arguments(command, value).is_err(),
                "{command} --timeout {value}"
            );
        }
    }
}

#[test]
fn normalization_respects_option_values_global_paths_and_the_end_of_options_marker() {
    let raw = [
        "fwm",
        "--json",
        "--config-dir",
        "add",
        "add",
        "--server",
        "--remote",
        "--ssh-config",
        "-L3000",
        "--group",
        "-R4000",
        "--local",
        "5000",
        "--",
        "-R6000",
    ];
    let expected = [
        "fwm",
        "--json",
        "--config-dir",
        "add",
        "add",
        "--server",
        "--remote",
        "--ssh-config",
        "-L3000",
        "--group",
        "-R4000",
        "--local=5000",
        "--",
        "-R6000",
    ];
    assert_eq!(input::normalize(raw), expected.map(OsString::from));
}

#[test]
fn normalization_never_interprets_arguments_to_other_command_families_as_forwards() {
    for command in [
        "server", "daemon", "config", "status", "logs", "doctor", "service",
    ] {
        let raw = ["fwm", command, "add", "--remote", "3000:localhost:3000"];
        assert_eq!(input::normalize(raw), raw.map(OsString::from));
    }
}

fn rule(
    tunnel: Tunnel,
    connection_mode: ConnectionMode,
    remote_cleanup: RemoteCleanup,
) -> ForwardSpec {
    ForwardSpec {
        id: "id".into(),
        name: "web".into(),
        group: None,
        server_id: "server".into(),
        tunnel,
        desired_state: DesiredState::Stopped,
        connection_mode,
        remote_cleanup,
    }
}

#[test]
fn cleanup_policy_preserves_an_explicit_remote_opt_out_and_mode_until_overridden() {
    let remote = Tunnel::Remote {
        listen: "127.0.0.1:3000".parse().unwrap(),
        target: "localhost:3000".parse().unwrap(),
    };
    for mode in [ConnectionMode::Shared, ConnectionMode::Dedicated] {
        let previous = rule(remote.clone(), mode, RemoteCleanup::Off);
        assert_eq!(
            cleanup::effective(&remote, Some(&previous), None, None).unwrap(),
            (mode, RemoteCleanup::Off)
        );
        for request in [None, Some(Mode::Shared), Some(Mode::Dedicated)] {
            assert_eq!(
                cleanup::effective(
                    &remote,
                    Some(&previous),
                    request,
                    Some(CleanupMode::Verified)
                )
                .unwrap(),
                (ConnectionMode::Dedicated, RemoteCleanup::Verified)
            );
        }
        assert_eq!(
            cleanup::effective(
                &remote,
                Some(&previous),
                Some(Mode::Shared),
                Some(CleanupMode::Off)
            )
            .unwrap(),
            (ConnectionMode::Shared, RemoteCleanup::Off)
        );
    }
}

#[test]
fn leaving_remote_forwarding_turns_cleanup_off_and_rejects_explicit_verified_cleanup() {
    let listen = "127.0.0.1:3000".parse().unwrap();
    let target = "localhost:3000".parse().unwrap();
    let previous = rule(
        Tunnel::Remote { listen, target },
        ConnectionMode::Dedicated,
        RemoteCleanup::Verified,
    );
    for next in [
        Tunnel::Local {
            listen,
            target: "localhost:3000".parse().unwrap(),
        },
        Tunnel::Dynamic { listen },
    ] {
        assert_eq!(
            cleanup::effective(&next, Some(&previous), None, None).unwrap(),
            (ConnectionMode::Dedicated, RemoteCleanup::Off)
        );
        assert_eq!(
            cleanup::effective(&next, Some(&previous), Some(Mode::Shared), None).unwrap(),
            (ConnectionMode::Shared, RemoteCleanup::Off)
        );
        assert!(
            cleanup::effective(&next, Some(&previous), None, Some(CleanupMode::Verified)).is_err()
        );
    }
}

#[test]
fn selecting_an_inherited_profile_cannot_silently_replace_its_ssh_config() {
    let mut profile = ServerProfile::new("dev");
    profile.ssh_alias = Some("dev-alias".into());
    let config = Config {
        servers: vec![profile.clone()],
        ..Default::default()
    };
    for selector in [profile.name.as_str(), profile.id.as_str()] {
        let error = server_selection::select(&config, selector, Some(Path::new("other.conf")))
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("server edit NAME --ssh-config PATH")
        );
        let selected = server_selection::select(&config, selector, None).unwrap();
        assert!(!selected.is_new);
        assert_eq!(selected.profile, profile);
    }
    assert_eq!(config.servers, [profile]);
}

#[test]
fn new_alias_selection_validates_unicode_length_and_never_persists_on_failure() {
    let config = Config::default();
    let valid = "服".repeat(33);
    let selected = server_selection::select(&config, &valid, None).unwrap();
    assert_eq!(selected.profile.ssh_alias.as_deref(), Some(valid.as_str()));
    for alias in [
        "".to_owned(),
        "服".repeat(34),
        "bad alias".to_owned(),
        "user@host".to_owned(),
        "dev/../../other".to_owned(),
    ] {
        assert!(server_selection::select(&config, &alias, None).is_err());
    }
    assert!(config.servers.is_empty());
}

#[test]
fn service_runtime_failures_and_timeouts_keep_distinct_exit_codes_through_context() {
    for (code, exit) in [
        ("service_command_failed", 5),
        ("service_install_failed", 5),
        ("service_definition_conflict", 5),
        ("service_manager_unavailable", 5),
        ("service_timeout", 4),
        ("daemon_stop_timeout", 4),
        ("service_lock_timeout", 4),
    ] {
        let error = anyhow::Error::new(crate::platform::service::ServiceError {
            code: code.into(),
            message: "fixture failure".into(),
        })
        .context("operation failed");
        assert_eq!(super::error_code(&error), (code, exit));
    }
    let error = anyhow::Error::new(crate::client::ClientError {
        code: "daemon_unresponsive".into(),
        message: "owner exists".into(),
    });
    assert_eq!(super::error_code(&error), ("daemon_unresponsive", 5));
}
