use super::{
    args::{ServerCommand, ServerEditArgs, ServerField},
    get_config, output, server_selection,
};
use crate::offline;
use anyhow::{Result, bail};
use fwm_api::protocol::Command;
use fwm_core::{
    model::{Config, ServerProfile},
    paths::Paths,
};
#[cfg(test)]
use std::path::PathBuf;

pub async fn run(paths: &Paths, command: ServerCommand, json_output: bool) -> Result<()> {
    match command {
        ServerCommand::List => output::servers(&get_config(paths, false).await?, json_output),
        ServerCommand::Add(args) => {
            let config = get_config(paths, false).await?;
            if config.server(&args.name).is_some() {
                bail!("server {:?} already exists", args.name);
            }
            let mut server = ServerProfile::new(args.name);
            server.host = args.host;
            server.user = args.user;
            server.port = args.port;
            server.ssh_alias = args.ssh;
            server.ssh_config = args.ssh_config;
            server.identity_files = args.identity_files;
            server.known_hosts = args.known_hosts;
            server.proxy_jump = args.proxy_jump;
            fwm_core::ssh::normalize_profile_paths(&mut server, &std::env::current_dir()?)?;
            output::mutation(
                offline::mutate(paths, Command::PutServer { server }, Some(config.revision))
                    .await?,
                json_output,
            )?;
            Ok(())
        }
        ServerCommand::Edit(args) => {
            let config = get_config(paths, false).await?;
            let server = edited(&config, args)?;
            output::mutation(
                offline::mutate(paths, Command::PutServer { server }, Some(config.revision))
                    .await?,
                json_output,
            )?;
            Ok(())
        }
        ServerCommand::Remove { name } => {
            let config = get_config(paths, false).await?;
            output::mutation(
                offline::mutate(
                    paths,
                    Command::RemoveServer { selector: name },
                    Some(config.revision),
                )
                .await?,
                json_output,
            )?;
            Ok(())
        }
        ServerCommand::Check { name, ssh_config } => {
            let config = get_config(paths, false).await?;
            let server = server_selection::select(&config, &name, ssh_config.as_deref())?.profile;
            output::diagnostic(
                offline::query(paths, Command::DoctorProfile { server }).await?,
                json_output,
            )
        }
        ServerCommand::Trust {
            name,
            fingerprint,
            ssh_config,
            hop,
        } => super::server_trust::run(paths, name, fingerprint, ssh_config, hop, json_output).await,
    }
}

fn edited(config: &Config, args: ServerEditArgs) -> Result<ServerProfile> {
    validate_unset(&args)?;
    let base = std::env::current_dir()?;
    let existing = config.server(&args.name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown server {:?}; use `fwm server add` to create it",
            args.name
        )
    })?;
    let mut server = existing.clone();
    for field in &args.unset {
        match field {
            ServerField::User => server.user = None,
            ServerField::Port => server.port = None,
            ServerField::Identity => server.identity_files.clear(),
            ServerField::SshConfig => server.ssh_config = None,
            ServerField::KnownHosts => server.known_hosts = None,
            ServerField::ProxyJump => server.proxy_jump.clear(),
        }
    }
    if let Some(name) = args.rename {
        server.name = name;
    }
    if let Some(alias) = args.ssh {
        server.ssh_alias = Some(alias);
        server.host = None;
    }
    if let Some(host) = args.host {
        server.host = Some(host);
        server.ssh_alias = None;
    }
    if let Some(user) = args.user {
        server.user = Some(user);
    }
    if let Some(port) = args.port {
        server.port = Some(port);
    }
    if let Some(identities) = args.identity_files {
        server.identity_files = identities
            .iter()
            .map(|path| fwm_core::ssh::normalized_path(path, &base))
            .collect::<Result<_, _>>()?;
    }
    if let Some(path) = args.ssh_config {
        server.ssh_config = Some(fwm_core::ssh::normalized_path(&path, &base)?);
    }
    if let Some(path) = args.known_hosts {
        server.known_hosts = Some(fwm_core::ssh::normalized_path(&path, &base)?);
    }
    if let Some(jumps) = args.proxy_jump {
        server.proxy_jump = jumps;
    }
    let mut candidate = config.clone();
    *candidate
        .servers
        .iter_mut()
        .find(|profile| profile.id == server.id)
        .unwrap() = server.clone();
    candidate.validate().map_err(anyhow::Error::msg)?;
    Ok(server)
}

