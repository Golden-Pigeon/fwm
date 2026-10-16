//! Two-server relay add planning: defaults, expansions, and rejections.
use super::*;
use crate::cli::args::{Cli, Command as CliCommand};
use fwm_core::model::{ConnectionMode, RemoteCleanup};

fn relay_plan(config: &Config, arguments: &[&str]) -> Result<(AddPlan, Vec<Tunnel>)> {
    let mut argv = vec!["fwm", "add"];
    argv.extend(arguments);
    let CliCommand::Add(args) = Cli::try_parse_from(argv)?.command else {
        unreachable!()
    };
    let specs = parse::relays(
        args.ports.src.as_deref(),
        args.ports.tgt.as_deref(),
        args.ports.port.as_deref(),
    )?;
    let plan = plan(config, &args, Vec::new(), specs)?;
    let tunnels = plan
        .forwards
        .iter()
        .map(|forward| forward.tunnel.clone())
        .collect();
    Ok((plan, tunnels))
}

fn tunnel_of(tunnels: &[Tunnel]) -> (std::net::SocketAddr, String, u16, String) {
    let Tunnel::Relay {
        listen,
        target,
        target_server_id,
    } = &tunnels[0]
    else {
        panic!("expected a relay tunnel");
    };
    (
        *listen,
        target.host.clone(),
        target.port,
        target_server_id.clone(),
    )
}

#[test]
fn relay_add_defaults_to_loopback_and_localhost_target() {
    let (plan, tunnels) = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "host-a",
            "--src",
            "8080",
            "--tgt-server",
            "host-c",
            "--tgt",
            "9000",
        ],
    )
    .unwrap();
    assert_eq!(plan.server.as_ref().unwrap().name, "host-a");
    assert_eq!(plan.target_server.as_ref().unwrap().name, "host-c");
    let (listen, host, port, target_id) = tunnel_of(&tunnels);
    assert_eq!(listen.to_string(), "127.0.0.1:8080");
    assert_eq!(host, "localhost");
    assert_eq!(port, 9000);
    assert_eq!(target_id, plan.target_server.as_ref().unwrap().id);
    // Relay rules always isolate both endpoints with verified cleanup.
    assert!(plan.forwards.iter().all(|forward| {
        forward.remote_cleanup == RemoteCleanup::Verified
            && forward.connection_mode == ConnectionMode::Dedicated
    }));
}

#[test]
fn relay_add_expands_ports_addresses_and_target_hosts() {
    let (_, tunnels) = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "a",
            "--src",
            "0.0.0.0:3000-3001",
            "--tgt-server",
            "c",
            "--tgt",
            "db.internal:5432",
        ],
    )
    .unwrap();
    let ports: Vec<_> = tunnels
        .iter()
        .map(|tunnel| tunnel.listen().to_string())
        .collect();
    assert_eq!(ports, ["0.0.0.0:3000", "0.0.0.0:3001"]);
    assert!(
        tunnels
            .iter()
            .all(|tunnel| tunnel.target().unwrap().host == "db.internal"
                && tunnel.target().unwrap().port == 5432)
    );

    let (_, same_port) = relay_plan(
        &Config::default(),
        &["--src-server", "a", "--tgt-server", "c", "--port", "8080"],
    )
    .unwrap();
    let (listen, host, port, _) = tunnel_of(&same_port);
    assert_eq!(listen.to_string(), "127.0.0.1:8080");
    assert_eq!(host, "localhost");
    assert_eq!(port, 8080);

    let (_, ipv6) = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "a",
            "--src",
            "[::1]:8080",
            "--tgt-server",
            "c",
            "--tgt",
            "[::1]:9000",
        ],
    )
    .unwrap();
    let (listen, host, _, _) = tunnel_of(&ipv6);
    assert_eq!(listen.to_string(), "[::1]:8080");
    assert_eq!(host, "::1");
    // Multi-port relay batches share one target host across members.
    let (_, batch) = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "a",
            "--src",
            "3000-3001",
            "--tgt-server",
            "c",
            "--tgt",
            "db.internal:5432",
        ],
    )
    .unwrap();
    assert!(
        batch
            .iter()
            .all(|tunnel| tunnel.target().unwrap().host == "db.internal")
    );
}

