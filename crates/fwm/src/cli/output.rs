use anyhow::Result;
use fwm_api::protocol::{MutationReply, Response, StatusSnapshot};
use fwm_core::model::{Config, DesiredState, ForwardStatus, RuntimeState, unix_ms};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

pub fn json_value(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub fn response(response: Response, json_output: bool) -> Result<()> {
    if json_output {
        return json_value(&serde_json::to_value(response)?);
    }
    if let Some(message) = response.data.get("message").and_then(Value::as_str) {
        println!("{message}");
    } else {
        println!("{}", serde_json::to_string_pretty(&response.data)?);
    }
    Ok(())
}

pub fn mutation(response: Response, json_output: bool) -> Result<MutationReply> {
    let reply: MutationReply = serde_json::from_value(response.data.clone())?;
    if json_output {
        json_value(&serde_json::to_value(response)?)?;
    } else {
        println!("{}", reply.message);
    }
    Ok(reply)
}

pub fn servers(config: &Config, json_output: bool) -> Result<()> {
    if json_output {
        return json_value(&json!({"servers": config.servers, "revision": config.revision}));
    }
    if config.servers.is_empty() {
        println!(
            "No servers configured. Use `fwm add --server SSH_ALIAS --local --port PORT` to add a forward directly."
        );
        return Ok(());
    }
    println!(
        "{:<20} {:<30} {:<18} PORT",
        "NAME", "HOST / SSH ALIAS", "USER"
    );
    for server in &config.servers {
        println!(
            "{:<20} {:<30} {:<18} {}",
            server.name,
            server
                .host
                .as_deref()
                .or(server.ssh_alias.as_deref())
                .unwrap_or("—"),
            server.user.as_deref().unwrap_or("SSH config"),
            server
                .port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "SSH config".into())
        );
    }
    Ok(())
}

pub fn status(snapshot: &StatusSnapshot, daemon_running: bool, json_output: bool) -> Result<()> {
    status_with_warnings(snapshot, daemon_running, json_output, &[])
}

pub fn status_with_warnings(
    snapshot: &StatusSnapshot,
    daemon_running: bool,
    json_output: bool,
    warnings: &[String],
) -> Result<()> {
    status_with_state(
        snapshot,
        if daemon_running { "running" } else { "stopped" },
        json_output,
        warnings,
    )
}

pub fn status_with_state(
    snapshot: &StatusSnapshot,
    daemon_state: &str,
    json_output: bool,
    warnings: &[String],
) -> Result<()> {
    let daemon_running = daemon_state == "running";
    let stopped = daemon_state == "stopped";
    if json_output {
        let mut value = serde_json::to_value(snapshot)?;
        value["daemon_running"] = if daemon_running || stopped {
            json!(daemon_running)
        } else {
            Value::Null
        };
        value["daemon_state"] = json!(daemon_state);
        value["runtime_available"] = json!(daemon_running);
        value["warnings"] = json!(warnings);
        return json_value(&value);
    }
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    if stopped {
        println!("Daemon is not running. Saved running intent resumes with `fwm daemon start`.");
    } else if !daemon_running {
        println!(
            "Daemon {daemon_state}; live connection state is unavailable. Showing saved configuration."
        );
    }
    if snapshot.forwards.is_empty() {
        println!("No matching forwards.");
        return Ok(());
    }
    let headers = ["NAME", "SERVER", "GROUP", "STATE", "INTENT", "RETRY"].map(str::to_owned);
    let now = unix_ms();
    let rows: Vec<_> = snapshot
        .forwards
        .iter()
        .map(|forward| {
            [
                forward.name.clone(),
                forward.server.clone(),
                forward.group.as_deref().unwrap_or("—").to_owned(),
                if daemon_running {
                    state_name(forward.state)
                } else if stopped {
                    "daemon_offline"
                } else {
                    "unverified"
                }
                .to_owned(),
                match forward.desired_state {
                    DesiredState::Running => "running",
                    DesiredState::Stopped => "stopped",
                }
                .to_owned(),
                forward
                    .next_retry_unix_ms
                    .map(|when| format!("{}s", when.saturating_sub(now).div_ceil(1000)))
                    .unwrap_or_else(|| "—".into()),
            ]
        })
        .collect();
    // Rust's formatting width counts characters, not terminal columns. Measure
    // the whole table so both long identifiers and wide Unicode names align.
    let widths = std::array::from_fn(|column| {
        std::iter::once(&headers)
            .chain(&rows)
            .map(|row| row[column].width())
            .max()
            .unwrap_or(0)
    });
    println!("{}", status_line(&headers, &widths));
    for (forward, row) in snapshot.forwards.iter().zip(&rows) {
        println!("{}", status_line(row, &widths));
        if daemon_running || stopped {
            println!(
                "  {}  ({} active)",
                mapping(forward),
                forward.active_connections
            );
        } else {
            println!("  {}  (active connections unknown)", mapping(forward));
        }
        if let Some(error) = &forward.last_error {
            println!("  {error}");
        }
    }
    Ok(())
}

