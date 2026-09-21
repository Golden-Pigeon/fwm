mod add;
pub mod args;
mod cleanup;
mod completion;
mod daemon;
mod forwards;
mod groups;
mod input;
mod output;
mod parse;
mod ports;
mod queries;
mod server_selection;
mod server_trust;
mod servers;
mod shell_completion;

#[cfg(test)]
mod contracts_tests;

use crate::client;
use anyhow::Result;
use args::{Cli, Command, ConfigCommand};
use fwm_api::protocol::Command as Rpc;
use fwm_core::{model::Config, paths::Paths, store::Store};

pub async fn run(args: Cli) -> Result<()> {
    // Shell completion must also work before an instance exists, without IPC,
    // directory creation, or validation of the command being completed.
    if let Command::Completions { shell } = &args.command {
        return shell_completion::script(*shell);
    }
    if let Command::Complete { cursor, words } = &args.command {
        return shell_completion::write_candidates(words, *cursor);
    }
    let paths = Paths::new(args.config_dir)?;
    match args.command {
        Command::Server { command } => servers::run(&paths, command, args.json).await,
        Command::Add(command) => add::run(&paths, command, args.json).await,
        Command::Edit(command) => forwards::edit(&paths, command, args.json).await,
        Command::Group { command } => groups::run(&paths, command, args.json).await,
        Command::Up(command) => forwards::up(&paths, command, args.json).await,
        Command::Down(command) => forwards::down(&paths, command, args.json).await,
        Command::Retry(command) => forwards::retry(&paths, command, args.json).await,
        Command::Restart(command) => forwards::restart(&paths, command, args.json).await,
        Command::Remove(command) => forwards::remove(&paths, command, args.json).await,
        Command::Status(command) => {
            queries::status(&paths, command.selection, command.watch, args.json).await
        }
        Command::Logs(command) => {
            queries::logs(
                &paths,
                command.selection,
                command.follow,
                command.tail,
                args.json,
            )
            .await
        }
        Command::Doctor { server, ssh_config } => {
            let request = if let Some(server) = server {
                let config = get_config(&paths, false).await?;
                let selected = server_selection::select(&config, &server, ssh_config.as_deref())?;
                Rpc::DoctorProfile {
                    server: selected.profile,
                }
            } else {
                Rpc::Doctor { server: None }
            };
            output::diagnostic(crate::offline::query(&paths, request).await?, args.json)
        }
        Command::Config { command } => config(&paths, command, args.json).await,
        Command::Daemon { command } => daemon::run(paths, command, args.json).await,
        Command::Service { command } => daemon::service(&paths, command, args.json).await,
        Command::Completions { .. } | Command::Complete { .. } => unreachable!(),
    }
}

async fn get_config(paths: &Paths, online: bool) -> Result<Config> {
    if online || client::running(paths).await {
        let config: Config = client::decode(client::request(paths, Rpc::GetConfig).await?)?;
        if config.schema_version != fwm_core::model::SCHEMA_VERSION {
            return Err(client::ClientError {
                code:"daemon_upgrade_required".into(),
                message:format!("the running daemon uses configuration schema {}; run {}, then retry so the new daemon can migrate saved rules", config.schema_version, crate::ssh_actions::command_line(paths, &["daemon", "restart"])),
            }.into());
        }
        return Ok(config);
    }
    // Loading is also used by JSON mutations and diagnostics. Their output
    // layer owns warnings; emitting text here would corrupt a single-result
    // JSON failure. Doctor reports candidate/applied differences explicitly.
    Ok(Store::new(paths.clone()).load()?.config)
}

async fn config(paths: &Paths, command: ConfigCommand, json_output: bool) -> Result<()> {
    match command {
        ConfigCommand::Recover {
            discard_unreadable_intent,
            ..
        } => {
            let result = crate::offline::recover(paths, discard_unreadable_intent).await?;
            if json_output {
                output::json_value(
                    &serde_json::json!({"ok":true,"saved":true,"daemon_running":false,"message":"Configuration recovered; all rules are stopped. Use up to start selected rules.","data":result}),
                )?;
            } else {
                println!(
                    "Configuration recovered; all rules are stopped. Backups: {}. Use `fwm up` to start selected rules.",
                    result.backup_directory.display()
                );
                if let Some(warning) = result.warning {
                    eprintln!("warning: {warning}");
                }
            }
        }
        ConfigCommand::Validate => {
            let config = Store::new(paths.clone()).read_candidate()?;
            if json_output {
                output::json_value(&serde_json::json!({"valid":true,"revision":config.revision}))?;
            } else {
                println!("Configuration is valid: {}", paths.config_file.display());
            }
        }
        ConfigCommand::Reload => {
            output::mutation(
                crate::offline::mutate(paths, Rpc::Reload, None).await?,
                json_output,
            )?;
        }
        ConfigCommand::Export => {
            let config = get_config(paths, false).await?;
            if json_output {
                output::json_value(&serde_json::to_value(config)?)?;
            } else {
                if !client::running(paths).await
                    && let Some(warning) = Store::new(paths.clone()).load()?.warning
                {
                    eprintln!("warning: {warning}");
                }
                print!("{}", toml::to_string_pretty(&config)?);
            }
        }
    }
    Ok(())
}

pub fn error_code(error: &anyhow::Error) -> (&str, u8) {
    if let Some(service) = error.downcast_ref::<crate::platform::service::ServiceError>() {
        return (
            &service.code,
            if service.code.ends_with("timeout") {
                4
            } else {
                5
            },
        );
    }
    if let Some(completion) = error.downcast_ref::<completion::CompletionError>() {
        return (
            &completion.code,
            if completion.code == "wait_timeout" {
                4
            } else if matches!(
                completion.code.as_str(),
                "needs_attention" | "check_failed" | "trust_required"
            ) {
                3
            } else {
                5
            },
        );
    }
    if let Some(client) = error.downcast_ref::<client::ClientError>() {
        let status = match client.code.as_str() {
            "needs_attention" | "authentication" | "host_key" | "trust_required"
            | "check_failed" => 3,
            "wait_timeout" | "daemon_stop_timeout" | "service_timeout" => 4,
            "daemon_unavailable"
            | "daemon_unresponsive"
            | "ipc_timeout"
            | "storage_error"
            | "runtime_error" => 5,
            _ => 2,
        };
        (&client.code, status)
    } else if error.chain().any(|cause| cause.is::<std::io::Error>()) {
        ("io_error", 5)
    } else {
        ("invalid_request", 2)
    }
}

pub fn json_error(error: &anyhow::Error) -> Option<serde_json::Value> {
    error
        .downcast_ref::<completion::CompletionError>()
        .map(|error| error.result.clone())
}
