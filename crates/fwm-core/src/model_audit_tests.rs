use super::*;
use serde_json::{Value, json};

fn wire(kind: &str) -> Value {
    let mut value = json!({"id":"rule-id","name":"web","server_id":"server-id","kind":kind,"listen":"127.0.0.1:3000","desired_state":"stopped"});
    if !matches!(kind, "dynamic" | "remote_dynamic") {
        value["target"] = json!("db.internal:8080");
    }
    value
}

fn fixture() -> Config {
    let mut server = ServerProfile::new("dev");
    server.id = "server-id".into();
    server.host = Some("127.0.0.1".into());
    Config {
        servers: vec![server],
        forwards: vec![serde_json::from_value(wire("local")).unwrap()],
        ..Default::default()
    }
}

#[test]
fn unknown_forward_fields_never_turn_misspelled_stopped_intent_into_running() {
    for field in [
        "desired_sate",
        "connection_mdoe",
        "remote_celanup",
        "listen_port",
        "tunnel",
    ] {
        let mut value = wire("remote");
        value.as_object_mut().unwrap().remove("desired_state");
        value[field] = json!("stopped");
        let error = serde_json::from_value::<ForwardSpec>(value.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains(field), "{error}");
        let text = toml::to_string(&value).unwrap();
        let error = toml::from_str::<ForwardSpec>(&text)
            .unwrap_err()
            .to_string();
        assert!(error.contains(field), "{error}");
    }
}

#[test]
fn server_typos_and_tunnel_incompatible_fields_are_rejected_in_json_and_toml() {
    let server = json!({"name":"dev","host":"localhost","usr":"fixture"});
    assert!(
        serde_json::from_value::<ServerProfile>(server.clone())
            .unwrap_err()
            .to_string()
            .contains("usr")
    );
    assert!(
        toml::from_str::<ServerProfile>(&toml::to_string(&server).unwrap())
            .unwrap_err()
            .to_string()
            .contains("usr")
    );
    for kind in ["dynamic", "remote_dynamic"] {
        let mut dynamic = wire(kind);
        dynamic["target"] = json!("localhost:80");
        assert!(serde_json::from_value::<ForwardSpec>(dynamic.clone()).is_err());
        assert!(toml::from_str::<ForwardSpec>(&toml::to_string(&dynamic).unwrap()).is_err());
    }
    for kind in ["local", "remote"] {
        let mut direct = wire(kind);
        direct.as_object_mut().unwrap().remove("target");
        assert!(serde_json::from_value::<ForwardSpec>(direct).is_err());
    }
}

#[test]
fn direction_defaults_match_cli_and_explicit_cleanup_off_is_retained() {
    for kind in ["local", "remote", "dynamic", "remote_dynamic"] {
        let rule: ForwardSpec = serde_json::from_value(wire(kind)).unwrap();
        assert_eq!(
            rule.remote_cleanup,
            if matches!(kind, "remote" | "remote_dynamic") {
                RemoteCleanup::Verified
            } else {
                RemoteCleanup::Off
            }
        );
        assert_eq!(
            rule.connection_mode,
            if matches!(kind, "remote" | "remote_dynamic") {
                ConnectionMode::Dedicated
            } else {
                ConnectionMode::Shared
            }
        );
        assert_eq!(rule.desired_state, DesiredState::Stopped);
        assert_eq!(
            serde_json::from_str::<ForwardSpec>(&serde_json::to_string(&rule).unwrap()).unwrap(),
            rule
        );
        assert_eq!(
            toml::from_str::<ForwardSpec>(&toml::to_string(&rule).unwrap()).unwrap(),
            rule
        );
    }
    for kind in ["remote", "remote_dynamic"] {
        for mode in [None, Some("shared"), Some("dedicated")] {
            let mut value = wire(kind);
            value["remote_cleanup"] = json!("off");
            if let Some(mode) = mode {
                value["connection_mode"] = json!(mode);
            }
            let rule: ForwardSpec = serde_json::from_value(value).unwrap();
            assert_eq!(rule.remote_cleanup, RemoteCleanup::Off);
            assert_eq!(
                rule.connection_mode,
                if mode == Some("dedicated") {
                    ConnectionMode::Dedicated
                } else {
                    ConnectionMode::Shared
                }
            );
        }
        let mut value = wire(kind);
        value["connection_mode"] = json!("shared");
        assert_eq!(
            serde_json::from_value::<ForwardSpec>(value)
                .unwrap()
                .connection_mode,
            ConnectionMode::Dedicated
        );
    }
}

#[test]
fn bracketed_targets_must_be_valid_ipv6_with_a_valid_optional_scope() {
    for value in [
        "[2001:::1]:80",
        "[not:ipv6]:80",
        "[example.org]:80",
        "[::1]:0",
        "[::1]:65536",
        "[::1%]:80",
        "[::1%a%b]:80",
        "foo[bar]:80",
    ] {
        assert!(value.parse::<Endpoint>().is_err(), "{value}");
    }
    for value in [
        "[::1]:1",
        "[2001:db8::1]:65535",
        "[::ffff:127.0.0.1]:80",
        "[fe80::1%en0]:80",
        "db.internal:80",
    ] {
        assert_eq!(value.parse::<Endpoint>().unwrap().to_string(), value);
    }
}

#[test]
fn foreign_rule_server_ids_and_group_ids_cannot_be_claimed_as_names() {
    let mut config = fixture();
    let mut second = config.forwards[0].clone();
    second.id = "second-id".into();
    second.name = "second".into();
    config.forwards.push(second);
    config.forwards[0].name = "second-id".into();
    assert_eq!(
        config.forward("second-id").unwrap().id,
        "second-id",
        "ID selection wins even before validation"
    );
    assert!(
        config
            .validate()
            .unwrap_err()
            .contains("another forward's ID")
    );
    config.forwards[0].name = "first".into();
    config.forwards[0].group = Some("second-id".into());
    assert!(config.validate().unwrap_err().contains("forward ID"));
    config.forwards[0].group = None;
    let mut other = ServerProfile::new("other");
    other.id = "other-id".into();
    other.host = Some("127.0.0.1".into());
    config.servers.push(other);
    config.servers[0].name = "other-id".into();
    assert_eq!(config.server("other-id").unwrap().id, "other-id");
    assert!(
        config
            .validate()
            .unwrap_err()
            .contains("another server's ID")
    );
}

#[test]
fn overlap_respects_families_and_ipv4_mapping_in_both_argument_orders() {
    for (a, b, conflict) in [
        ("0.0.0.0:3000", "[::1]:3000", false),
        ("127.0.0.1:3000", "[::1]:3000", false),
        ("127.0.0.1:3000", "[::ffff:127.0.0.1]:3000", true),
        ("0.0.0.0:3000", "[::ffff:127.0.0.1]:3000", true),
        ("[::ffff:0.0.0.0]:3000", "127.0.0.1:3000", true),
        ("127.0.0.1:3000", "[::]:3000", true),
        ("[::]:3000", "[::1]:3000", true),
        ("127.0.0.1:3000", "127.0.0.2:3000", false),
        ("127.0.0.1:3000", "[::ffff:127.0.0.1]:3001", false),
    ] {
        let a = a.parse().unwrap();
        let b = b.parse().unwrap();
        assert_eq!(overlaps(a, b), conflict, "{a} / {b}");
        assert_eq!(overlaps(b, a), conflict, "{b} / {a}");
    }
}
