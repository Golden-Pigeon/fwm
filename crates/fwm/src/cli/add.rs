//! Prepare and submit a named batch, optionally registering its SSH server.
use super::{args::AddArgs, cleanup, completion, get_config, parse, server_selection};
use crate::offline;
use anyhow::{Result, bail};
use fwm_api::protocol::Command;
use fwm_core::{
    model::{Config, DesiredState, ForwardSpec, ServerProfile, Tunnel, new_id, validate_name},
    paths::Paths,
};

pub async fn run(paths: &Paths, args: AddArgs, json_output: bool) -> Result<()> {
    if args.disabled && (args.wait || args.timeout.is_some()) {
        bail!("--wait cannot be combined with --disabled; --timeout also implies --wait");
    }
    let tunnels = parse::tunnels(
        args.local.as_deref(),
        args.remote.as_deref(),
        args.dynamic.as_deref(),
        &args.ports,
        None,
    )?;
    let config = get_config(paths, false).await?;
    let plan = plan(&config, &args, tunnels)?;
    let ids: Vec<_> = plan
        .forwards
        .iter()
        .map(|forward| forward.id.clone())
        .collect();
    let response = offline::mutate(
        paths,
        Command::CreateForwards {
            forwards: plan.forwards,
            server: plan.server,
        },
        Some(config.revision),
    )
    .await?;
    let response = if args.disabled {
        response
    } else {
        completion::start_saved(paths, response).await?
    };
    let reply = completion::mutation(
        paths,
        response,
        &ids,
        super::args::wait_effective(args.wait, args.timeout),
        json_output,
    )
    .await?;
    if !json_output {
        for id in &ids {
            let forward = reply
                .config
                .forward(id)
                .expect("accepted forward exists in mutation reply");
            let server = reply
                .config
                .server(&forward.server_id)
                .expect("accepted forward has server");
            let target = forward
                .tunnel
                .target()
                .map(ToString::to_string)
                .unwrap_or_else(|| "SOCKS5 destinations".into());
            let (listen_side, target_side) = if forward.tunnel.is_remote() {
                ("remote", "local")
            } else {
                ("local", "remote")
            };
            println!(
                "  {}  [{} / {}]  {} {} -> {} {}",
                forward.name,
                server.name,
                forward.tunnel.kind(),
                listen_side,
                forward.tunnel.listen(),
                target_side,
                target,
            );
            cleanup::describe(forward);
        }
    }
    Ok(())
}

pub(super) struct AddPlan {
    pub forwards: Vec<ForwardSpec>,
    pub server: Option<ServerProfile>,
}

pub(super) fn plan(config: &Config, args: &AddArgs, tunnels: Vec<Tunnel>) -> Result<AddPlan> {
    let explicit_name = args.explicit_name.as_deref().or(args.name.as_deref());
    if let Some(name) = explicit_name {
        validate_name(name).map_err(anyhow::Error::msg)?;
    }
    if let Some(group) = &args.group {
        validate_name(group).map_err(anyhow::Error::msg)?;
    }
    if tunnels.is_empty() {
        bail!("choose a forwarding direction and ports");
    }
    let selected = server_selection::select(config, &args.server, args.ssh_config.as_deref())?;
    let multiple = tunnels.len() > 1;
    let mut forwards = Vec::with_capacity(tunnels.len());
    for tunnel in tunnels {
        let name = match explicit_name {
            Some(name) if multiple => format!("{name}-{}", tunnel.listen().port()),
            Some(name) => name.to_owned(),
            None => automatic_name(&selected.profile.name, &tunnel),
        };
        validate_name(&name).map_err(anyhow::Error::msg)?;
        if config
            .forwards
            .iter()
            .any(|forward| forward.group.as_deref() == Some(&name))
        {
            bail!(
                "name {name:?} is an existing group; use --group {name} to add members and choose a different --name or omit it"
            );
        }
        if config.forward(&name).is_some() {
            bail!("forward {name:?} already exists; use `fwm edit` or choose another --name");
        }
        let (connection_mode, remote_cleanup) = cleanup::effective(
            &tunnel,
            None,
            Some(args.connection_mode),
            args.remote_cleanup,
        )?;
        let group = args.group.clone().or_else(|| {
            multiple.then(|| {
                explicit_name.map_or_else(
                    || automatic_group(&selected.profile, tunnel.kind()),
                    str::to_owned,
                )
            })
        });
        if args.group.is_none()
            && explicit_name.is_none()
            && let Some(group) = &group
            && config.forwards.iter().any(|forward| {
                forward.group.as_ref() == Some(group)
                    && (forward.server_id != selected.profile.id
                        || forward.tunnel.kind() != tunnel.kind())
            })
        {
            bail!(
                "automatic group {group:?} is already used by another server or direction; choose --group GROUP explicitly"
            );
        }
        forwards.push(ForwardSpec {
            id: new_id(),
            name,
            group,
            server_id: selected.profile.id.clone(),
            tunnel,
            desired_state: if args.disabled {
                DesiredState::Stopped
            } else {
                DesiredState::Running
            },
            connection_mode,
            remote_cleanup,
        });
    }
    let server = selected.is_new.then_some(selected.profile);
    let mut candidate = config.clone();
    candidate.servers.extend(server.iter().cloned());
    candidate.forwards.extend(forwards.iter().cloned());
    candidate.validate().map_err(anyhow::Error::msg)?;
    Ok(AddPlan { forwards, server })
}

