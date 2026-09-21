//! SSH diagnostics and explicit trust shared by online and offline commands.
use fwm_api::protocol::{ApiError, Command};
use fwm_core::{
    model::{Config, RetryPolicy, ServerProfile},
    paths::Paths,
    ssh::{self, HostKeyInfo},
    store::Store,
};
use serde_json::{Value, json};

pub fn validate_profile(profile: &ServerProfile) -> Result<(), ApiError> {
    Config {
        servers: vec![profile.clone()],
        ..Default::default()
    }
    .validate()
    .map_err(|error| ApiError::new("invalid_config", error))
}

pub async fn inspect(
    profile: &ServerProfile,
    policy: &RetryPolicy,
) -> Result<HostKeyInfo, ApiError> {
    validate_profile(profile)?;
    ssh::inspect_host_key(profile, policy)
        .await
        .map_err(|error| ApiError::new("ssh_error", error.to_string()))
}

pub async fn inspect_hop(
    profile: &ServerProfile,
    policy: &RetryPolicy,
    hop: &str,
) -> Result<HostKeyInfo, ApiError> {
    validate_profile(profile)?;
    ssh::inspect_hop_key(profile, policy, hop)
        .await
        .map_err(|error| ApiError::new("ssh_error", error.to_string()))
}

pub fn trust(info: &mut HostKeyInfo, fingerprint: &str) -> Result<(), ApiError> {
    ssh::trust_host_key(info, fingerprint)
        .map_err(|error| ApiError::new("trust_error", error.to_string()))?;
    info.status = "trusted".into();
    Ok(())
}

pub fn configuration(store: &Store, applied: &Config) -> Value {
    match store.read_candidate() {
        Ok(candidate) => {
            let pending = candidate != *applied;
            let mut result = json!({
                "valid":true,
                "applied_revision":applied.revision,
                "candidate_revision":candidate.revision,
                "unapplied_changes":pending,
                "using_applied_snapshot":pending,
            });
            if pending {
                result["warning"] = json!(
                    "config.toml has unapplied edits; SSH checks use the applied configuration. Run config reload to apply the draft."
                );
            }
            result
        }
        Err(error) => json!({
            "valid":false,
            "applied_revision":applied.revision,
            "unapplied_changes":true,
            "using_applied_snapshot":true,
            "error":format!("{error:#}"),
            "warning":"config.toml is invalid; SSH checks use the last applied configuration. Repair the draft, then run config validate and config reload.",
        }),
    }
}

pub async fn doctor(
    profiles: Vec<ServerProfile>,
    policy: RetryPolicy,
    configuration: Value,
    process: &'static str,
) -> Result<Value, ApiError> {
    let mut checks = tokio::task::JoinSet::new();
    for profile in profiles {
        let policy = policy.clone();
        checks.spawn(async move {
            let context = ssh::resolve(&profile).ok().map(|resolved| json!({
                "process":process, "pid":std::process::id(),
                "agent_socket":ssh::agent_socket(&resolved),
                "agent_source":if resolved.identity_agent.is_some() {"SSH config IdentityAgent"} else {"authentication process SSH_AUTH_SOCK"},
                "ssh_config":resolved.ssh_config,
                "note":"A running daemon keeps the environment inherited at startup. For service-managed daemons, configure IdentityAgent explicitly in the SSH config and restart this server.",
            }));
            let mut result = match ssh::check_connection(&profile, &policy).await {
                Ok(()) => json!({"server":profile.name,"ok":true,"authenticated":true,"note":"authentication succeeded; forwarding permissions are checked by individual listener/channel requests"}),
                Err(error) => json!({"server":profile.name,"ok":false,"error":error.to_string()}),
            };
            result["authentication_context"] = json!(context);
            result["server_id"] = json!(profile.id);
            result["ssh_config"] = json!(profile.ssh_config);
            result
        });
    }
    let mut results = vec![json!({"kind":"configuration","ok":configuration["valid"]})];
    while let Some(result) = checks.join_next().await {
        results.push(result.map_err(|error| ApiError::new("internal_error", error.to_string()))?);
    }
    Ok(json!({"backend":"russh", "configuration":configuration, "checks":results}))
}