fn validate_unset(args: &ServerEditArgs) -> Result<()> {
    for field in &args.unset {
        let (specified, flag) = match field {
            ServerField::User => (args.user.is_some(), "user"),
            ServerField::Port => (args.port.is_some(), "port"),
            ServerField::Identity => (args.identity_files.is_some(), "identity"),
            ServerField::SshConfig => (args.ssh_config.is_some(), "ssh-config"),
            ServerField::KnownHosts => (args.known_hosts.is_some(), "known-hosts"),
            ServerField::ProxyJump => (args.proxy_jump.is_some(), "proxy-jump"),
        };
        if specified {
            bail!("--{flag} cannot be combined with --unset {flag}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Cli, Command as CliCommand};

    fn edit(config: &Config, flags: &[&str]) -> Result<ServerProfile> {
        let mut argv = vec!["fwm", "server", "edit", "dev"];
        argv.extend(flags);
        let CliCommand::Server {
            command: ServerCommand::Edit(args),
        } = Cli::try_parse_from(argv)?.command
        else {
            unreachable!()
        };
        edited(config, args)
    }

    fn config() -> Config {
        let mut server = ServerProfile::new("dev");
        server.host = Some("old.example".into());
        server.user = Some("alice".into());
        server.port = Some(2222);
        server.identity_files = vec!["old-key".into()];
        server.proxy_jump = vec!["bastion".into()];
        Config {
            servers: vec![server],
            ..Default::default()
        }
    }

    #[test]
    fn server_edit_preserves_identity_and_unspecified_fields() {
        let config = config();
        let original = config.servers[0].clone();
        let renamed = edit(&config, &["--rename", "production"]).unwrap();
        assert_eq!(renamed.name, "production");
        assert_eq!(
            ServerProfile {
                name: original.name.clone(),
                ..renamed
            },
            original
        );
        let updated = edit(
            &config,
            &[
                "--port",
                "2200",
                "--identity",
                "key-a",
                "--identity",
                "key-b",
                "--proxy-jump",
                "none",
            ],
        )
        .unwrap();
        assert_eq!(updated.id, original.id);
        assert_eq!(updated.port, Some(2200));
        assert_eq!(
            updated.identity_files,
            ["key-a", "key-b"].map(|name| fwm_core::ssh::normalized_path(
                &PathBuf::from(name),
                &std::env::current_dir().unwrap()
            )
            .unwrap())
        );
        assert_eq!(updated.proxy_jump, ["none"]);
        assert_eq!(updated.user, original.user);
        assert_eq!(updated.host, original.host);
        assert_eq!(config.servers[0], original);
    }

    #[test]
    fn server_edit_changes_address_modes_and_rejects_invalid_or_duplicate_names() {
        let mut config = config();
        let alias = edit(&config, &["--ssh", "new-alias"]).unwrap();
        assert_eq!(alias.ssh_alias.as_deref(), Some("new-alias"));
        assert!(alias.host.is_none());
        config.servers[0] = alias;
        let direct = edit(&config, &["--host", "new.example"]).unwrap();
        assert_eq!(direct.host.as_deref(), Some("new.example"));
        assert!(direct.ssh_alias.is_none());
        let mut other = ServerProfile::new("production");
        other.host = Some("other.example".into());
        config.servers.push(other);
        assert!(edit(&config, &["--rename", "production"]).is_err());
        assert!(edit(&config, &["--port", "0"]).is_err());
        assert!(edit(&config, &["--rename", "bad/name"]).is_err());
        assert!(edit(&Config::default(), &["--port", "22"]).is_err());
    }

    #[test]
    fn unset_removes_only_the_requested_overrides() {
        let mut config = config();
        config.servers[0].ssh_config = Some("custom-config".into());
        config.servers[0].known_hosts = Some("custom-known-hosts".into());
        let original = config.servers[0].clone();
        let unset = edit(&config, &["--unset", "user,port", "--unset", "identity"]).unwrap();
        assert!(unset.user.is_none());
        assert!(unset.port.is_none());
        assert!(unset.identity_files.is_empty());
        assert_eq!(unset.host, original.host);
        assert_eq!(unset.ssh_config, original.ssh_config);
        assert_eq!(unset.known_hosts, original.known_hosts);
        assert_eq!(unset.proxy_jump, original.proxy_jump);
        let all = edit(
            &config,
            &[
                "--unset",
                "user,port,identity,ssh-config,known-hosts,proxy-jump",
            ],
        )
        .unwrap();
        assert!(all.user.is_none());
        assert!(all.port.is_none());
        assert!(all.identity_files.is_empty());
        assert!(all.ssh_config.is_none());
        assert!(all.known_hosts.is_none());
        assert!(all.proxy_jump.is_empty());
        assert_eq!(all.id, original.id);
        assert_eq!(config.servers[0], original);
    }

    #[test]
    fn setting_and_unsetting_the_same_field_is_rejected_without_mutation() {
        let config = config();
        let original = config.clone();
        for (field, value) in [
            ("user", "bob"),
            ("port", "2222"),
            ("identity", "key"),
            ("ssh-config", "config"),
            ("known-hosts", "known_hosts"),
            ("proxy-jump", "none"),
        ] {
            let flag = format!("--{field}");
            let error = edit(&config, &[&flag, value, "--unset", field])
                .unwrap_err()
                .to_string();
            assert!(error.contains("cannot be combined"), "{field}: {error}");
            assert_eq!(config, original);
        }
        assert!(edit(&config, &["--unset", "unknown"]).is_err());
    }

    #[test]
    fn unset_is_idempotent_and_can_be_combined_with_other_updates() {
        let mut config = config();
        config.servers[0] = edit(&config, &["--unset", "user,identity,proxy-jump"]).unwrap();
        let updated = edit(
            &config,
            &["--unset", "user,user,identity,proxy-jump", "--port", "2200"],
        )
        .unwrap();
        assert_eq!(updated.port, Some(2200));
        assert!(updated.user.is_none());
        assert!(updated.identity_files.is_empty());
        assert!(updated.proxy_jump.is_empty());
    }

    #[test]
    fn server_edit_rejects_empty_connection_overrides_and_ambiguous_jump_lists() {
        let config = config();
        for flag in [
            "--user",
            "--host",
            "--ssh",
            "--identity",
            "--ssh-config",
            "--known-hosts",
            "--proxy-jump",
        ] {
            assert!(edit(&config, &[flag, ""]).is_err(), "accepted empty {flag}");
        }
        assert!(edit(&config, &["--proxy-jump", "none,bastion"]).is_err());
        assert!(edit(&config, &["--proxy-jump", "bastion,NONE"]).is_err());
        assert_eq!(
            edit(&config, &["--proxy-jump", "jump-a,jump-b"])
                .unwrap()
                .proxy_jump,
            ["jump-a", "jump-b"]
        );
    }
}