#[test]
fn relay_add_rejects_shared_mode_and_invalid_hosts_or_ports() {
    let error = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "c",
            "--tgt",
            "9000",
            "--connection-mode",
            "shared",
        ],
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("shared"), "{error}");
    for arguments in [
        vec![
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "c",
            "--tgt",
            "0",
        ],
        vec![
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "c",
            "--tgt",
            "bad host:9000",
        ],
        vec![
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "c",
            "--tgt",
            "2001:zz:9000",
        ],
        vec![
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "c",
            "--tgt",
            "db.internal:0",
        ],
        vec![
            "--src-server",
            "a",
            "--src",
            "3000-3001",
            "--tgt-server",
            "c",
            "--tgt",
            "9000",
            "--port",
            "8080",
        ],
    ] {
        assert!(
            relay_plan(&Config::default(), &arguments).is_err(),
            "{arguments:?}"
        );
    }
}

#[test]
fn relay_flags_conflict_with_single_server_and_direction_flags() {
    for arguments in [
        vec![
            "fwm",
            "add",
            "--server",
            "dev",
            "--src-server",
            "a",
            "--tgt-server",
            "c",
            "--src",
            "8080",
            "--tgt",
            "9000",
        ],
        vec![
            "fwm",
            "add",
            "--local",
            "--src-server",
            "a",
            "--tgt-server",
            "c",
            "--src",
            "8080",
            "--tgt",
            "9000",
        ],
        vec![
            "fwm",
            "add",
            "--remote",
            "--src-server",
            "a",
            "--tgt-server",
            "c",
            "--src",
            "8080",
            "--tgt",
            "9000",
        ],
        vec![
            "fwm",
            "add",
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt",
            "9000",
        ],
        vec![
            "fwm",
            "add",
            "--tgt-server",
            "c",
            "--src",
            "8080",
            "--tgt",
            "9000",
        ],
    ] {
        assert!(
            Cli::try_parse_from(arguments.clone()).is_err(),
            "{arguments:?}"
        );
    }
    // A relay pair without ports parses but is rejected with guidance.
    let error = relay_plan(
        &Config::default(),
        &["--src-server", "a", "--tgt-server", "c"],
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("--src"), "{error}");
    assert!(error.contains("--port"), "{error}");
}

#[test]
fn relay_add_keeps_existing_profiles_and_allows_both_roles() {
    let mut a = fwm_core::model::ServerProfile::new("a");
    a.host = Some("127.0.0.1".into());
    let config = Config {
        servers: vec![a.clone()],
        ..Config::default()
    };
    let (plan, _) = relay_plan(
        &config,
        &[
            "--src-server",
            "a",
            "--src",
            "8080",
            "--tgt-server",
            "a",
            "--tgt",
            "9000",
        ],
    )
    .unwrap();
    assert!(plan.server.is_none());
    assert!(plan.target_server.is_none());
    assert_eq!(plan.forwards[0].server_id, a.id);
    assert_eq!(
        plan.forwards[0].tunnel.target_server_id().unwrap(),
        a.id.as_str()
    );
    // Both new aliases and the rule commit as one candidate batch.
    let (fresh, _) = relay_plan(
        &Config::default(),
        &[
            "--src-server",
            "alias-a",
            "--src",
            "8080",
            "--tgt-server",
            "alias-c",
            "--tgt",
            "9000",
        ],
    )
    .unwrap();
    assert!(fresh.server.is_some() && fresh.target_server.is_some());
    let mut candidate = Config::default();
    candidate.servers.extend(
        [fresh.server.clone(), fresh.target_server.clone()]
            .into_iter()
            .flatten(),
    );
    candidate.forwards.extend(fresh.forwards.clone());
    candidate.validate().unwrap();
}