fn automatic_name(server: &str, tunnel: &Tunnel) -> String {
    append_name(
        server,
        &format!("-{}-{}", tunnel.kind(), tunnel.listen().port()),
    )
}

fn automatic_group(server: &ServerProfile, direction: &str) -> String {
    let suffix = format!("-{direction}");
    if server.name.len() + suffix.len() <= 100 {
        return format!("{}{suffix}", server.name);
    }
    // Stable FNV-1a over the full server identity avoids losing distinct tails
    // when the readable prefix must be shortened. Collision checks still run.
    let hash = server.id.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    append_name(&server.name, &format!("-{hash:016x}{suffix}"))
}

fn append_name(server: &str, suffix: &str) -> String {
    // Names have a 100-byte limit; do not split a Unicode server name when adding
    // the automatic suffix. A collision still produces an explicit rename error.
    let mut end = server.len().min(100 - suffix.len());
    while !server.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &server[..end])
}

#[cfg(test)]
#[path = "add_group_tests.rs"]
mod group_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{Cli, Command as CliCommand};
    use fwm_core::model::{ConnectionMode, RemoteCleanup};

    fn create(config: &Config, arguments: &[&str]) -> Result<AddPlan> {
        let mut argv = vec!["fwm", "add"];
        argv.extend(arguments);
        let CliCommand::Add(args) = Cli::try_parse_from(argv)?.command else {
            unreachable!()
        };
        let tunnels = parse::tunnels(
            args.local.as_deref(),
            args.remote.as_deref(),
            args.dynamic.as_deref(),
            &args.ports,
            None,
        )?;
        plan(config, &args, tunnels)
    }

    #[test]
    fn saved_batch_groups_match_explicit_or_automatic_names() {
        let named = create(
            &Config::default(),
            &[
                "--server",
                "dev",
                "--local",
                "--port",
                "3000-3002",
                "--name",
                "web",
            ],
        )
        .unwrap();
        assert!(
            named
                .forwards
                .iter()
                .all(|rule| rule.group.as_deref() == Some("web"))
        );
        let automatic = create(
            &Config::default(),
            &["--server", "dev", "--remote", "--port", "3000-3002"],
        )
        .unwrap();
        assert!(
            automatic
                .forwards
                .iter()
                .all(|rule| rule.group.as_deref() == Some("dev-remote"))
        );
        let single = create(
            &Config::default(),
            &[
                "--server",
                "dev",
                "--local",
                "--port",
                "3000,3000",
                "--name",
                "web",
            ],
        )
        .unwrap();
        assert!(single.forwards[0].group.is_none());
        assert_eq!(single.forwards[0].name, "web");
    }

    #[test]
    fn remote_cleanup_defaults_to_verified_with_a_dedicated_connection() {
        for flags in [
            vec![],
            vec!["--connection-mode", "shared"],
            vec!["--remote-cleanup", "verified"],
        ] {
            let mut arguments = vec!["--server", "dev", "--remote", "--port", "3000-3001"];
            arguments.extend(flags);
            let batch = create(&Config::default(), &arguments).unwrap();
            assert!(
                batch
                    .forwards
                    .iter()
                    .all(|forward| forward.remote_cleanup == RemoteCleanup::Verified
                        && forward.connection_mode == ConnectionMode::Dedicated)
            );
        }
        let batch = create(
            &Config::default(),
            &[
                "--server",
                "dev",
                "--remote",
                "--port",
                "3000",
                "--remote-cleanup",
                "off",
            ],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].remote_cleanup, RemoteCleanup::Off);
        assert_eq!(batch.forwards[0].connection_mode, ConnectionMode::Shared);
        let batch = create(
            &Config::default(),
            &[
                "--server",
                "dev",
                "--remote",
                "--port",
                "3000",
                "--remote-cleanup",
                "off",
                "--connection-mode",
                "dedicated",
            ],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].connection_mode, ConnectionMode::Dedicated);
    }

    #[test]
    fn local_and_dynamic_forwards_cannot_enable_remote_cleanup() {
        for mut arguments in [
            vec!["--server", "dev", "--local", "--port", "3000"],
            vec!["--server", "dev", "--dynamic", "1080"],
        ] {
            let batch = create(&Config::default(), &arguments).unwrap();
            assert_eq!(batch.forwards[0].remote_cleanup, RemoteCleanup::Off);
            assert_eq!(batch.forwards[0].connection_mode, ConnectionMode::Shared);
            arguments.extend(["--remote-cleanup", "verified"]);
            let error = create(&Config::default(), &arguments)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("requires a remote forward"));
        }
    }

    #[test]
    fn auto_names_all_directions_and_batch_ports() {
        for (direction, expected) in [("--local", "local"), ("--remote", "remote")] {
            let batch = create(
                &Config::default(),
                &[
                    "--server",
                    "example-cluster",
                    direction,
                    "--port",
                    "3002,3000-3001",
                ],
            )
            .unwrap();
            assert_eq!(
                batch
                    .forwards
                    .iter()
                    .map(|forward| forward.name.clone())
                    .collect::<Vec<_>>(),
                [3000, 3001, 3002].map(|port| format!("example-cluster-{expected}-{port}"))
            );
            assert!(
                batch
                    .forwards
                    .iter()
                    .all(|forward| forward.server_id == batch.server.as_ref().unwrap().id)
            );
        }
        let batch = create(
            &Config::default(),
            &["--server", "dev", "--dynamic", "1080"],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].name, "dev-dynamic-1080");
    }

    #[test]
    fn explicit_names_are_preserved_and_batches_keep_port_suffixes() {
        let batch = create(
            &Config::default(),
            &[
                "--server", "dev", "--local", "--port", "3000", "--name", "web",
            ],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].name, "web");
        let batch = create(
            &Config::default(),
            &[
                "--server",
                "dev",
                "--remote",
                "cluster-ssh-mac",
                "--src",
                "12222-12223",
                "--tgt",
                "22",
            ],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].name, "cluster-ssh-mac-12222");
        assert_eq!(batch.forwards[1].name, "cluster-ssh-mac-12223");
    }

    #[test]
    fn auto_name_uses_resolved_profile_name_and_respects_unicode_length() {
        let mut server = ServerProfile::new("dev");
        server.ssh_alias = Some("dev-host".into());
        let config = Config {
            servers: vec![server.clone()],
            ..Default::default()
        };
        let batch = create(&config, &["--server", "dev", "--remote", "--port", "7890"]).unwrap();
        assert!(batch.server.is_none());
        assert_eq!(batch.forwards[0].name, "dev-remote-7890");
        let batch = create(
            &config,
            &["--server", &server.id, "--local", "--port", "7890"],
        )
        .unwrap();
        assert_eq!(batch.forwards[0].name, "dev-local-7890");
        let batch = create(
            &Config::default(),
            &[
                "--server",
                &"测试服".repeat(11),
                "--local",
                "--port",
                "65535",
            ],
        )
        .unwrap();
        validate_name(&batch.forwards[0].name).unwrap();
        assert!(batch.forwards[0].name.ends_with("-local-65535"));
    }

    #[test]
    fn rejects_invalid_batch_without_saving_an_automatic_profile() {
        let config = Config::default();
        let result = create(
            &config,
            &[
                "--server",
                "dev",
                "--local",
                "--port",
                "3000-3001",
                "--name",
                &"x".repeat(96),
            ],
        );
        assert!(result.is_err());
        assert!(config.servers.is_empty());
        assert!(config.forwards.is_empty());
    }
}
