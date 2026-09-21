use super::probes::{doctor, doctor_profile, inspect, inspect_hop_profile, inspect_profile};
use super::state::State;
use fwm_api::protocol::{
    API_VERSION, ApiError, Command, Request, Response, StatusSnapshot, StatusView,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub async fn dispatch(
    state: Arc<Mutex<State>>,
    request: Request,
    stop: CancellationToken,
) -> Response {
    let request_id = request.request_id.clone();
    if request_id.is_empty() || request_id.len() > 128 {
        return Response::failure(
            request_id,
            ApiError::new("invalid_request", "request_id must contain 1–128 bytes"),
        );
    }
    if request.api_version != API_VERSION {
        return Response::failure(
            request_id,
            ApiError::new(
                "version_mismatch",
                format!("supported API version is {API_VERSION}"),
            ),
        );
    }
    // SSH probes run without holding the application lock so a dead server
    // cannot block queries, stop requests, or unrelated configuration updates.
    let result = match &request.command {
        Command::InspectHost { server } => inspect(state, server, None).await,
        Command::TrustHost {
            server,
            fingerprint,
        } => inspect(state, server, Some(fingerprint)).await,
        Command::Doctor { server } => doctor(state, server.as_deref()).await,
        Command::InspectHostProfile { server } => inspect_profile(state, server, None).await,
        Command::InspectHopProfile { server, hop } => {
            inspect_hop_profile(state, server, hop, None).await
        }
        Command::TrustHopProfile {
            server,
            hop,
            fingerprint,
        } => inspect_hop_profile(state, server, hop, Some(fingerprint)).await,
        Command::TrustHostProfile {
            server,
            fingerprint,
        } => inspect_profile(state, server, Some(fingerprint)).await,
        Command::DoctorProfile { server } => doctor_profile(state, server).await,
        _ => apply(&mut *state.lock().await, request, stop).await,
    };
    match result {
        Ok(value) => Response::success(request_id, value),
        Err(error) => Response::failure(request_id, error),
    }
}

async fn apply(
    state: &mut State,
    request: Request,
    stop: CancellationToken,
) -> Result<Value, ApiError> {
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return Err(ApiError::new(
            "invalid_request",
            "request_id must contain 1–128 bytes",
        ));
    }
    if request.command.mutates_config() {
        let signature = serde_json::to_string(&(&request.command, request.expected_revision))
            .map_err(|e| ApiError::new("invalid_request", e.to_string()))?;
        if let Some((_, previous_signature, reply)) = state
            .completed_requests
            .iter()
            .find(|(id, _, _)| *id == request.request_id)
        {
            return if *previous_signature == signature {
                Ok(reply.clone())
            } else {
                Err(ApiError::new(
                    "request_id_conflict",
                    "request_id was already used with different parameters",
                ))
            };
        }
        let id = request.request_id.clone();
        let reply = apply_inner(state, request, stop).await?;
        state
            .completed_requests
            .push_back((id, signature, reply.clone()));
        if state.completed_requests.len() > 32 {
            state.completed_requests.pop_front();
        }
        return Ok(reply);
    }
    apply_inner(state, request, stop).await
}

