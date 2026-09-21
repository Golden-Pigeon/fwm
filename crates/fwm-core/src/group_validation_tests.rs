use super::*;

fn pair(remote: bool, first_bind: &str, second_bind: &str) -> Config {
    let mut server = ServerProfile::new("dev");
    server.ssh_alias = Some("dev".into());
    let forwards = [first_bind, second_bind]
        .into_iter()
        .enumerate()
        .map(|(index, bind)| {
            let listen = bind.parse().unwrap();
            let target = "db.internal:8080".parse().unwrap();
            ForwardSpec {
                id: format!("id-{index}"),
                name: format!("rule-{index}"),
                group: Some("web".into()),
                server_id: server.id.clone(),
                tunnel: if remote {
                    Tunnel::Remote { listen, target }
                } else {
                    Tunnel::Local { listen, target }
                },
                desired_state: DesiredState::Stopped,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            }
        })
        .collect();
    Config {
        servers: vec![server],
        forwards,
        ..Default::default()
    }
}

#[test]
fn group_listener_conflicts_are_rejected_for_all_member_states_and_directions() {
    for remote in [false, true] {
        for (first, second) in [
            ("127.0.0.1:3000", "127.0.0.1:3000"),
            ("0.0.0.0:3000", "127.0.0.1:3000"),
            ("[::]:3000", "[::1]:3000"),
            ("[::1]:3000", "[::1]:3000"),
        ] {
            for a in [DesiredState::Running, DesiredState::Stopped] {
                for b in [DesiredState::Running, DesiredState::Stopped] {
                    let mut config = pair(remote, first, second);
                    config.forwards[0].desired_state = a;
                    config.forwards[1].desired_state = b;
                    let error = config.validate().unwrap_err();
                    assert!(error.contains("conflict within group \"web\""), "{error}");
                }
            }
        }
    }
}

#[test]
fn group_members_on_distinct_addresses_or_ports_are_valid() {
    for remote in [false, true] {
        for (first, second) in [
            ("127.0.0.1:3000", "127.0.0.1:3001"),
            ("127.0.0.1:3000", "127.0.0.2:3000"),
            ("[::1]:3000", "[::2]:3000"),
        ] {
            pair(remote, first, second).validate().unwrap();
        }
    }
}

#[test]
fn separate_stopped_alternatives_may_reuse_ports_but_running_rules_may_not() {
    for second_group in [None, Some("other".into())] {
        let mut config = pair(false, "127.0.0.1:3000", "127.0.0.1:3000");
        config.forwards[1].group = second_group;
        config.validate().unwrap();
        config.forwards[0].desired_state = DesiredState::Running;
        config.validate().unwrap();
        config.forwards[1].desired_state = DesiredState::Running;
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("listen addresses conflict")
        );
    }
}

#[test]
fn same_group_can_reuse_ports_on_distinct_remote_servers_or_distinct_sides() {
    let mut config = pair(true, "127.0.0.1:3000", "127.0.0.1:3000");
    let mut server = ServerProfile::new("other");
    server.ssh_alias = Some("other".into());
    config.forwards[1].server_id = server.id.clone();
    config.servers.push(server);
    config.validate().unwrap();
    config.forwards[1].server_id = config.forwards[0].server_id.clone();
    config.forwards[1].tunnel = Tunnel::Local {
        listen: "127.0.0.1:3000".parse().unwrap(),
        target: "db.internal:8080".parse().unwrap(),
    };
    config.validate().unwrap();
}

#[test]
fn dynamic_and_local_members_share_the_same_local_listener_namespace() {
    let mut config = pair(false, "127.0.0.1:3000", "127.0.0.1:3000");
    config.forwards[1].tunnel = Tunnel::Dynamic {
        listen: "127.0.0.1:3000".parse().unwrap(),
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .contains("conflict within group")
    );
}

#[test]
fn remote_dynamic_conflicts_with_remote_listeners_on_the_same_server() {
    for (first, second) in [
        ("127.0.0.1:7897", "127.0.0.1:7897"),
        ("0.0.0.0:7897", "127.0.0.1:7897"),
        ("[::]:7897", "[::1]:7897"),
    ] {
        for dynamic_pair in [false, true] {
            let mut config = pair(true, first, second);
            config.forwards[1].tunnel = Tunnel::RemoteDynamic {
                listen: second.parse().unwrap(),
            };
            if dynamic_pair {
                config.forwards[0].tunnel = Tunnel::RemoteDynamic {
                    listen: first.parse().unwrap(),
                };
            }
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .contains("conflict within group")
            );
            for rule in &mut config.forwards {
                rule.group = None;
                rule.desired_state = DesiredState::Running;
            }
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .contains("listen addresses conflict")
            );
        }
    }
}

#[test]
fn remote_dynamic_can_reuse_local_ports_and_other_servers_remote_ports() {
    let mut config = pair(false, "127.0.0.1:7897", "127.0.0.1:7897");
    config.forwards[1].tunnel = Tunnel::RemoteDynamic {
        listen: "127.0.0.1:7897".parse().unwrap(),
    };
    config.validate().unwrap();
    config.forwards[0].tunnel = Tunnel::Dynamic {
        listen: "127.0.0.1:7897".parse().unwrap(),
    };
    config.validate().unwrap();

    let mut server = ServerProfile::new("other");
    server.ssh_alias = Some("other".into());
    config.forwards[0].server_id = server.id.clone();
    config.forwards[0].tunnel = config.forwards[1].tunnel.clone();
    config.servers.push(server);
    config.validate().unwrap();
}