/// Execute a probe without acquiring a daemon lock or starting saved forwards.
pub async fn offline(paths: &Paths, command: Command) -> Result<Value, ApiError> {
    let store = Store::new(paths.clone());
    let diagnostic = matches!(
        command,
        Command::Doctor { .. } | Command::DoctorProfile { .. }
    );
    let (applied, load_error) = match store.load() {
        Ok(loaded) => (loaded.config, None),
        Err(error) if diagnostic => (Config::default(), Some(format!("{error:#}"))),
        Err(error) => return Err(ApiError::new("invalid_config", format!("{error:#}"))),
    };
    let configuration = if !paths.config_file.exists()
        && !paths.state_dir.join("applied.toml").exists()
    {
        json!({"valid":true,"applied_revision":0,"candidate_revision":0,"unapplied_changes":false,"using_applied_snapshot":false})
    } else {
        let mut report = configuration(&store, &applied);
        if let Some(error) = load_error {
            report["valid"] = json!(false);
            report["using_applied_snapshot"] = json!(false);
            report["error"] = json!(error);
            report["warning"] = json!(
                "No valid applied configuration is available. Repair config.toml before starting forwards."
            );
        }
        report
    };
    match command {
        Command::InspectHostProfile { server } => {
            encode(inspect(&server, &applied.defaults.retry).await?)
        }
        Command::TrustHostProfile {
            server,
            fingerprint,
        } => {
            let mut info = inspect(&server, &applied.defaults.retry).await?;
            trust(&mut info, &fingerprint)?;
            trust_result(info, record_trust(paths, &server, None).err())
        }
        Command::InspectHopProfile { server, hop } => {
            encode(inspect_hop(&server, &applied.defaults.retry, &hop).await?)
        }
        Command::TrustHopProfile {
            server,
            hop,
            fingerprint,
        } => {
            let mut info = inspect_hop(&server, &applied.defaults.retry, &hop).await?;
            trust(&mut info, &fingerprint)?;
            trust_result(info, record_trust(paths, &server, Some(&hop)).err())
        }
        Command::DoctorProfile { server } => {
            validate_profile(&server)?;
            doctor(vec![server], applied.defaults.retry, configuration, "CLI").await
        }
        Command::Doctor { server } => {
            let profiles = if let Some(selector) = server {
                vec![
                    applied
                        .server(&selector)
                        .ok_or_else(|| ApiError::new("not_found", "server not found"))?
                        .clone(),
                ]
            } else {
                applied.servers
            };
            doctor(profiles, applied.defaults.retry, configuration, "CLI").await
        }
        _ => Err(ApiError::new(
            "invalid_request",
            "this operation requires the daemon",
        )),
    }
}

fn trust_result(info: HostKeyInfo, warning: Option<ApiError>) -> Result<Value, ApiError> {
    let mut result = encode(info)?;
    result["trusted"] = json!(true);
    if let Some(warning) = warning {
        result["warnings"] = json!([format!(
            "Host key is trusted; history could not be recorded: {}",
            warning.message
        )]);
    }
    Ok(result)
}

fn record_trust(paths: &Paths, profile: &ServerProfile, hop: Option<&str>) -> Result<(), ApiError> {
    use fwm_core::{
        history::{HistoryEntry, append_history},
        model::{EngineEvent, unix_ms},
    };
    paths
        .ensure_dirs()
        .map_err(|error| ApiError::new("storage_error", error.to_string()))?;
    let entry = HistoryEntry::for_server(
        format!("offline-{}", uuid::Uuid::new_v4()),
        EngineEvent {
            context: None,
            sequence: 1,
            timestamp_ms: unix_ms(),
            forward_id: None,
            server_id: Some(profile.id.clone()),
            message: format!(
                "host key trusted for {}{}",
                profile.name,
                hop.map(|hop| format!(" hop {hop}")).unwrap_or_default()
            ),
        },
        profile,
    );
    append_history(&paths.state_dir.join("events.jsonl"), &entry).map_err(|error| {
        ApiError::new(
            "storage_error",
            format!("host key is trusted, but its history could not be recorded: {error}"),
        )
    })
}

pub fn command_line(paths: &Paths, arguments: &[&str]) -> String {
    let executable = std::env::current_exe().unwrap_or_else(|_| "fwm".into());
    let mut words = vec![
        executable.to_string_lossy().into_owned(),
        "--config-dir".into(),
        paths.cli_config_dir().to_string_lossy().into_owned(),
    ];
    words.extend(arguments.iter().map(|word| (*word).to_owned()));
    render_command(&words, cfg!(windows))
}

