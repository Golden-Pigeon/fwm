use super::*;

#[test]
fn numeric_and_range_names_are_not_consumed_by_direction_flags_before_the_name() {
    for name in ["1234", "3000-3001", "65536"] {
        for direction in ["--local", "--remote", "-L", "-R"] {
            for argv in [
                vec!["fwm", "edit", direction, name],
                vec!["fwm", "edit", name, direction],
            ] {
                let Command::Edit(args) = Cli::try_parse_from(&argv).unwrap().command else {
                    unreachable!()
                };
                assert_eq!(args.name, name);
                assert_eq!(args.local.as_deref().or(args.remote.as_deref()), Some(""));
            }
        }
    }
}

#[test]
fn edit_numeric_names_keep_unambiguous_legacy_and_explicit_port_specifications() {
    for argv in [
        vec!["fwm", "edit", "1234", "-L", "3000"],
        vec!["fwm", "edit", "-L3000", "1234"],
        vec!["fwm", "edit", "--local=3000", "1234"],
        vec!["fwm", "edit", "--local", "3000", "1234"],
        vec!["fwm", "edit", "--local", "3000", "--", "1234"],
        vec![
            "fwm", "edit", "--local", "3000", "--rename", "later", "1234",
        ],
        vec!["fwm", "edit", "--local", "3000:host:80", "1234"],
    ] {
        let Command::Edit(args) = Cli::try_parse_from(&argv).unwrap().command else {
            unreachable!()
        };
        assert_eq!(args.name, "1234");
        assert!(args.local.as_deref().unwrap().starts_with("3000"));
    }
}

#[test]
fn numeric_name_is_not_mistaken_for_spec_when_only_metadata_values_follow() {
    for argv in [
        vec!["fwm", "edit", "--remote", "1234", "--rename", "456"],
        vec!["fwm", "edit", "--remote", "1234", "--group", "456"],
        vec!["fwm", "edit", "--remote", "1234", "--server", "456"],
    ] {
        let Command::Edit(args) = Cli::try_parse_from(argv).unwrap().command else {
            unreachable!()
        };
        assert_eq!(args.name, "1234");
        assert_eq!(args.remote.as_deref(), Some(""));
    }
}

#[test]
fn explicit_timeout_implies_wait_for_every_activation_command() {
    assert_eq!(wait_effective(false, None), None);
    assert_eq!(wait_effective(true, None), Some(Duration::from_secs(20)));
    for command in ["add", "up", "restart"] {
        let mut argv = vec!["fwm", command];
        if command == "add" {
            argv.extend(["--server", "dev", "--local", "--port", "3000"]);
        } else {
            argv.push("web");
        }
        argv.extend(["--timeout", "500ms"]);
        let (wait, timeout) = match Cli::try_parse_from(argv).unwrap().command {
            Command::Add(args) => (args.wait, args.timeout),
            Command::Up(args) | Command::Restart(args) => (args.wait, args.timeout),
            _ => unreachable!(),
        };
        assert!(!wait);
        assert_eq!(
            wait_effective(wait, timeout),
            Some(Duration::from_millis(500))
        );
    }
}

#[test]
fn group_management_arguments_are_discoverable_and_membership_flags_are_exclusive() {
    assert!(matches!(
        Cli::try_parse_from(["fwm", "group", "list"])
            .unwrap()
            .command,
        Command::Group {
            command: GroupCommand::List
        }
    ));
    for flags in [
        vec!["--group", "apps"],
        vec!["--ungroup"],
        vec!["--rename", "api", "--group", "apps"],
    ] {
        let mut argv = vec!["fwm", "edit", "web"];
        argv.extend(flags);
        assert!(Cli::try_parse_from(argv).is_ok());
    }
    assert!(Cli::try_parse_from(["fwm", "edit", "web", "--group", "apps", "--ungroup"]).is_err());
}
