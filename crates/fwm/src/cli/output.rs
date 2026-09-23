use std::io::IsTerminal;

use anyhow::Result;
use clap::builder::styling::{AnsiColor, Style};
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
    let color = std::io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var("TERM").as_deref() != Ok("dumb");
    print!("{}", status_text(snapshot, daemon_state, unix_ms(), color));
    Ok(())
}

fn status_text(snapshot: &StatusSnapshot, daemon_state: &str, now: u64, color: bool) -> String {
    let live = daemon_state == "running";
    let daemon_marker = match daemon_state {
        "running" => paint("●", AnsiColor::Green.on_default(), color),
        "stopped" => paint("○", Style::new().dimmed(), color),
        _ => paint("?", AnsiColor::Yellow.on_default(), color),
    };
    let mut text = format!("FWM  {daemon_marker} daemon {daemon_state}\n");
    let count = snapshot.forwards.len();
    let mut summary = vec![format!(
        "{count} {}",
        if count == 1 { "forward" } else { "forwards" }
    )];
    if live {
        for state in [
            RuntimeState::Established,
            RuntimeState::Backoff,
            RuntimeState::NeedsAttention,
            RuntimeState::Starting,
            RuntimeState::Stopping,
            RuntimeState::Stopped,
            RuntimeState::Unverified,
        ] {
            let count = snapshot
                .forwards
                .iter()
                .filter(|f| f.state == state)
                .count();
            if count > 0 {
                summary.push(format!("{count} {}", state_name(state)));
            }
        }
    } else {
        summary.push("live state unavailable".into());
    }
    text.push_str(&summary.join(" · "));
    text.push('\n');
    if daemon_state == "stopped" {
        text.push_str(
            "Daemon is not running. Saved running intent resumes with `fwm daemon start`.\n",
        );
    } else if !live {
        text.push_str("Showing saved configuration; live connection state is unknown.\n");
    }
    if snapshot.forwards.is_empty() {
        text.push_str("\nNo matching forwards.\n");
        return text;
    }

    let mut forwards: Vec<_> = snapshot.forwards.iter().collect();
    // Stable sorting keeps the configured order within each priority level.
    if live {
        forwards.sort_by_key(|forward| match forward.state {
            RuntimeState::NeedsAttention | RuntimeState::Unverified => 0,
            RuntimeState::Backoff => 1,
            _ if forward.last_error.is_some() => 2,
            _ => 3,
        });
    }
    let headers = [
        "", "NAME", "SERVER", "GROUP", "DIR", "SOURCE", "TARGET", "STATUS",
    ]
    .map(str::to_owned);
    let rows: Vec<_> = forwards
        .iter()
        .map(|forward| {
            let (marker, _) = status_style(forward.state, daemon_state);
            [
                marker.into(),
                forward.name.clone(),
                forward.server.clone(),
                forward.group.as_deref().unwrap_or("—").into(),
                direction(forward).into(),
                compact_endpoint(&forward.listen).into(),
                forward
                    .target
                    .as_deref()
                    .map(compact_endpoint)
                    .unwrap_or("SOCKS5")
                    .into(),
                status_label(forward, daemon_state, now),
            ]
        })
        .collect();
    // Measure unstyled terminal columns, including wide Unicode identifiers.
    let widths = std::array::from_fn(|column| {
        std::iter::once(&headers)
            .chain(&rows)
            .map(|row| row[column].width())
            .max()
            .unwrap_or(0)
    });
    text.push('\n');
    text.push_str(&paint(
        &status_line(&headers, &widths, None),
        Style::new().bold(),
        color,
    ));
    text.push('\n');
    let separator = "─".repeat(widths.iter().sum::<usize>() + 2 * (headers.len() - 1));
    text.push_str(&paint(&separator, Style::new().dimmed(), color));
    text.push('\n');
    for (forward, row) in forwards.iter().zip(&rows) {
        let (_, style) = status_style(forward.state, daemon_state);
        text.push_str(&status_line(row, &widths, color.then_some(style)));
        text.push('\n');
    }
    // Offline snapshots repeat the same daemon warning for every saved rule.
    // The global notice above already explains it; only show live diagnostics here.
    if live {
        let mut heading = false;
        for forward in forwards {
            if let Some(error) = &forward.last_error {
                if !heading {
                    text.push_str("\nAttention\n");
                    heading = true;
                }
                text.push_str(&format!("  {}\n", forward.name));
                for line in error.lines() {
                    text.push_str(&format!("    {line}\n"));
                }
            }
        }
    }
    text
}

fn paint(value: &str, style: Style, color: bool) -> String {
    if color {
        format!("{style}{value}{}", style.render_reset())
    } else {
        value.into()
    }
}

fn status_line(cells: &[String; 8], widths: &[usize; 8], style: Option<Style>) -> String {
    let mut line = String::new();
    for (column, value) in cells.iter().enumerate() {
        if let Some(style) = style.filter(|_| column == 0 || column == cells.len() - 1) {
            line.push_str(&paint(value, style, true));
        } else {
            line.push_str(value);
        }
        if column + 1 < cells.len() {
            line.push_str(&" ".repeat(widths[column] - value.width() + 2));
        }
    }
    line
}

fn compact_endpoint(endpoint: &str) -> &str {
    let Some((host, port)) = endpoint.rsplit_once(':') else {
        return endpoint;
    };
    if host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "[::1]") {
        port
    } else {
        endpoint
    }
}

