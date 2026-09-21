use super::*;
use crate::cli::args::{EditPortArgs, PortArgs};

fn original(remote: bool, ipv6: bool) -> Tunnel {
    directed(
        remote,
        if ipv6 { "[::1]:1234" } else { "127.0.0.2:1234" }
            .parse()
            .unwrap(),
        "db.internal:5678".parse().unwrap(),
    )
}

#[test]
fn direction_port_shorthand_matches_port_patch_for_every_direction_and_address_family() {
    for was_remote in [false, true] {
        for ipv6 in [false, true] {
            let previous = original(was_remote, ipv6);
            for remote in [false, true] {
                for port in ["1", "3001", "65535"] {
                    let direct = tunnels(
                        (!remote).then_some(port),
                        remote.then_some(port),
                        None,
                        None,
                        &EditPortArgs::default(),
                        Some(&previous),
                    )
                    .unwrap();
                    let patch = tunnels(
                        (!remote).then_some(""),
                        remote.then_some(""),
                        None,
                        None,
                        &EditPortArgs {
                            port: Some(port.into()),
                            ..Default::default()
                        },
                        Some(&previous),
                    )
                    .unwrap();
                    assert_eq!(direct, patch);
                    assert_eq!(direct.len(), 1);
                    assert_eq!(direct[0].is_remote(), remote);
                    assert_eq!(direct[0].listen().ip(), previous.listen().ip());
                    assert_eq!(direct[0].target().unwrap().host, "db.internal");
                    assert_eq!(
                        direct[0].target().unwrap().port,
                        port.parse::<u16>().unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn shorthand_edit_rejects_ranges_lists_and_invalid_ports_without_mutating_input() {
    let previous = original(false, true);
    for value in ["0", "65536", "3000-3001", "3000,3001", "abc", "-1"] {
        for remote in [false, true] {
            assert!(
                tunnels(
                    (!remote).then_some(value),
                    remote.then_some(value),
                    None,
                    None,
                    &EditPortArgs::default(),
                    Some(&previous),
                )
                .is_err(),
                "{value}"
            );
        }
    }
    assert_eq!(previous, original(false, true));
}

#[test]
fn shorthand_create_still_defaults_to_loopback_and_expands_port_lists() {
    for remote in [false, true] {
        let rules = tunnels(
            (!remote).then_some("3000-3001,8080"),
            remote.then_some("3000-3001,8080"),
            None,
            None,
            &PortArgs::default(),
            None,
        )
        .unwrap();
        assert_eq!(rules.len(), 3);
        for (rule, port) in rules.iter().zip([3000, 3001, 8080]) {
            assert_eq!(rule.listen().to_string(), format!("127.0.0.1:{port}"));
            assert_eq!(
                rule.target().unwrap().to_string(),
                format!("localhost:{port}")
            );
        }
    }
}

#[test]
fn explicit_spec_still_replaces_both_addresses_on_edit() {
    let previous = original(false, true);
    let rules = tunnels(
        None,
        Some("127.0.0.3:5000:new.internal:8080"),
        None,
        None,
        &EditPortArgs::default(),
        Some(&previous),
    )
    .unwrap();
    assert_eq!(rules[0].listen().to_string(), "127.0.0.3:5000");
    assert_eq!(rules[0].target().unwrap().to_string(), "new.internal:8080");
    assert!(rules[0].is_remote());
}

#[test]
fn full_mapping_edits_without_bind_preserve_the_original_ip_for_both_directions() {
    for existing in [original(false, true), original(true, false)] {
        for remote in [false, true] {
            let changed = tunnels(
                (!remote).then_some("3001:new.internal:8081"),
                remote.then_some("3001:new.internal:8081"),
                None,
                None,
                &EditPortArgs::default(),
                Some(&existing),
            )
            .unwrap();
            assert_eq!(changed[0].listen().ip(), existing.listen().ip());
            assert_eq!(changed[0].listen().port(), 3001);
            assert_eq!(
                changed[0].target().unwrap().to_string(),
                "new.internal:8081"
            );
            assert_eq!(changed[0].is_remote(), remote);
        }
    }
}

#[test]
fn direction_port_shorthand_converts_dynamic_using_existing_bind_and_default_target_host() {
    let previous = Tunnel::Dynamic {
        listen: "[::1]:1080".parse().unwrap(),
    };
    for remote in [false, true] {
        let rules = tunnels(
            (!remote).then_some("8080"),
            remote.then_some("8080"),
            None,
            None,
            &EditPortArgs::default(),
            Some(&previous),
        )
        .unwrap();
        assert_eq!(rules[0].listen().to_string(), "[::1]:8080");
        assert_eq!(rules[0].target().unwrap().to_string(), "localhost:8080");
        assert_eq!(rules[0].is_remote(), remote);
    }
}

#[test]
fn remote_dynamic_requires_one_valid_listen_address_without_a_destination() {
    for (spec, expected) in [
        ("7897", "127.0.0.1:7897"),
        ("127.0.0.2:7897", "127.0.0.2:7897"),
        ("[::1]:7897", "[::1]:7897"),
    ] {
        let rules = tunnels(None, None, None, Some(spec), &PortArgs::default(), None).unwrap();
        assert_eq!(rules.len(), 1);
        assert!(matches!(rules[0], Tunnel::RemoteDynamic { .. }));
        assert_eq!(rules[0].listen().to_string(), expected);
        assert!(rules[0].target().is_none());
    }
    for spec in [
        "",
        "0",
        "65536",
        "7897-7898",
        "7897,7898",
        "localhost:7897",
        "::1:7897",
        "[::1:7897",
        "7897:localhost:8080",
        "127.0.0.1:0",
    ] {
        assert!(
            tunnels(None, None, None, Some(spec), &PortArgs::default(), None).is_err(),
            "{spec}"
        );
    }
}
