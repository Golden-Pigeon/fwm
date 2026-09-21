//! Validated configuration changes shared by the daemon and offline CLI.
pub(crate) mod batch;
#[cfg(test)]
mod tests;

use fwm_api::protocol::{ApiError, Command, Selection};
use fwm_core::model::{Config, DesiredState};

pub struct Change {
    pub config: Config,
    pub selected: Option<Vec<String>>,
    pub message: String,
}

/// Build a candidate without touching storage, listeners, or the SSH engine.
pub fn prepare(before: &Config, command: &Command) -> Result<Change, ApiError> {
    let mut config = before.clone();
    let mut selected = None;
    let message = match command {
        Command::PutServer { server } => {
            if let Some(existing) = config.servers.iter_mut().find(|s| s.id == server.id) {
                *existing = server.clone();
            } else {
                config.servers.push(server.clone());
            }
            "server saved".into()
        }
        Command::RemoveServer { selector } => {
            let server = config
                .server(selector)
                .ok_or_else(|| ApiError::new("not_found", "server not found"))?;
            if config.forwards.iter().any(|f| f.server_id == server.id) {
                return Err(ApiError::new(
                    "server_in_use",
                    "remove this server's forwards before removing the server",
                ));
            }
            let id = server.id.clone();
            config.servers.retain(|s| s.id != id);
            "server removed".into()
        }
        Command::PutForward { forward } => {
            if let Some(existing) = config.forwards.iter_mut().find(|f| f.id == forward.id) {
                *existing = forward.clone();
            } else {
                config.forwards.push(forward.clone());
            }
            "forward saved".into()
        }
        Command::PutForwardWithServer { forward, server } => {
            batch::replace(&mut config, vec![forward.clone()], server.clone())?;
            "forward saved".into()
        }
        Command::PutForwardsWithServer { forwards, server } => {
            batch::replace(&mut config, forwards.clone(), server.clone())?;
            batch::saved_message(forwards)
        }
        Command::CreateForwards { forwards, server } => {
            batch::append_new(&mut config, forwards.clone(), server.clone())?;
            batch::saved_message(forwards)
        }
        Command::RemoveForward { selector } => {
            let ids = resolve(before, &Selection::Forward(selector.clone()))?;
            config.forwards.retain(|f| !ids.contains(&f.id));
            let message = format!("{} forward(s) removed", ids.len());
            selected = Some(ids);
            message
        }
        Command::RemoveForwards { selection } => {
            let ids = resolve(before, selection)?;
            config.forwards.retain(|f| !ids.contains(&f.id));
            let message = format!("{} forward(s) removed", ids.len());
            selected = Some(ids);
            message
        }
        Command::SetDesired { selection, .. } | Command::Restart { selection } => {
            let state = match command {
                Command::SetDesired { state, .. } => *state,
                _ => DesiredState::Running,
            };
            let ids = resolve(before, selection)?;
            for forward in &mut config.forwards {
                if ids.contains(&forward.id) {
                    forward.desired_state = state;
                }
            }
            let verb = if matches!(command, Command::Restart { .. }) {
                "Restart"
            } else if state == DesiredState::Stopped {
                "Stop"
            } else {
                "Start"
            };
            let message = format!("{verb} requested for {} forward(s)", ids.len());
            selected = Some(ids);
            message
        }
        _ => {
            return Err(ApiError::new(
                "invalid_request",
                "operation is not a configuration change",
            ));
        }
    };
    config
        .validate()
        .map_err(|e| ApiError::new("invalid_config", e))?;
    Ok(Change {
        config,
        selected,
        message,
    })
}

fn resolve(config: &Config, selection: &Selection) -> Result<Vec<String>, ApiError> {
    selection
        .resolve(config)
        .map_err(|e| ApiError::new("not_found", e))
}
