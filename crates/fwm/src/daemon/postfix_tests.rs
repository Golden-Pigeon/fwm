use super::*;
use fwm_core::model::{Config, DesiredState, ForwardStatus, RuntimeState};

#[tokio::test]
async fn every_probe_rejects_invalid_request_ids_before_execution() {
    let temp = tempfile::tempdir().unwrap();
    let paths = fwm_core::paths::Paths::new(Some(temp.path().to_owned())).unwrap();
    let state = Arc::new(Mutex::new(State::new(&paths).await.unwrap()));
    let profile = fwm_core::model::ServerProfile::new("missing");
    let commands = vec![
        Command::Ping,
        Command::Doctor { server: None },
        Command::InspectHost {
            server: "missing".into(),
        },
        Command::TrustHost {
            server: "missing".into(),
            fingerprint: "unused".into(),
        },
        Command::InspectHostProfile {
            server: profile.clone(),
        },
        Command::DoctorProfile {
            server: profile.clone(),
        },
        Command::TrustHostProfile {
            server: profile.clone(),
            fingerprint: "unused".into(),
        },
        Command::InspectHopProfile {
            server: profile.clone(),
            hop: "unused".into(),
        },
        Command::TrustHopProfile {
            server: profile,
            hop: "unused".into(),
            fingerprint: "unused".into(),
        },
    ];
    for command in commands {
        for id in [String::new(), "x".repeat(129)] {
            let reply = dispatch(
                state.clone(),
                Request::new(id, command.clone()),
                CancellationToken::new(),
            )
            .await;
            assert_eq!(reply.error.unwrap().code, "invalid_request");
        }
    }
    for id in ["x".into(), "x".repeat(128)] {
        assert!(
            dispatch(
                state.clone(),
                Request::new(id, Command::Ping),
                CancellationToken::new()
            )
            .await
            .ok
        );
    }
}

fn snapshot(count: usize, error: &str) -> StatusSnapshot {
    StatusSnapshot {
        daemon_instance_id: "test".into(),
        config_revision: 3,
        forwards: (0..count)
            .map(|i| ForwardStatus {
                id: format!("rule-{i}"),
                name: format!("web-{i}"),
                group: None,
                server: "server".into(),
                kind: "local".into(),
                listen: "127.0.0.1:8080".into(),
                target: None,
                desired_state: DesiredState::Running,
                state: RuntimeState::NeedsAttention,
                retry_count: 0,
                next_retry_unix_ms: None,
                last_error: Some(error.into()),
                active_connections: 0,
            })
            .collect(),
    }
}

#[test]
fn maximum_status_stays_in_frame_and_individual_query_keeps_details() {
    for error in ["界".repeat(2700), "\u{0001}".repeat(8100)] {
        for include_config in [false, true] {
            let value =
                bounded_status(Config::default(), snapshot(512, &error), include_config).unwrap();
            let response = Response::success("\u{0001}".repeat(128), value.clone());
            assert!(
                serde_json::to_vec(&response).unwrap().len() < fwm_api::protocol::MAX_FRAME_BYTES
            );
            let forwards = if include_config {
                &value["snapshot"]["forwards"]
            } else {
                &value["forwards"]
            };
            assert_eq!(forwards.as_array().unwrap().len(), 512);
            assert!(
                forwards[0]["last_error"]
                    .as_str()
                    .unwrap()
                    .contains("truncated")
            );
        }
        let single = bounded_status(Config::default(), snapshot(1, &error), true).unwrap();
        assert_eq!(single["snapshot"]["forwards"][0]["last_error"], error);
    }
}

#[tokio::test]
async fn status_view_returns_configuration_from_the_snapshot_revision() {
    let temp = tempfile::tempdir().unwrap();
    let paths = fwm_core::paths::Paths::new(Some(temp.path().to_owned())).unwrap();
    let state = Arc::new(Mutex::new(State::new(&paths).await.unwrap()));
    state.lock().await.config.revision = 42;
    let reply = dispatch(
        state,
        Request::new("view", Command::StatusView { selection: None }),
        CancellationToken::new(),
    )
    .await;
    assert!(reply.ok);
    let view: StatusView = serde_json::from_value(reply.data).unwrap();
    assert_eq!(view.config.revision, 42);
    assert_eq!(view.snapshot.config_revision, 42);
}

#[test]
fn configuration_rejects_ids_that_cannot_fit_history_metadata() {
    let mut server = fwm_core::model::ServerProfile::new("dev");
    server.host = Some("127.0.0.1".into());
    server.id = "x".repeat(128);
    let mut config = Config {
        servers: vec![server],
        ..Default::default()
    };
    config.validate().unwrap();
    config.servers[0].id.push('x');
    assert!(config.validate().unwrap_err().contains("1–128"));
}

#[test]
fn maximum_legal_config_and_runtime_are_bounded_together() {
    use fwm_core::model::{ConnectionMode, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel};
    let mut server = ServerProfile::new("s".repeat(100));
    server.id = "server".into();
    server.host = Some("127.0.0.1".into());
    let mut config = Config {
        revision: 3,
        servers: vec![server],
        ..Default::default()
    };
    for i in 0..512 {
        config.forwards.push(ForwardSpec {
            id: format!("{i:0128}"),
            name: format!("{i:0100}"),
            server_id: "server".into(),
            group: Some("g".repeat(80)),
            tunnel: Tunnel::Local {
                listen: format!("127.0.0.1:{}", 10000 + i).parse().unwrap(),
                target: "localhost:8080".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        });
    }
    config.validate().unwrap();
    assert!(serde_json::to_vec(&config).unwrap().len() > 220 * 1024);
    let mut statuses = snapshot(512, &"\u{0001}".repeat(8192));
    for (status, rule) in statuses.forwards.iter_mut().zip(&config.forwards) {
        status.id.clone_from(&rule.id);
        status.name.clone_from(&rule.name);
        status.server.clone_from(&config.servers[0].name);
    }
    let result = bounded_status(config, statuses, true).unwrap();
    let response = Response::success("\u{0001}".repeat(128), result);
    assert!(serde_json::to_vec(&response).unwrap().len() <= fwm_api::protocol::MAX_FRAME_BYTES);
}
