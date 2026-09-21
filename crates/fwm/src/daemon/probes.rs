//! Online probes share SSH work with the CLI and resume blocked trust failures.
use super::state::State;
use crate::ssh_actions;
use fwm_api::protocol::ApiError;
use fwm_core::{
    model::{DesiredState, ForwardStatus, RuntimeState, ServerProfile},
    ssh::HostKeyInfo,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(super) async fn inspect(
    state: Arc<Mutex<State>>,
    selector: &str,
    fingerprint: Option<&String>,
) -> Result<Value, ApiError> {
    let profile = state
        .lock()
        .await
        .config
        .server(selector)
        .ok_or_else(|| ApiError::new("not_found", "server not found"))?
        .clone();
    inspect_profile(state, &profile, fingerprint).await
}

pub(super) async fn inspect_profile(
    state: Arc<Mutex<State>>,
    profile: &ServerProfile,
    fingerprint: Option<&String>,
) -> Result<Value, ApiError> {
    inspect_at(state, profile, fingerprint, None).await
}

pub(super) async fn inspect_hop_profile(
    state: Arc<Mutex<State>>,
    profile: &ServerProfile,
    hop: &str,
    fingerprint: Option<&String>,
) -> Result<Value, ApiError> {
    inspect_at(state, profile, fingerprint, Some(hop)).await
}

async fn inspect_at(
    state: Arc<Mutex<State>>,
    profile: &ServerProfile,
    fingerprint: Option<&String>,
    hop: Option<&str>,
) -> Result<Value, ApiError> {
    let (policy, saved) = {
        let state = state.lock().await;
        (
            state.config.defaults.retry.clone(),
            state.config.server(&profile.id).cloned(),
        )
    };
    let mut info = if let Some(hop) = hop {
        ssh_actions::inspect_hop(profile, &policy, hop).await?
    } else {
        ssh_actions::inspect(profile, &policy).await?
    };
    if let Some(fingerprint) = fingerprint {
        let mut state = state.lock().await;
        if state.config.server(&profile.id) != saved.as_ref()
            || saved.as_ref().is_some_and(|saved| saved != profile)
        {
            return Err(ApiError::new(
                "revision_conflict",
                "server changed during host inspection; repeat the trust command",
            ));
        }
        ssh_actions::trust(&mut info, fingerprint)?;
        let related: Vec<_> = state
            .config
            .servers
            .iter()
            .filter(|server| server.id == profile.id || shares_trust_identity(server, &info))
            .map(|server| server.id.as_str())
            .collect();
        let ids: Vec<_> = state
            .engine
            .snapshot()
            .await
            .into_iter()
            .filter(|forward| {
                state
                    .config
                    .forward(&forward.id)
                    .is_some_and(|rule| related.contains(&rule.server_id.as_str()))
                    && trust_blocked(forward, &info)
            })
            .map(|forward| forward.id)
            .collect();
        if !ids.is_empty() {
            state
                .engine
                .retry(&ids)
                .await
                .map_err(|error| ApiError::new("runtime_error", error.to_string()))?;
        }
        state.journal.set_server(profile);
        state.journal.record_server(
            profile.id.clone(),
            format!(
                "host key explicitly trusted for {}; resumed {} blocked forward(s)",
                profile.name,
                ids.len()
            ),
        );
    }
    serde_json::to_value(info).map_err(|error| ApiError::new("internal_error", error.to_string()))
}

fn shares_trust_identity(profile: &ServerProfile, info: &HostKeyInfo) -> bool {
    fwm_core::ssh::resolved_route(profile).is_ok_and(|route| {
        route.iter().any(|server| {
            server.trust_host() == info.host
                && server.port == info.port
                && server.known_hosts == info.known_hosts
        })
    })
}

fn trust_blocked(forward: &ForwardStatus, info: &HostKeyInfo) -> bool {
    forward.desired_state == DesiredState::Running
        && forward.state == RuntimeState::NeedsAttention
        && forward.last_error.as_deref().is_some_and(|error| {
            error.contains(&format!("unknown host key for {}: ", info.host))
                || error.contains(&format!("HOST KEY CHANGED for {}: ", info.host))
                || error.ends_with(&format!("revoked host key for {}", info.host))
        })
}

pub(super) async fn doctor(
    state: Arc<Mutex<State>>,
    selector: Option<&str>,
) -> Result<Value, ApiError> {
    let (profiles, policy, configuration) = {
        let state = state.lock().await;
        let profiles = if let Some(selector) = selector {
            vec![
                state
                    .config
                    .server(selector)
                    .ok_or_else(|| ApiError::new("not_found", "server not found"))?
                    .clone(),
            ]
        } else {
            state.config.servers.clone()
        };
        (
            profiles,
            state.config.defaults.retry.clone(),
            ssh_actions::configuration(&state.store, &state.config),
        )
    };
    ssh_actions::doctor(profiles, policy, configuration, "daemon").await
}

pub(super) async fn doctor_profile(
    state: Arc<Mutex<State>>,
    profile: &ServerProfile,
) -> Result<Value, ApiError> {
    ssh_actions::validate_profile(profile)?;
    let (policy, configuration) = {
        let state = state.lock().await;
        (
            state.config.defaults.retry.clone(),
            ssh_actions::configuration(&state.store, &state.config),
        )
    };
    ssh_actions::doctor(vec![profile.clone()], policy, configuration, "daemon").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_recovery_does_not_resume_stopped_healthy_or_other_attention_failures() {
        let info = HostKeyInfo {
            host: "fixture.invalid".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:fixture".into(),
            public_key: String::new(),
            known_hosts: "known_hosts".into(),
            global_known_hosts: vec![],
            status: "trusted".into(),
        };
        let mut forward = ForwardStatus {
            id: "forward".into(),
            name: "forward".into(),
            group: None,
            server: "fixture".into(),
            kind: "local".into(),
            listen: "127.0.0.1:3000".into(),
            target: Some("localhost:3000".into()),
            desired_state: DesiredState::Running,
            state: RuntimeState::NeedsAttention,
            retry_count: 0,
            next_retry_unix_ms: None,
            last_error: Some("unknown host key for fixture.invalid: SHA256:fixture; inspect and explicitly trust this key first".into()),
            active_connections: 0,
        };
        assert!(trust_blocked(&forward, &info));
        forward.desired_state = DesiredState::Stopped;
        assert!(!trust_blocked(&forward, &info));
        forward.desired_state = DesiredState::Running;
        forward.state = RuntimeState::Established;
        assert!(!trust_blocked(&forward, &info));
        forward.state = RuntimeState::NeedsAttention;
        for error in [
            "SSH authentication requires attention: fixture.invalid rejected the key",
            "unknown host key for jump.invalid: SHA256:other",
            "unknown host key for fixture.invalid.other: SHA256:other",
            "SSH configuration: invalid identity for fixture.invalid",
        ] {
            forward.last_error = Some(error.into());
            assert!(!trust_blocked(&forward, &info), "{error}");
        }
    }

    #[test]
    fn aliases_match_only_the_same_host_key_identity_port_and_trust_database() {
        let temp = tempfile::tempdir().unwrap();
        let ssh_config = temp.path().join("ssh_config");
        let known_hosts = temp.path().join("known_hosts");
        std::fs::write(
            &ssh_config,
            "Host alias\n HostName 127.0.0.1\n HostKeyAlias key-name\n Port 2202\n User fixture\n",
        )
        .unwrap();
        let mut profile = ServerProfile::new("saved-name");
        profile.ssh_alias = Some("alias".into());
        profile.ssh_config = Some(ssh_config);
        profile.known_hosts = Some(known_hosts.clone());
        let mut info = HostKeyInfo {
            host: "key-name".into(),
            port: 2202,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:fixture".into(),
            public_key: String::new(),
            known_hosts,
            global_known_hosts: vec![],
            status: "trusted".into(),
        };
        assert!(shares_trust_identity(&profile, &info));
        info.host = "127.0.0.1".into();
        assert!(!shares_trust_identity(&profile, &info));
        info.host = "key-name".into();
        info.port = 22;
        assert!(!shares_trust_identity(&profile, &info));
        info.port = 2202;
        info.known_hosts = temp.path().join("other-known-hosts");
        assert!(!shares_trust_identity(&profile, &info));
    }
}
