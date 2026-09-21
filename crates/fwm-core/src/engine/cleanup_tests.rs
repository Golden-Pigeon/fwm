use super::*;
use crate::model::{ForwardSpec, RemoteCleanup, Tunnel};

fn fixture() -> Config {
    let mut server = ServerProfile::new("test");
    server.host = Some("127.0.0.1".into());
    let forwards = (0..4)
        .map(|index| ForwardSpec {
            id: format!("rule-{index}"),
            name: format!("forward-{index}"),
            group: None,
            server_id: server.id.clone(),
            tunnel: Tunnel::Remote {
                listen: ([127, 0, 0, 1], 12000 + index).into(),
                target: "localhost:22".parse().unwrap(),
            },
            desired_state: DesiredState::Stopped,
            // Older/manual configs may still request shared mode. Cleanup must
            // enforce session isolation regardless of that stored preference.
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: if index < 2 {
                RemoteCleanup::Verified
            } else {
                RemoteCleanup::Off
            },
        })
        .collect();
    Config {
        servers: vec![server],
        forwards,
        ..Config::default()
    }
}

#[tokio::test]
async fn verified_rules_always_have_independent_connection_identity() {
    let config = fixture();
    let mut engine = Engine::new();
    engine.reconcile(&config).await.unwrap();
    let keys: Vec<_> = {
        let statuses = engine.statuses.lock().unwrap();
        config
            .forwards
            .iter()
            .map(|forward| statuses[&forward.id].connection_key.clone())
            .collect()
    };
    assert_ne!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert_ne!(keys[1], keys[2]);
    assert_eq!(keys[2], keys[3]);
    assert_eq!(engine.groups.len(), 3);
    engine.shutdown().await;
}

#[tokio::test]
async fn missing_identity_is_actionable_before_ssh_and_does_not_cancel_siblings() {
    let mut config = fixture();
    config.forwards[0].desired_state = DesiredState::Running;
    let mut engine = Engine::new();
    let mut events = engine.subscribe();
    engine.reconcile(&config).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.forward_id.as_deref() == Some("rule-0") {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert!(event.message.contains("NeedsAttention"));
    assert!(event.message.contains("persistent manager identity"));
    let statuses = engine.snapshot().await;
    assert_eq!(statuses[0].state, RuntimeState::NeedsAttention);
    assert!(statuses[0].next_retry_unix_ms.is_none());
    assert!(
        statuses[1..]
            .iter()
            .all(|status| status.state == RuntimeState::Stopped)
    );

    let (failed_key, sibling_key, sibling_generation) = {
        let statuses = engine.statuses.lock().unwrap();
        (
            statuses["rule-0"].connection_key.clone(),
            statuses["rule-2"].connection_key.clone(),
            statuses["rule-2"].generation,
        )
    };
    let failed_cancel = engine.groups[&failed_key].cancel.clone();
    let sibling_cancel = engine.groups[&sibling_key].cancel.clone();
    config.forwards.remove(0);
    engine.reconcile(&config).await.unwrap();
    assert!(failed_cancel.is_cancelled());
    assert!(!sibling_cancel.is_cancelled());
    assert_eq!(
        engine.statuses.lock().unwrap()["rule-2"].generation,
        sibling_generation
    );
    engine.shutdown().await;
}
