use super::*;
use crate::model::Config;

fn fixture(settings: &str) -> (tempfile::TempDir, ServerProfile) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".ssh")).unwrap();
    std::fs::write(
        directory.path().join(".ssh/config"),
        format!("Host dev\n HostName inherited.example\n User inherited\n {settings}\n"),
    )
    .unwrap();
    let mut profile = ServerProfile::new("dev");
    profile.ssh_alias = Some("dev".into());
    (directory, profile)
}

#[test]
fn openssh_boolean_aliases_are_respected_and_invalid_values_are_rejected() {
    for (value, expected) in [
        ("yes", true),
        ("true", true),
        ("no", false),
        ("false", false),
    ] {
        let (directory, profile) = fixture(&format!(
            "IdentitiesOnly {value}\n StrictHostKeyChecking true\n ForwardAgent false"
        ));
        assert_eq!(
            resolve_with_home(&profile, directory.path())
                .unwrap()
                .identities_only,
            expected
        );
    }
    for value in ["1", "potato"] {
        let (directory, profile) = fixture(&format!("IdentitiesOnly {value}"));
        assert!(
            resolve_with_home(&profile, directory.path())
                .unwrap_err()
                .to_string()
                .contains("IdentitiesOnly expects")
        );
    }
}

#[test]
fn relative_paths_inside_included_ssh_files_have_a_stable_source_directory() {
    let (directory, profile) = fixture("Include placeholder");
    let sub = directory.path().join(".ssh/parts");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(
        sub.join("dev.conf"),
        "IdentityFile keys/%h\nIdentityAgent agent.sock\nUserKnownHostsFile trusted_hosts\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join(".ssh/config"),
        format!(
            "Host dev\n HostName fixture\n Include {}\n",
            sub.join("dev.conf").display()
        ),
    )
    .unwrap();
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    let sub = std::fs::canonicalize(sub).unwrap();
    assert_eq!(resolved.identity_files, [sub.join("keys/fixture")]);
    assert_eq!(resolved.known_hosts, sub.join("trusted_hosts"));
    assert_eq!(
        resolved.identity_agent.as_deref(),
        sub.join("agent.sock").to_str()
    );
}

#[test]
fn proxy_jump_inherits_overrides_disables_and_restores_inheritance() {
    let (directory, mut profile) = fixture("ProxyJump inherited-a,inherited-b");
    let resolve = |profile: &ServerProfile| resolve_with_home(profile, directory.path()).unwrap();
    assert_eq!(resolve(&profile).proxy_jump, ["inherited-a", "inherited-b"]);
    profile.proxy_jump = vec!["explicit-a".into(), "explicit-b".into()];
    assert_eq!(resolve(&profile).proxy_jump, ["explicit-a", "explicit-b"]);
    for none in ["none", "NONE", "NoNe"] {
        profile.proxy_jump = vec![none.into()];
        assert!(resolve(&profile).proxy_jump.is_empty());
    }
    profile.proxy_jump.clear();
    assert_eq!(resolve(&profile).proxy_jump, ["inherited-a", "inherited-b"]);
}

#[test]
fn explicit_jump_can_override_ssh_config_none() {
    let (directory, mut profile) = fixture("ProxyJump none");
    assert!(
        resolve_with_home(&profile, directory.path())
            .unwrap()
            .proxy_jump
            .is_empty()
    );
    profile.proxy_jump = vec!["explicit".into()];
    assert_eq!(
        resolve_with_home(&profile, directory.path())
            .unwrap()
            .proxy_jump,
        ["explicit"]
    );
}

#[test]
fn connection_overrides_take_precedence_and_unset_follows_updated_ssh_config() {
    let (directory, mut profile) = fixture(
        "Port 2222\n IdentityFile ~/inherited-key-a\n IdentityFile ~/inherited-key-b\n UserKnownHostsFile ~/inherited-hosts",
    );
    profile.host = Some("explicit.example".into());
    profile.user = Some("explicit".into());
    profile.port = Some(2200);
    profile.identity_files = vec!["~/explicit-key".into()];
    profile.known_hosts = Some("~/explicit-hosts".into());
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "explicit.example");
    assert_eq!(resolved.user, "explicit");
    assert_eq!(resolved.port, 2200);
    assert_eq!(
        resolved.identity_files,
        [directory.path().join("explicit-key")]
    );
    assert_eq!(
        resolved.known_hosts,
        directory.path().join("explicit-hosts")
    );
    profile.host = None;
    profile.user = None;
    profile.port = None;
    profile.identity_files.clear();
    profile.known_hosts = None;
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "inherited.example");
    assert_eq!(resolved.user, "inherited");
    assert_eq!(resolved.port, 2222);
    assert_eq!(
        resolved.identity_files,
        [
            directory.path().join("inherited-key-a"),
            directory.path().join("inherited-key-b")
        ]
    );
    assert_eq!(
        resolved.known_hosts,
        directory.path().join("inherited-hosts")
    );
    std::fs::write(
        directory.path().join(".ssh/config"),
        "Host dev\n User changed\n Port 2223\n",
    )
    .unwrap();
    let changed = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(changed.user, "changed");
    assert_eq!(changed.port, 2223);
}

