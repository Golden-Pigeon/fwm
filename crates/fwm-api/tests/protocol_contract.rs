use fwm_api::protocol::{API_VERSION, Command, Request};
use serde_json::json;

#[test]
fn requests_accept_absent_revision_but_reject_invalid_revision_types() {
    let mut wire =
        json!({"api_version":API_VERSION,"request_id":"ui-1","command":{"method":"ping"}});
    let request: Request = serde_json::from_value(wire.clone()).unwrap();
    assert!(request.expected_revision.is_none());
    wire["expected_revision"] = json!(u64::MAX);
    assert_eq!(
        serde_json::from_value::<Request>(wire.clone())
            .unwrap()
            .expected_revision,
        Some(u64::MAX)
    );
    for invalid in [json!(-1), json!("42"), json!(1.5)] {
        wire["expected_revision"] = invalid;
        assert!(serde_json::from_value::<Request>(wire.clone()).is_err());
    }
}

#[test]
fn grouped_control_wire_format_is_stable_for_other_ui_clients() {
    let wire = json!({"api_version":API_VERSION,"request_id":"ui-1","expected_revision":12,"command":{"method":"restart","params":{"selection":{"by":"group","value":"web"}}}});
    let request: Request = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(request).unwrap(), wire);
    for command in [
        json!({"method":"unknown"}),
        json!({"method":"restart"}),
        json!({"method":"restart","params":{"selection":{"by":"group"}}}),
        json!({"method":"retry","params":{"selection":{"by":"invalid","value":"web"}}}),
        json!({"method":"set_desired","params":{"selection":{"by":"all"},"state":"invalid"}}),
    ] {
        assert!(serde_json::from_value::<Command>(command).is_err());
    }
}

#[test]
fn revision_guard_classification_covers_every_public_command() {
    let server = json!({"id":"server-1","name":"dev","host":"127.0.0.1"});
    let forward = json!({"id":"rule-1","name":"web","server_id":"server-1","kind":"dynamic","listen":"127.0.0.1:3000"});
    let cases = [
        ("ping", None, false),
        ("get_config", None, false),
        ("put_server", Some(json!({"server":server})), true),
        ("remove_server", Some(json!({"selector":"dev"})), true),
        ("put_forward", Some(json!({"forward":forward})), true),
        (
            "put_forward_with_server",
            Some(json!({"forward":forward})),
            true,
        ),
        (
            "put_forwards_with_server",
            Some(json!({"forwards":[forward]})),
            true,
        ),
        ("create_forwards", Some(json!({"forwards":[forward]})), true),
        ("remove_forward", Some(json!({"selector":"web"})), true),
        (
            "remove_forwards",
            Some(json!({"selection":{"by":"all"}})),
            true,
        ),
        (
            "set_desired",
            Some(json!({"selection":{"by":"all"},"state":"stopped"})),
            true,
        ),
        ("retry", Some(json!({"selection":{"by":"all"}})), false),
        ("restart", Some(json!({"selection":{"by":"all"}})), true),
        ("status", None, false),
        ("events", Some(json!({"after":0})), false),
        ("reload", None, true),
        ("validate", None, false),
        ("doctor", Some(json!({"server":null})), false),
        ("inspect_host", Some(json!({"server":"dev"})), false),
        (
            "inspect_host_profile",
            Some(json!({"server":server})),
            false,
        ),
        (
            "inspect_hop_profile",
            Some(json!({"server":server,"hop":"jump"})),
            false,
        ),
        (
            "trust_hop_profile",
            Some(json!({"server":server,"hop":"1","fingerprint":"SHA256:test"})),
            false,
        ),
        (
            "trust_host_profile",
            Some(json!({"server":server,"fingerprint":"SHA256:test"})),
            false,
        ),
        ("doctor_profile", Some(json!({"server":server})), false),
        (
            "trust_host",
            Some(json!({"server":"dev","fingerprint":"SHA256:test"})),
            false,
        ),
        ("shutdown", None, false),
    ];
    for (method, params, guarded) in cases {
        let mut wire = json!({"method":method});
        if let Some(params) = params {
            wire["params"] = params;
        }
        let command: Command =
            serde_json::from_value(wire).unwrap_or_else(|error| panic!("{method}: {error}"));
        assert_eq!(
            command.mutates_config(),
            guarded,
            "{method} has the wrong revision guard"
        );
        assert_eq!(serde_json::to_value(&command).unwrap()["method"], method);
    }
}
