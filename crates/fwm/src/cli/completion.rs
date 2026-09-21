//! One final result per non-streaming CLI command, including readiness failures.
use super::output;
use crate::client;
use anyhow::Result;
use fwm_api::protocol::{Command, MutationReply, Response, StatusSnapshot};
use fwm_core::{
    model::{DesiredState, RuntimeState},
    paths::Paths,
};
use serde_json::{Value, json};
use std::{fmt, time::Duration};

#[derive(Debug)]
pub struct CompletionError {
    pub code: String,
    pub message: String,
    pub result: Value,
}
impl fmt::Display for CompletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}
impl std::error::Error for CompletionError {}

/// Starting is deliberately after a successful validated commit. Preserve that
/// fact in the final response if launching the daemon fails.
pub async fn start_saved(paths: &Paths, mut response: Response) -> Result<Response> {
    if let Err(error) = client::ensure_running(paths).await {
        let message = format!("Configuration is saved, but the daemon could not start: {error:#}");
        return Err(CompletionError {
            code: "daemon_unavailable".into(),
            result: json!({"ok":false,"error":{"code":"daemon_unavailable","message":message},
                "data":{"saved":true,"ready":false,"revision":response.data["revision"],"config":response.data["config"]}}),
            message,
        }.into());
    }
    if let Some(message) = response.data["message"].as_str() {
        response.data["message"] =
            json!(message.replace("; daemon remains stopped.", "; daemon is running."));
    }
    Ok(response)
}

pub async fn mutation(
    paths: &Paths,
    mut response: Response,
    ids: &[String],
    wait: Option<Duration>,
    json_output: bool,
) -> Result<MutationReply> {
    let reply: MutationReply = serde_json::from_value(response.data.clone())?;
    response.data["saved"] = json!(true);
    let stopped = !ids.is_empty()
        && ids.iter().all(|id| {
            reply
                .config
                .forward(id)
                .is_some_and(|rule| rule.desired_state == DesiredState::Stopped)
        });
    response.data["state"] = json!(if stopped { "stopped" } else { "saved" });
    response.data["ready"] = Value::Null;
    let Some(timeout) = wait else {
        return output::mutation(response, json_output);
    };
    match wait_ready(paths, ids, timeout).await {
        Ok(snapshot) => {
            response.data["state"] = json!("established");
            response.data["ready"] = json!(true);
            response.data["runtime"] = serde_json::to_value(&snapshot)?;
            if json_output {
                output::response(response, true)?;
            } else {
                println!("{} forward(s) ready.", ids.len());
                output::status(&snapshot, true, false)?;
            }
            Ok(reply)
        }
        Err(failure) => {
            let message = format!(
                "{} Configuration is saved; background recovery continues for enabled rules.",
                failure.message
            );
            let result = json!({"ok":false,"error":{"code":failure.code,"message":message},
                "data":{"saved":true,"ready":false,"revision":reply.revision,"runtime":failure.snapshot}});
            if !json_output && let Some(snapshot) = failure.snapshot {
                output::status(&snapshot, true, false)?;
            }
            Err(CompletionError {
                code: failure.code,
                message,
                result,
            }
            .into())
        }
    }
}

struct WaitFailure {
    code: String,
    message: String,
    snapshot: Option<StatusSnapshot>,
}

async fn wait_ready(
    paths: &Paths,
    ids: &[String],
    timeout: Duration,
) -> std::result::Result<StatusSnapshot, WaitFailure> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(wait_timeout(last));
        }
        let response = match tokio::time::timeout_at(
            deadline,
            client::request(paths, Command::Status),
        )
        .await
        {
            Err(_) => return Err(wait_timeout(last)),
            Ok(Err(error)) => {
                return Err(WaitFailure {
                    code: "wait_failed".into(),
                    message: format!("Could not query readiness: {error:#}"),
                    snapshot: last,
                });
            }
            Ok(Ok(response)) => response,
        };
        let mut snapshot: StatusSnapshot =
            client::decode(response).map_err(|error| WaitFailure {
                code: "wait_failed".into(),
                message: error.to_string(),
                snapshot: last.clone(),
            })?;
        snapshot.forwards.retain(|rule| ids.contains(&rule.id));
        if snapshot.forwards.len() == ids.len()
            && snapshot
                .forwards
                .iter()
                .all(|rule| rule.state == RuntimeState::Established)
        {
            return Ok(snapshot);
        }
        if snapshot
            .forwards
            .iter()
            .any(|rule| rule.state == RuntimeState::NeedsAttention)
        {
            return Err(WaitFailure {
                code: "needs_attention".into(),
                message:
                    "A selected forward needs authentication, trust, or configuration changes."
                        .into(),
                snapshot: Some(snapshot),
            });
        }
        last = Some(snapshot);
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
        )
        .await;
    }
}

fn wait_timeout(snapshot: Option<StatusSnapshot>) -> WaitFailure {
    WaitFailure {
        code: "wait_timeout".into(),
        message: "Timed out waiting for all selected listeners.".into(),
        snapshot,
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