#[test]
fn unsetting_custom_config_restores_default_config_and_default_paths() {
    let (directory, mut profile) = fixture("");
    let custom = directory.path().join("custom_config");
    std::fs::write(
        &custom,
        "Host dev\n User custom\n Port 2200\n IdentityFile none\n",
    )
    .unwrap();
    profile.ssh_config = Some(custom.clone());
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.user, "custom");
    assert_eq!(resolved.port, 2200);
    assert_eq!(resolved.ssh_config, custom);
    assert!(resolved.identity_files.is_empty());
    profile.ssh_config = None;
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.user, "inherited");
    assert_eq!(resolved.port, 22);
    assert_eq!(resolved.ssh_config, directory.path().join(".ssh/config"));
    assert_eq!(
        resolved.known_hosts,
        directory.path().join(".ssh/known_hosts")
    );
    assert_eq!(
        resolved.identity_files,
        ["id_ed25519", "id_ecdsa", "id_rsa"].map(|key| directory.path().join(".ssh").join(key))
    );
}

#[test]
fn identity_unset_restores_configured_authentication_options() {
    let (directory, mut profile) =
        fixture("IdentityFile none\n IdentityAgent none\n IdentitiesOnly yes");
    profile.identity_files = vec!["~/override".into()];
    assert_eq!(
        resolve_with_home(&profile, directory.path())
            .unwrap()
            .identity_files,
        [directory.path().join("override")]
    );
    profile.identity_files.clear();
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert!(resolved.identity_files.is_empty());
    assert_eq!(resolved.identity_agent.as_deref(), Some("none"));
    assert!(resolved.identities_only);
}

#[test]
fn invalid_direct_overrides_are_rejected_before_configuration_or_network_access() {
    let (directory, original) = fixture("");
    for field in [
        "user",
        "host",
        "alias",
        "identity",
        "ssh_config",
        "known_hosts",
        "proxy_jump",
        "mixed_proxy_jump",
        "port",
    ] {
        let mut profile = original.clone();
        match field {
            "user" => profile.user = Some(String::new()),
            "host" => profile.host = Some(String::new()),
            "alias" => profile.ssh_alias = Some(String::new()),
            "identity" => profile.identity_files = vec![PathBuf::new()],
            "ssh_config" => profile.ssh_config = Some(PathBuf::new()),
            "known_hosts" => profile.known_hosts = Some(PathBuf::new()),
            "proxy_jump" => profile.proxy_jump = vec![String::new()],
            "mixed_proxy_jump" => profile.proxy_jump = vec!["none".into(), "jump".into()],
            "port" => profile.port = Some(0),
            _ => unreachable!(),
        }
        assert!(
            resolve_with_home(&profile, directory.path()).is_err(),
            "accepted {field}"
        );
        let config = Config {
            servers: vec![profile],
            ..Config::default()
        };
        assert!(
            config.validate().is_err(),
            "config validation accepted {field}"
        );
    }
}

#[test]
fn disable_and_inherit_states_survive_toml_round_trips() {
    let (_, mut server) = fixture("");
    for jumps in [
        vec![],
        vec!["none".into()],
        vec!["jump-a".into(), "jump-b".into()],
    ] {
        server.proxy_jump = jumps;
        let config = Config {
            servers: vec![server.clone()],
            ..Config::default()
        };
        config.validate().unwrap();
        let decoded: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(decoded, config);
    }
}

#[test]
fn include_host_scope_does_not_escape_into_its_parent() {
    let (directory, profile) = fixture("Include child\n Port 23456\n");
    std::fs::write(
        directory.path().join(".ssh/child"),
        "Host other\n Port 19999\n",
    )
    .unwrap();
    assert_eq!(
        resolve_with_home(&profile, directory.path()).unwrap().port,
        23456
    );
}

#[test]
fn shadowed_unsupported_defaults_preserve_the_effective_first_value() {
    for (supported, shadowed) in [
        ("ForwardAgent no", "ForwardAgent yes"),
        ("Compression no", "Compression yes"),
        ("StrictHostKeyChecking yes", "StrictHostKeyChecking no"),
        ("IdentitiesOnly yes", "IdentitiesOnly invalid"),
        ("IdentityAgent none", "IdentityAgent $UNSUPPORTED"),
    ] {
        let (directory, profile) = fixture(&format!("{supported}\nHost *\n {shadowed}"));
        resolve_with_home(&profile, directory.path()).unwrap();
    }
}

#[test]
fn host_alias_case_is_preserved_without_changing_trust_matching() {
    let (directory, mut profile) = fixture("Port 23456");
    profile.ssh_alias = Some("DEV".into());
    assert_eq!(
        resolve_with_home(&profile, directory.path()).unwrap().port,
        22
    );
    assert!(wildcard_match("EXAMPLE.*", "example.org"));
}

#[test]
fn ssh_lexer_preserves_literal_path_backslashes_and_quoted_escapes() {
    assert_eq!(
        tokenize(r"IdentityFile C:\Users\fixture\key").unwrap(),
        ["IdentityFile", r"C:\Users\fixture\key"]
    );
    assert_eq!(
        tokenize(r#"IdentityFile "a\b\"c""#).unwrap(),
        ["IdentityFile", "a\\b\"c"]
    );
    assert_eq!(
        tokenize("IdentityFile trailing\\").unwrap(),
        ["IdentityFile", "trailing\\"]
    );
}

#[test]
fn identity_agent_environment_syntax_is_rejected_explicitly() {
    for value in ["$SSH_AUTH_SOCK", "$FWM_AGENT", "${FWM_AGENT}"] {
        let (directory, profile) = fixture(&format!("IdentityAgent {value}"));
        assert!(
            resolve_with_home(&profile, directory.path())
                .unwrap_err()
                .to_string()
                .contains("IdentityAgent $ENV")
        );
    }
}

#[cfg(unix)]
#[test]
fn ssh_configuration_and_includes_reject_fifos_without_reading_them() {
    let (directory, mut profile) = fixture("Include fifo");
    let fifo = directory.path().join(".ssh/fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
    profile.ssh_config = Some(fifo);
    assert!(
        resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
}
