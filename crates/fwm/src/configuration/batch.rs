//! Batch creation has one commit point and cannot overwrite existing rules.
use fwm_api::protocol::ApiError;
use fwm_core::model::{Config, ForwardSpec, ServerProfile};

pub(super) fn append_new(
    config: &mut Config,
    forwards: Vec<ForwardSpec>,
    server: Option<ServerProfile>,
) -> Result<(), ApiError> {
    if forwards.is_empty() {
        return Err(ApiError::new(
            "invalid_request",
            "a batch must contain at least one forward",
        ));
    }
    if let Some(server) = server {
        let references = forwards
            .iter()
            .map(|forward| forward.server_id.clone())
            .collect::<Vec<_>>();
        insert_server(config, server, &references)?;
    }
    for forward in &forwards {
        if let Some(existing) = config
            .forwards
            .iter()
            .find(|existing| existing.id == forward.id || existing.name == forward.name)
        {
            return Err(ApiError::new(
                "already_exists",
                format!(
                    "forward {} already exists; edit it explicitly",
                    existing.name
                ),
            ));
        }
    }
    // The caller owns this candidate clone. The entire resulting config is
    // validated before it can reach either durable storage or the engine.
    config.forwards.extend(forwards);
    config
        .validate()
        .map_err(|error| ApiError::new("invalid_config", error))
}

pub(super) fn insert_server(
    config: &mut Config,
    server: ServerProfile,
    references: &[String],
) -> Result<(), ApiError> {
    if let Some(existing) = config
        .servers
        .iter()
        .find(|existing| existing.id == server.id || existing.name == server.name)
    {
        return Err(ApiError::new(
            "already_exists",
            format!(
                "server {} already exists; reload the configuration and retry",
                existing.name
            ),
        ));
    }
    if !references.contains(&server.id) {
        return Err(ApiError::new(
            "invalid_request",
            "a new server must be used by at least one selected forward",
        ));
    }
    config.servers.push(server);
    Ok(())
}

pub(super) fn replace(
    config: &mut Config,
    forwards: Vec<ForwardSpec>,
    server: Option<ServerProfile>,
) -> Result<(), ApiError> {
    if forwards.is_empty() {
        return Err(ApiError::new(
            "invalid_request",
            "an edit must contain at least one forward",
        ));
    }
    let mut ids = std::collections::HashSet::new();
    for forward in &forwards {
        if !ids.insert(&forward.id) {
            return Err(ApiError::new(
                "invalid_request",
                "an edit contains the same forward more than once",
            ));
        }
        if !config
            .forwards
            .iter()
            .any(|existing| existing.id == forward.id)
        {
            return Err(ApiError::new(
                "not_found",
                format!("forward {} no longer exists", forward.name),
            ));
        }
    }
    if let Some(server) = server {
        let references = forwards
            .iter()
            .map(|forward| forward.server_id.clone())
            .collect::<Vec<_>>();
        insert_server(config, server, &references)?;
    }
    for forward in forwards {
        let existing = config
            .forwards
            .iter_mut()
            .find(|existing| existing.id == forward.id)
            .unwrap();
        *existing = forward;
    }
    config
        .validate()
        .map_err(|error| ApiError::new("invalid_config", error))
}

pub(super) fn saved_message(forwards: &[ForwardSpec]) -> String {
    let running = forwards
        .iter()
        .filter(|forward| forward.desired_state == fwm_core::model::DesiredState::Running)
        .count();
    format!(
        "{} forward(s) saved (running: {running}, stopped: {})",
        forwards.len(),
        forwards.len() - running
    )
}