fn status_line(cells: &[String; 6], widths: &[usize; 6]) -> String {
    let mut line = String::new();
    for (column, value) in cells.iter().enumerate() {
        line.push_str(value);
        if column + 1 < cells.len() {
            line.push_str(&" ".repeat(widths[column] - value.width() + 2));
        }
    }
    line
}

fn mapping(forward: &ForwardStatus) -> String {
    let (source_side, target_side) = if matches!(forward.kind.as_str(), "remote" | "remote_dynamic")
    {
        ("remote", "local")
    } else {
        ("local", "remote")
    };
    let target = forward.target.as_deref().unwrap_or("SOCKS5 destinations");
    format!("{source_side} {} -> {target_side} {target}", forward.listen)
}

pub fn offline(config: Config) -> StatusSnapshot {
    StatusSnapshot {
        daemon_instance_id: String::new(),
        config_revision: config.revision,
        forwards: config
            .forwards
            .iter()
            .map(|forward| fwm_core::model::ForwardStatus {
                id: forward.id.clone(),
                name: forward.name.clone(),
                group: forward.group.clone(),
                server: config
                    .server(&forward.server_id)
                    .map(|server| server.name.clone())
                    .unwrap_or_else(|| forward.server_id.clone()),
                kind: forward.tunnel.kind().into(),
                listen: forward.tunnel.listen().to_string(),
                target: forward.tunnel.target().map(ToString::to_string),
                desired_state: forward.desired_state,
                state: RuntimeState::Stopped,
                retry_count: 0,
                next_retry_unix_ms: None,
                last_error: if forward.desired_state == DesiredState::Running {
                    Some("daemon is offline; running intent is saved".into())
                } else {
                    None
                },
                active_connections: 0,
            })
            .collect(),
    }
}

pub fn state_name(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Stopped => "stopped",
        RuntimeState::Starting => "starting",
        RuntimeState::Established => "established",
        RuntimeState::Backoff => "backoff",
        RuntimeState::NeedsAttention => "needs_attention",
        RuntimeState::Stopping => "stopping",
        RuntimeState::Unverified => "unverified",
    }
}

/// A successful diagnostic RPC can still contain failed server checks.
pub fn diagnostic(response: Response, json_output: bool) -> Result<()> {
    let failed = response
        .data
        .get("checks")
        .and_then(Value::as_array)
        .is_some_and(|checks| {
            checks
                .iter()
                .any(|check| check.get("ok").and_then(Value::as_bool) == Some(false))
        });
    if failed && json_output {
        let message = "one or more server checks failed; see the diagnostic results below";
        return Err(super::completion::CompletionError {
            code: "check_failed".into(),
            message: message.into(),
            result: json!({"ok":false,"error":{"code":"check_failed","message":message},"data":response.data}),
        }.into());
    }
    self::response(response, json_output)?;
    if failed {
        return Err(crate::client::ClientError {
            code: "check_failed".into(),
            message: "one or more server checks failed; see the diagnostic results above".into(),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_diagnostic_is_a_command_failure_but_successful_checks_are_not() {
        let failure = Response::success(
            "check".into(),
            json!({"checks":[{"server":"dev","ok":false,"error":"authentication failed"}]}),
        );
        let error = diagnostic(failure, true).unwrap_err();
        assert_eq!(super::super::error_code(&error), ("check_failed", 3));
        let success = Response::success(
            "check".into(),
            json!({"checks":[{"server":"dev","ok":true}]}),
        );
        assert!(diagnostic(success, true).is_ok());
    }

    #[test]
    fn mappings_show_both_sides_and_full_targets() {
        let mut forward = ForwardStatus {
            id: "id".into(),
            name: "proxy".into(),
            group: None,
            server: "dev".into(),
            kind: "remote".into(),
            listen: "[::1]:17890".into(),
            target: Some("127.0.0.1:7890".into()),
            desired_state: DesiredState::Running,
            state: RuntimeState::Established,
            retry_count: 0,
            next_retry_unix_ms: None,
            last_error: None,
            active_connections: 0,
        };
        assert_eq!(
            mapping(&forward),
            "remote [::1]:17890 -> local 127.0.0.1:7890"
        );
        forward.kind = "local".into();
        assert_eq!(
            mapping(&forward),
            "local [::1]:17890 -> remote 127.0.0.1:7890"
        );
        forward.kind = "dynamic".into();
        forward.target = None;
        assert_eq!(
            mapping(&forward),
            "local [::1]:17890 -> remote SOCKS5 destinations"
        );
        forward.kind = "remote_dynamic".into();
        assert_eq!(
            mapping(&forward),
            "remote [::1]:17890 -> local SOCKS5 destinations"
        );
    }
}
