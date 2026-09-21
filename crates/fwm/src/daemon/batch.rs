use fwm_core::model::ForwardSpec;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{dispatch::dispatch, state::State};
    use fwm_api::protocol::{Command, Request, Response};
    use fwm_core::{
        model::{ConnectionMode, DesiredState, RemoteCleanup, ServerProfile, Tunnel, new_id},
        paths::Paths,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    pub(super) struct Fixture {
        pub(super) _directory: tempfile::TempDir,
        pub(super) state: Arc<Mutex<State>>,
        server: String,
    }

    impl Fixture {
        pub(super) async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
            let mut state = State::new(&paths).await.unwrap();
            let mut config = state.config.clone();
            let mut server = ServerProfile::new("test");
            server.host = Some("127.0.0.1".into());
            config.servers.push(server.clone());
            state.commit(config, "test setup").await.unwrap();
            Self {
                _directory: directory,
                state: Arc::new(Mutex::new(state)),
                server: server.id,
            }
        }

        pub(super) fn rule(&self, name: &str, port: u16) -> ForwardSpec {
            ForwardSpec {
                id: new_id(),
                name: name.into(),
                group: None,
                server_id: self.server.clone(),
                tunnel: Tunnel::Local {
                    listen: ([127, 0, 0, 1], port).into(),
                    target: format!("localhost:{port}").parse().unwrap(),
                },
                desired_state: DesiredState::Stopped,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            }
        }

        pub(super) async fn send(&self, request: Request) -> Response {
            dispatch(self.state.clone(), request, CancellationToken::new()).await
        }
    }

    #[tokio::test]
    async fn batch_commits_once_and_retry_does_not_duplicate_rules() {
        let fixture = Fixture::new().await;
        let mut request = Request::new(
            "batch",
            Command::CreateForwards {
                forwards: vec![
                    fixture.rule("web-3000", 3000),
                    fixture.rule("web-3001", 3001),
                ],
                server: None,
            },
        );
        request.expected_revision = Some(1);
        let reply = fixture.send(request.clone()).await;
        assert!(reply.ok, "{reply:?}");
        let repeated = fixture.send(request).await;
        assert!(repeated.ok, "{repeated:?}");
        assert_eq!(reply.data, repeated.data);
        let mut state = fixture.state.lock().await;
        assert_eq!(state.config.revision, 2);
        assert_eq!(state.config.forwards.len(), 2);
        assert_eq!(state.store.load().unwrap().config, state.config);
        state.engine.shutdown().await;
    }

    #[tokio::test]
    async fn bad_last_member_or_internal_conflict_never_partially_commits() {
        let fixture = Fixture::new().await;
        let before = fixture.state.lock().await.config.clone();
        let valid = fixture.rule("valid", 3000);
        let mut invalid = fixture.rule("invalid", 3001);
        invalid.server_id = "missing".into();
        let mut a = fixture.rule("a", 4000);
        let mut b = fixture.rule("b", 4000);
        a.desired_state = DesiredState::Running;
        b.desired_state = DesiredState::Running;
        for forwards in [
            vec![valid.clone(), invalid],
            vec![a, b],
            vec![valid.clone(), valid],
            vec![],
        ] {
            let reply = fixture
                .send(Request::new(
                    new_id(),
                    Command::CreateForwards {
                        forwards,
                        server: None,
                    },
                ))
                .await;
            assert!(!reply.ok);
            let state = fixture.state.lock().await;
            assert_eq!(state.config, before);
            assert_eq!(state.store.load().unwrap().config, before);
            assert!(state.engine.snapshot().await.is_empty());
        }
    }

    #[tokio::test]
    async fn existing_rule_is_not_overwritten_even_when_another_member_is_new() {
        let fixture = Fixture::new().await;
        let existing = fixture.rule("existing", 5000);
        let created = fixture
            .send(Request::new(
                "first",
                Command::CreateForwards {
                    forwards: vec![existing.clone()],
                    server: None,
                },
            ))
            .await;
        assert!(created.ok);
        let before = fixture.state.lock().await.config.clone();
        let mut overwrite = existing;
        overwrite.tunnel = Tunnel::Dynamic {
            listen: ([127, 0, 0, 1], 6000).into(),
        };
        let reply = fixture
            .send(Request::new(
                "second",
                Command::CreateForwards {
                    forwards: vec![fixture.rule("new", 6001), overwrite],
                    server: None,
                },
            ))
            .await;
        assert_eq!(reply.error.unwrap().code, "already_exists");
        let mut state = fixture.state.lock().await;
        assert_eq!(state.config, before);
        assert_eq!(state.store.load().unwrap().config, before);
        state.engine.shutdown().await;
    }

    #[tokio::test]
    async fn alias_server_and_rules_commit_once_and_retries_are_idempotent() {
        let fixture = Fixture::new().await;
        let mut server = ServerProfile::new("example-cluster");
        server.ssh_alias = Some("example-cluster".into());
        let mut forwards = vec![fixture.rule("ssh", 12222), fixture.rule("web", 8080)];
        for forward in &mut forwards {
            forward.server_id = server.id.clone();
        }
        let mut request = Request::new(
            "alias-batch",
            Command::CreateForwards {
                forwards: forwards.clone(),
                server: Some(server.clone()),
            },
        );
        request.expected_revision = Some(1);
        let reply = fixture.send(request.clone()).await;
        assert!(reply.ok, "{reply:?}");
        let repeated = fixture.send(request.clone()).await;
        assert!(repeated.ok, "{repeated:?}");
        assert_eq!(reply.data, repeated.data);

        // Reusing an id with a different profile cannot change the saved result.
        let Command::CreateForwards {
            server: Some(profile),
            ..
        } = &mut request.command
        else {
            unreachable!()
        };
        profile.ssh_alias = Some("other-server".into());
        let changed = fixture.send(request).await;
        assert_eq!(changed.error.unwrap().code, "request_id_conflict");

        let mut state = fixture.state.lock().await;
        assert_eq!(state.config.revision, 2);
        assert_eq!(state.config.servers.len(), 2);
        assert_eq!(state.config.server(&server.id), Some(&server));
        assert_eq!(state.config.forwards, forwards);
        assert_eq!(state.store.load().unwrap().config, state.config);
        state.engine.shutdown().await;
    }

    #[tokio::test]
    async fn invalid_batch_or_orphan_never_persists_the_new_server() {
        let fixture = Fixture::new().await;
        let before = fixture.state.lock().await.config.clone();
        let mut server = ServerProfile::new("new-server");
        server.ssh_alias = Some("new-server".into());
        let mut valid = fixture.rule("valid", 3000);
        valid.server_id = server.id.clone();
        let mut invalid = fixture.rule("invalid", 3001);
        invalid.server_id = "missing-server".into();
        for forwards in [
            vec![valid, invalid],
            vec![fixture.rule("unrelated", 3002)],
            vec![],
        ] {
            let reply = fixture
                .send(Request::new(
                    new_id(),
                    Command::CreateForwards {
                        forwards,
                        server: Some(server.clone()),
                    },
                ))
                .await;
            assert!(!reply.ok);
            let state = fixture.state.lock().await;
            assert_eq!(state.config, before);
            assert_eq!(state.store.load().unwrap().config, before);
            assert!(state.engine.snapshot().await.is_empty());
        }
    }

    #[tokio::test]
    async fn alias_add_cannot_overwrite_existing_server_id_or_name() {
        let fixture = Fixture::new().await;
        let before = fixture.state.lock().await.config.clone();
        let existing = before.servers[0].clone();
        let mut name_collision = ServerProfile::new(existing.name.clone());
        name_collision.ssh_alias = Some("replacement".into());
        let mut id_collision = ServerProfile::new("different-name");
        id_collision.id = existing.id.clone();
        id_collision.ssh_alias = Some("replacement".into());
        for server in [name_collision, id_collision] {
            let mut forward = fixture.rule("new-rule", 3000);
            forward.server_id = server.id.clone();
            let reply = fixture
                .send(Request::new(
                    new_id(),
                    Command::CreateForwards {
                        forwards: vec![forward],
                        server: Some(server),
                    },
                ))
                .await;
            assert_eq!(reply.error.unwrap().code, "already_exists");
            let state = fixture.state.lock().await;
            assert_eq!(state.config, before);
            assert_eq!(state.store.load().unwrap().config, before);
        }
    }

    #[tokio::test]
    async fn stale_alias_add_returns_revision_conflict_without_changes() {
        let fixture = Fixture::new().await;
        let before = fixture.state.lock().await.config.clone();
        let mut server = ServerProfile::new("new-server");
        server.ssh_alias = Some("new-server".into());
        let mut forward = fixture.rule("ssh", 12222);
        forward.server_id = server.id.clone();
        let mut request = Request::new(
            "stale-alias-batch",
            Command::CreateForwards {
                forwards: vec![forward],
                server: Some(server),
            },
        );
        request.expected_revision = Some(before.revision - 1);
        let reply = fixture.send(request).await;
        assert_eq!(reply.error.unwrap().code, "revision_conflict");
        let state = fixture.state.lock().await;
        assert_eq!(state.config, before);
        assert_eq!(state.store.load().unwrap().config, before);
    }

    #[tokio::test]
    async fn ping_advertises_atomic_alias_add() {
        let fixture = Fixture::new().await;
        let reply = fixture.send(Request::new("ping", Command::Ping)).await;
        assert!(reply.ok);
        let capabilities = reply.data["capabilities"].as_array().unwrap();
        assert!(capabilities.contains(&serde_json::json!("atomic_alias_add")));
        assert!(capabilities.contains(&serde_json::json!("verified_remote_cleanup")));
    }
}

#[cfg(test)]
#[path = "mutation_tests.rs"]
mod mutation_tests;