/// Format for the shell named in the diagnostic response. Keep both dialects
/// testable on every host; PowerShell needs an invocation operator for quoted
/// executable paths and escapes literal apostrophes by doubling them.
fn render_command(words: &[String], powershell: bool) -> String {
    let rendered = words
        .iter()
        .map(|word| {
            if powershell {
                format!("'{}'", word.replace('\'', "''"))
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if powershell {
        format!("& {rendered}")
    } else {
        rendered
    }
}

pub fn diagnostic_recovery(paths: &Paths, data: &mut Value) {
    let saved = Store::new(paths.clone())
        .load()
        .ok()
        .map(|loaded| loaded.config);
    if let Some(checks) = data.get_mut("checks").and_then(Value::as_array_mut) {
        for check in checks {
            if let Some(server) = check
                .get("server")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let registered = saved.as_ref().is_some_and(|config| {
                    check["server_id"]
                        .as_str()
                        .is_some_and(|id| config.server(id).is_some())
                });
                let mut check_args = vec!["server", "check", server.as_str()];
                if let Some(path) = check["ssh_config"].as_str() {
                    check_args.extend(["--ssh-config", path]);
                }
                let check_command = command_line(paths, &check_args);
                check["recovery"] = json!({
                    "shell":if cfg!(windows) {"PowerShell"} else {"POSIX shell"},
                    "check_again":check_command,
                    "refresh_server":registered.then(|| command_line(paths, &["restart", "--server", &server])),
                    "refresh_unmanaged_daemon_environment":command_line(paths, &["daemon", "restart"]),
                    "current_shell_agent_socket":std::env::var("SSH_AUTH_SOCK").ok(),
                    "service_environment":"For an installed startup service, set IdentityAgent to the intended socket in SSH config before restarting the server; service managers supply their own environment.",
                });
            }
        }
    }
}

fn encode(value: impl serde::Serialize) -> Result<Value, ApiError> {
    serde_json::to_value(value).map_err(|error| ApiError::new("internal_error", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powershell_commands_invoke_quoted_executables_and_preserve_literal_arguments() {
        let words = [
            r"C:\Program Files\O'Brien\fwm.exe",
            "--config-dir",
            r"C:\Users\O'Brien\SSH configs",
            "server",
            "check",
            "work's server",
            "--ssh-config",
            r"C:\SSH configs\literal $HOME `tick`'s.conf",
        ]
        .map(String::from);
        assert_eq!(
            render_command(&words, true),
            r"& 'C:\Program Files\O''Brien\fwm.exe' '--config-dir' 'C:\Users\O''Brien\SSH configs' 'server' 'check' 'work''s server' '--ssh-config' 'C:\SSH configs\literal $HOME `tick`''s.conf'"
        );
    }

    #[test]
    fn posix_commands_quote_spaces_apostrophes_and_shell_metacharacters() {
        let words = [
            "/opt/My Tools/O'Brien/fwm",
            "--config-dir",
            "/tmp/SSH configs/O'Brien",
            "server",
            "check",
            "work's server",
            "--ssh-config",
            "/tmp/literal $HOME `tick`'s.conf",
        ]
        .map(String::from);
        assert_eq!(
            render_command(&words, false),
            r"'/opt/My Tools/O'\''Brien/fwm' '--config-dir' '/tmp/SSH configs/O'\''Brien' 'server' 'check' 'work'\''s server' '--ssh-config' '/tmp/literal $HOME `tick`'\''s.conf'"
        );
    }

    #[test]
    fn diagnostics_identify_the_shell_used_by_recovery_commands() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().join("config's directory"))).unwrap();
        let mut data = json!({"checks":[{"server":"fixture", "server_id":"unregistered"}]});
        diagnostic_recovery(&paths, &mut data);
        let recovery = &data["checks"][0]["recovery"];
        assert_eq!(
            recovery["shell"],
            if cfg!(windows) {
                "PowerShell"
            } else {
                "POSIX shell"
            }
        );
        assert_eq!(
            recovery["check_again"].as_str().unwrap().starts_with("& "),
            cfg!(windows)
        );
        assert!(recovery["refresh_server"].is_null());
    }
}