fn direction(forward: &ForwardStatus) -> &'static str {
    if matches!(forward.kind.as_str(), "remote" | "remote_dynamic") {
        "R→L"
    } else {
        "L→R"
    }
}

fn status_label(forward: &ForwardStatus, daemon_state: &str, now: u64) -> String {
    let mut label = match daemon_state {
        "running" if forward.state == RuntimeState::Backoff => forward
            .next_retry_unix_ms
            .map(|when| format!("retry in {}s", when.saturating_sub(now).div_ceil(1000)))
            .unwrap_or_else(|| "retrying".into()),
        "running" => state_name(forward.state).into(),
        "stopped" => "offline".into(),
        _ => "unverified".into(),
    };
    if forward.desired_state == DesiredState::Running
        && (daemon_state != "running"
            || matches!(
                forward.state,
                RuntimeState::Stopped | RuntimeState::Stopping | RuntimeState::Unverified
            ))
    {
        label.push_str(" (want running)");
    } else if forward.desired_state == DesiredState::Stopped
        && daemon_state == "running"
        && forward.state != RuntimeState::Stopped
    {
        label.push_str(" (want stopped)");
    }
    label
}

fn status_style(state: RuntimeState, daemon_state: &str) -> (&'static str, Style) {
    match daemon_state {
        "stopped" => return ("○", Style::new().dimmed()),
        "running" => {}
        _ => return ("?", AnsiColor::Yellow.on_default()),
    }
    match state {
        RuntimeState::Established => ("●", AnsiColor::Green.on_default()),
        RuntimeState::Backoff => ("↻", AnsiColor::Yellow.on_default()),
        RuntimeState::NeedsAttention => ("!", AnsiColor::Red.on_default()),
        RuntimeState::Unverified => ("?", AnsiColor::Yellow.on_default()),
        RuntimeState::Starting | RuntimeState::Stopping => ("…", AnsiColor::Yellow.on_default()),
        RuntimeState::Stopped => ("○", Style::new().dimmed()),
    }
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
        RuntimeState::Established => "connected",
        RuntimeState::Backoff => "retrying",
        RuntimeState::NeedsAttention => "needs attention",
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
    fn status_table_keeps_directions_and_endpoints_and_moves_errors_below_rows() {
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
        let mut snapshot = StatusSnapshot {
            daemon_instance_id: "daemon".into(),
            config_revision: 1,
            forwards: vec![forward.clone()],
        };
        forward.name = "database".into();
        forward.kind = "local".into();
        forward.listen = "0.0.0.0:5432".into();
        forward.target = Some("[2001:db8::1]:5432".into());
        forward.state = RuntimeState::Backoff;
        forward.next_retry_unix_ms = Some(7_001);
        forward.last_error = Some("connection refused\nwill retry".into());
        snapshot.forwards.push(forward.clone());
        forward.name = "socks".into();
        forward.kind = "dynamic".into();
        forward.listen = "localhost:1080".into();
        forward.target = None;
        forward.state = RuntimeState::Stopped;
        forward.desired_state = DesiredState::Stopped;
        forward.next_retry_unix_ms = None;
        forward.last_error = None;
        snapshot.forwards.push(forward.clone());
        forward.name = "remote-socks".into();
        forward.kind = "remote_dynamic".into();
        snapshot.forwards.push(forward);

        let text = status_text(&snapshot, "running", 1_000, false);
        assert!(
            text.contains("4 forwards · 1 connected · 1 retrying · 2 stopped"),
            "{text}"
        );
        let rows: Vec<_> = text
            .lines()
            .filter(|line| matches!(line.split_whitespace().next(), Some("●" | "↻" | "○")))
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .collect();
        assert_eq!(
            rows,
            vec![
                vec![
                    "↻",
                    "database",
                    "dev",
                    "—",
                    "L→R",
                    "0.0.0.0:5432",
                    "[2001:db8::1]:5432",
                    "retry",
                    "in",
                    "7s"
                ],
                vec![
                    "●",
                    "proxy",
                    "dev",
                    "—",
                    "R→L",
                    "17890",
                    "7890",
                    "connected"
                ],
                vec!["○", "socks", "dev", "—", "L→R", "1080", "SOCKS5", "stopped"],
                vec![
                    "○",
                    "remote-socks",
                    "dev",
                    "—",
                    "R→L",
                    "1080",
                    "SOCKS5",
                    "stopped"
                ],
            ]
        );
        assert!(
            text.ends_with("\nAttention\n  database\n    connection refused\n    will retry\n")
        );
        assert!(!text.contains("active"));
        assert!(!text.contains('\x1b'));
        assert!(status_text(&snapshot, "running", 8_000, false).contains("retry in 0s"));

        // Even a cached successful snapshot must not claim live connectivity
        // or show an old retry countdown when the daemon cannot be reached.
        for daemon_state in ["stopped", "unresponsive", "unavailable"] {
            let offline = status_text(&snapshot, daemon_state, 1_000, false);
            assert!(
                !offline.contains("connected") && !offline.contains("retry in"),
                "{offline}"
            );
            assert!(offline.contains("live state unavailable"), "{offline}");
            assert!(offline.contains("(want running)"), "{offline}");
        }
    }
}
