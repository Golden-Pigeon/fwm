use super::*;
use crate::cli::args::{Cli, Command as CliCommand};

fn create(config: &Config, flags: &[&str]) -> Result<AddPlan> {
    let mut argv = vec!["fwm", "add", "--server", "dev", "--disabled"];
    argv.extend(flags);
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

fn saved(plan: AddPlan) -> Config {
    Config {
        servers: plan.server.into_iter().collect(),
        forwards: plan.forwards,
        ..Default::default()
    }
}

#[test]
fn explicit_group_creates_and_appends_one_or_many_members() {
    let mut config = saved(
        create(
            &Config::default(),
            &["--local", "--port", "3000", "--group", "web"],
        )
        .unwrap(),
    );
    assert!(
        config.forwards[0]
            .name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase())
    );
    assert_ne!(config.forwards[0].name, "web");
    for ports in ["3001", "3002-3003"] {
        let plan = create(&config, &["--local", "--port", ports, "--group", "web"]).unwrap();
        assert!(plan.server.is_none());
        config.forwards.extend(plan.forwards);
    }
    assert_eq!(config.select_group_forwards("web").unwrap().len(), 4);
    assert!(
        config
            .forwards
            .iter()
            .all(|forward| forward.group.as_deref() == Some("web"))
    );
    config.validate().unwrap();
}

#[test]
fn explicit_group_is_independent_of_single_name_or_batch_prefix() {
    for ports in ["3000", "3000-3001"] {
        let plan = create(
            &Config::default(),
            &[
                "--local", "--port", ports, "--group", "web", "--name", "api",
            ],
        )
        .unwrap();
        assert!(
            plan.forwards
                .iter()
                .all(|forward| forward.group.as_deref() == Some("web"))
        );
        assert_eq!(
            plan.forwards[0].name,
            if ports == "3000" { "api" } else { "api-3000" }
        );
    }
}

#[test]
fn explicit_group_accepts_dynamic_and_remote_members() {
    for flags in [
        vec!["--dynamic", "1080"],
        vec!["--remote", "--port", "3000"],
    ] {
        let mut arguments = flags;
        arguments.extend(["--group", "tools"]);
        let plan = create(&Config::default(), &arguments).unwrap();
        assert_eq!(plan.forwards[0].group.as_deref(), Some("tools"));
    }
}

#[test]
fn single_name_matching_an_existing_group_explains_how_to_append() {
    let config = saved(
        create(
            &Config::default(),
            &["--local", "--port", "3000-3001", "--name", "web"],
        )
        .unwrap(),
    );
    let error = create(&config, &["--local", "--port", "3002", "--name", "web"])
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("--group web"), "{error}");
    let plan = create(&config, &["--local", "--port", "3002", "--group", "web"]).unwrap();
    assert_eq!(plan.forwards[0].group.as_deref(), Some("web"));
}

#[test]
fn invalid_group_names_and_rule_group_collisions_are_atomic() {
    let config = saved(
        create(
            &Config::default(),
            &["--local", "--port", "3000", "--name", "web"],
        )
        .unwrap(),
    );
    let before = config.clone();
    for group in ["", "bad group", "web"] {
        assert!(create(&config, &["--local", "--port", "3001", "--group", group]).is_err());
    }
    assert!(
        create(
            &Config::default(),
            &[
                "--local", "--port", "3000", "--group", "web", "--name", "web"
            ],
        )
        .is_err()
    );
    assert_eq!(config, before);
}

#[test]
fn duplicate_ports_in_a_stopped_group_are_rejected_even_with_different_names() {
    let config = saved(
        create(
            &Config::default(),
            &[
                "--local", "--port", "3000", "--group", "web", "--name", "first",
            ],
        )
        .unwrap(),
    );
    let error = create(
        &config,
        &[
            "--local", "--port", "3000", "--group", "web", "--name", "second",
        ],
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("conflict within group"), "{error}");
}

#[test]
fn group_options_do_not_weaken_explicit_server_or_direction_requirements() {
    for flags in [
        vec!["fwm", "add", "--group", "web", "--local", "--port", "3000"],
        vec![
            "fwm", "add", "--server", "dev", "--group", "web", "--port", "3000",
        ],
        vec![
            "fwm", "add", "--server", "dev", "--group", "web", "--local", "--remote", "--port",
            "3000",
        ],
    ] {
        assert!(Cli::try_parse_from(flags).is_err());
    }
    for flags in [
        vec!["--group", "123", "--local", "3000"],
        vec!["--local", "3000", "--group", "123"],
        vec!["--group=123", "-L3000"],
    ] {
        let plan = create(&Config::default(), &flags).unwrap();
        assert_eq!(plan.forwards[0].group.as_deref(), Some("123"));
        assert_eq!(plan.forwards[0].tunnel.listen().port(), 3000);
    }
}
