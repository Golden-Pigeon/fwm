//! Idempotent host trust, including a single hop in a target's SSH route.
use super::{get_config, output, server_selection};
use crate::offline;
use anyhow::{Context, Result, bail};
use fwm_api::protocol::Command;
use fwm_core::paths::Paths;
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
};

pub(super) async fn run(
    paths: &Paths,
    name: String,
    expected: Option<String>,
    ssh_config: Option<PathBuf>,
    hop: Option<String>,
    json_output: bool,
) -> Result<()> {
    let config = get_config(paths, false).await?;
    let server = server_selection::select(&config, &name, ssh_config.as_deref())?.profile;
    let config_arg = ssh_config
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let command = |hop: Option<&str>, fingerprint: Option<&str>| {
        let mut args = vec!["server", "trust", name.as_str()];
        if let Some(config) = &config_arg {
            args.extend(["--ssh-config", config]);
        }
        if let Some(hop) = hop {
            args.extend(["--hop", hop]);
        }
        if let Some(fingerprint) = fingerprint {
            args.extend(["--fingerprint", fingerprint]);
        }
        crate::ssh_actions::command_line(paths, &args)
    };
    let response = offline::query(
        paths,
        if let Some(hop) = &hop {
            Command::InspectHopProfile {
                server: server.clone(),
                hop: hop.clone(),
            }
        } else {
            Command::InspectHostProfile {
                server: server.clone(),
            }
        },
    )
    .await
    .map_err(|error| {
        let message = error.to_string();
        if let Some(number) = message
            .split("SSH hop ")
            .nth(1)
            .and_then(|tail| tail.split_whitespace().next())
            .filter(|number| {
                number.parse::<usize>().is_ok() && message.contains("inspect/trust this hop")
            })
        {
            return error.context(format!(
                "Inspect and explicitly trust this hop using the target's database: {}",
                command(Some(number), None)
            ));
        }
        error
    })?;
    let fingerprint = response
        .data
        .get("fingerprint")
        .and_then(serde_json::Value::as_str)
        .context("host inspection did not include a fingerprint")?
        .to_owned();
    if let Some(expected) = expected {
        if fingerprint != expected {
            bail!(
                "host key fingerprint mismatch: expected {expected}, observed {fingerprint}; no trust was changed"
            );
        }
    } else if response
        .data
        .get("status")
        .and_then(serde_json::Value::as_str)
        != Some("trusted")
    {
        if json_output {
            let message = format!(
                "explicit trust required; verify this fingerprint independently, then run {}",
                command(hop.as_deref(), Some(&fingerprint))
            );
            let result = serde_json::json!({"ok":false,"error":{"code":"trust_required","message":message},"data":response.data});
            return Err(super::completion::CompletionError {
                code: "trust_required".into(),
                message,
                result,
            }
            .into());
        }
        if !std::io::stdin().is_terminal() {
            output::response(response, false)?;
            bail!(
                "explicit trust required; verify this fingerprint independently, then run {}",
                command(hop.as_deref(), Some(&fingerprint))
            );
        }
        eprintln!(
            "Host key for {name}:\n{}",
            serde_json::to_string_pretty(&response.data)?
        );
        eprint!("After verifying this fingerprint independently, type 'yes' to trust it: ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            bail!("host key was not trusted");
        }
    }
    output::response(
        offline::query(
            paths,
            if let Some(hop) = hop {
                Command::TrustHopProfile {
                    server,
                    hop,
                    fingerprint,
                }
            } else {
                Command::TrustHostProfile {
                    server,
                    fingerprint,
                }
            },
        )
        .await?,
        json_output,
    )
}