async fn apply_inner(
    state: &mut State,
    request: Request,
    stop: CancellationToken,
) -> Result<Value, ApiError> {
    if request.command.mutates_config()
        && request
            .expected_revision
            .is_some_and(|revision| revision != state.config.revision)
    {
        return Err(ApiError::new(
            "revision_conflict",
            format!("current revision is {}", state.config.revision),
        ));
    }
    let mut config = state.config.clone();
    match request.command {
        Command::Ping => Ok(
            json!({"daemon_instance_id":state.instance,"version":env!("CARGO_PKG_VERSION"),"capabilities":["atomic_alias_add","verified_remote_cleanup","cli_ux_v3","cli_ux_v4","cli_ux_v5","atomic_status_view"]}),
        ),
        Command::GetConfig => encode(&config),
        Command::Status | Command::StatusView { .. } => {
            let mut snapshot = StatusSnapshot {
                daemon_instance_id: state.instance.clone(),
                config_revision: config.revision,
                forwards: state.engine.snapshot().await,
            };
            if let Command::StatusView {
                selection: Some(selection),
            } = &request.command
            {
                let ids = state.select(selection)?;
                snapshot.forwards.retain(|rule| ids.contains(&rule.id));
            }
            let include_config = matches!(request.command, Command::StatusView { .. });
            bounded_status(config, snapshot, include_config)
        }
        Command::Events { after } => encode(&state.journal.since(after)),
        command @ (Command::PutServer { .. }
        | Command::RemoveServer { .. }
        | Command::PutForward { .. }
        | Command::PutForwardWithServer { .. }
        | Command::PutForwardsWithServer { .. }
        | Command::CreateForwards { .. }
        | Command::RemoveForward { .. }
        | Command::RemoveForwards { .. }
        | Command::SetDesired { .. }) => {
            let change = crate::configuration::prepare(&config, &command)?;
            let reply = if let Some(ids) = change.selected {
                state
                    .commit_control(change.config, &ids, &change.message)
                    .await?
            } else {
                state.commit(change.config, &change.message).await?
            };
            encode(&reply)
        }
        Command::Retry { selection } => {
            let ids = state.select(&selection)?;
            let report = state
                .engine
                .retry(&ids)
                .await
                .map_err(|e| ApiError::new("runtime_error", format!("{e:#}")))?;
            encode(&report)
        }
        Command::Restart { selection } => {
            let ids = state.select(&selection)?;
            for forward in &mut config.forwards {
                if ids.contains(&forward.id) {
                    forward.desired_state = fwm_core::model::DesiredState::Running;
                }
            }
            let mut reply = state
                .commit_control(config, &ids, "restart requested")
                .await?;
            reply.operation = Some(
                match &selection {
                    fwm_api::protocol::Selection::Server(selector) => {
                        state.engine.reconnect_server(&state.config, selector).await
                    }
                    _ => state.engine.restart(&ids).await,
                }
                .map_err(|error| ApiError::new("runtime_error", format!("{error:#}")))?,
            );
            encode(&reply)
        }
        Command::Reload => {
            let candidate = state
                .store
                .read_candidate()
                .map_err(|e| ApiError::new("invalid_config", format!("{e:#}")))?;
            let mut reply = state
                .commit_reloaded(candidate, "configuration reloaded")
                .await?;
            if let Err(error) = state.engine.refresh_ssh_config(&state.config).await {
                reply.message.push_str(&format!(
                    "; configuration saved but refreshing SSH sessions needs attention: {error:#}"
                ));
            }
            encode(&reply)
        }
        Command::Validate => {
            let candidate = state
                .store
                .read_candidate()
                .map_err(|e| ApiError::new("invalid_config", format!("{e:#}")))?;
            Ok(
                json!({"valid":true,"servers":candidate.servers.len(),"forwards":candidate.forwards.len()}),
            )
        }
        Command::Shutdown => {
            stop.cancel();
            Ok(json!({"message":"daemon shutdown requested"}))
        }
        _ => Err(ApiError::new("invalid_request", "unexpected request")),
    }
}

fn encode(value: &impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|e| ApiError::new("internal_error", e.to_string()))
}

fn bounded_status(
    config: fwm_core::model::Config,
    mut snapshot: StatusSnapshot,
    include_config: bool,
) -> Result<Value, ApiError> {
    // Leave room for the response envelope, including escaped request IDs.
    const BUDGET: usize = fwm_api::protocol::MAX_FRAME_BYTES - 2048;
    let mut limit = 4096;
    loop {
        let value = if include_config {
            encode(&StatusView {
                config: config.clone(),
                snapshot: snapshot.clone(),
            })?
        } else {
            encode(&snapshot)?
        };
        if serde_json::to_vec(&value)
            .map_err(|e| ApiError::new("internal_error", e.to_string()))?
            .len()
            <= BUDGET
        {
            return Ok(value);
        }
        if limit < 64 {
            return Err(ApiError::new(
                "response_too_large",
                "status metadata exceeds the IPC budget",
            ));
        }
        for rule in &mut snapshot.forwards {
            if let Some(error) = &mut rule.last_error
                && error.len() > limit
            {
                let mut end = limit;
                while !error.is_char_boundary(end) {
                    end -= 1;
                }
                error.truncate(end);
                error.push_str(" [truncated; query this rule alone for details]");
            }
        }
        limit /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fwm_api::protocol::Selection;
    use fwm_core::{model::DesiredState, paths::Paths};

    #[tokio::test]
    async fn repeated_mutation_id_returns_original_commit_without_reapplying() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let mut state = State::new(&paths).await.unwrap();
        let mut request = Request::new(
            "same-operation",
            Command::SetDesired {
                selection: Selection::All,
                state: DesiredState::Stopped,
            },
        );
        request.expected_revision = Some(0);
        let reply = apply(&mut state, request.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(state.config.revision, 1);
        assert_eq!(
            apply(&mut state, request.clone(), CancellationToken::new())
                .await
                .unwrap(),
            reply
        );
        assert_eq!(state.config.revision, 1);
        request.command = Command::SetDesired {
            selection: Selection::All,
            state: DesiredState::Running,
        };
        assert_eq!(
            apply(&mut state, request, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "request_id_conflict"
        );
    }

    #[tokio::test]
    async fn stale_revision_is_rejected_without_persisting() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(temp.path().to_owned())).unwrap();
        let mut state = State::new(&paths).await.unwrap();
        let mut request = Request::new(
            "outdated",
            Command::SetDesired {
                selection: Selection::All,
                state: DesiredState::Stopped,
            },
        );
        request.expected_revision = Some(999);
        assert_eq!(
            apply(&mut state, request, CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "revision_conflict"
        );
        assert_eq!(state.store.load().unwrap().config.revision, 0);
    }
}

#[cfg(test)]
#[path = "postfix_tests.rs"]
mod postfix_tests;
