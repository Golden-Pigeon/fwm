use super::*;

#[test]
fn numeric_ipv6_notation_matches_openssh_host_patterns() {
    for (input, expected) in [
        ("::192.0.2.1", "::192.0.2.1"),
        ("::c000:201", "::192.0.2.1"),
        ("::1:0", "::0.1.0.0"),
        ("::ffff:192.0.2.1", "::ffff:192.0.2.1"),
        ("::2", "::2"),
        ("::1", "::1"),
        ("::", "::"),
    ] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname yes\nHost dev\n HostName {input}\nHost {expected}\n User address-user\n Port 23456\n"
        ));
        let resolved = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(resolved.host, expected);
        assert_eq!(resolved.user, "address-user");
        assert_eq!(resolved.port, 23456);
    }
}

fn fixture(configuration: &str) -> (tempfile::TempDir, ServerProfile) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".ssh")).unwrap();
    std::fs::write(directory.path().join(".ssh/config"), configuration).unwrap();
    let mut profile = ServerProfile::new("dev");
    profile.ssh_alias = Some("dev".into());
    (directory, profile)
}

#[test]
fn canonicalize_no_accepts_config_without_reprocessing_hostname_blocks() {
    for setting in ["no", "false"] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname {setting}\nHost dev\n HostName target.example\n User original\nHost target.example\n Port 23456\n IdentityFile ~/second-pass-key\n"
        ));
        let resolved = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(resolved.host, "target.example");
        assert_eq!(resolved.user, "original");
        assert_eq!(resolved.port, 22);
        assert!(!resolved.explicit_identity_files);
    }
}

#[test]
fn canonicalize_enabled_reprocesses_hostname_and_retains_first_values() {
    for setting in ["yes", "true", "always"] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname {setting}\nHost dev\n HostName 192.0.2.10\n User first\n IdentityFile ~/original-key\nHost 192.0.2.*\n HostName changed.example\n User second\n Port 23456\n IdentityFile ~/target-key\n IdentitiesOnly yes\n"
        ));
        let resolved = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(resolved.alias, "dev");
        assert_eq!(resolved.host, "192.0.2.10");
        assert_eq!(resolved.user, "first");
        assert_eq!(resolved.port, 23456);
        assert!(resolved.identities_only);
        assert_eq!(
            resolved.identity_files,
            ["original-key", "target-key"].map(|key| directory.path().join(key))
        );
    }
}

#[test]
fn canonicalize_lowercases_dns_name_without_requiring_dns_or_suffixes() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n HostName Target.Example.Invalid\nHost target.example.invalid\n User matched-target\n Port 23456\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "target.example.invalid");
    assert_eq!(resolved.user, "matched-target");
    assert_eq!(resolved.port, 23456);
}

#[test]
fn canonicalize_normalizes_ipv6_before_matching_second_pass() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n HostName 2001:0DB8:0:0:0:0:0:1\nHost 2001:db8::1\n User ipv6-user\n Port 23456\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "2001:db8::1");
    assert_eq!(resolved.user, "ipv6-user");
    assert_eq!(resolved.port, 23456);
}

#[test]
fn canonicalize_does_not_apply_a_new_hostname_obtained_in_second_pass() {
    let (directory, mut profile) =
        fixture("CanonicalizeHostname yes\nHost dev\n HostName wrong.example\n User final-user\n");
    profile.ssh_alias = Some("DEV".into());
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.alias, "DEV");
    assert_eq!(resolved.host, "dev");
    assert_eq!(resolved.user, "final-user");
}

#[test]
fn canonicalize_preserves_original_alias_and_final_hostname_tokens() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n HostName Target.Example.Invalid\n IdentityFile ~/keys/%n-%h-%r-%p\nHost target.example.invalid\n User target-user\n Port 23456\n UserKnownHostsFile ~/trust/%n-%h\n IdentityAgent ~/agents/%n-%h\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(
        resolved.identity_files,
        [directory
            .path()
            .join("keys/dev-target.example.invalid-target-user-23456")]
    );
    assert_eq!(
        resolved.known_hosts,
        directory.path().join("trust/dev-target.example.invalid")
    );
    assert_eq!(
        resolved.identity_agent.as_deref(),
        directory
            .path()
            .join("agents/dev-target.example.invalid")
            .to_str()
    );
}

#[test]
fn canonicalize_preserves_explicit_connection_overrides() {
    let (directory, mut profile) = fixture(
        "CanonicalizeHostname always\nHost dev\n HostName ignored.example\n User inherited\n Port 2222\n IdentityFile ~/inherited-key\n UserKnownHostsFile ~/inherited-hosts\n ProxyJump inherited-jump\nHost explicit.example\n IdentityAgent ~/matched-explicit-host\n",
    );
    profile.host = Some("Explicit.Example".into());
    profile.user = Some("explicit-user".into());
    profile.port = Some(2200);
    profile.identity_files = vec!["~/explicit-key".into()];
    profile.known_hosts = Some("~/explicit-hosts".into());
    profile.proxy_jump = vec!["explicit-jump".into()];
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "explicit.example");
    assert_eq!(resolved.user, "explicit-user");
    assert_eq!(resolved.port, 2200);
    assert_eq!(
        resolved.identity_files,
        [directory.path().join("explicit-key")]
    );
    assert_eq!(
        resolved.known_hosts,
        directory.path().join("explicit-hosts")
    );
    assert_eq!(resolved.proxy_jump, ["explicit-jump"]);
    assert_eq!(
        resolved.identity_agent.as_deref(),
        directory.path().join("matched-explicit-host").to_str()
    );
}

#[test]
fn canonicalize_second_pass_deduplicates_identity_files_for_unchanged_host() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n User fixture\n IdentityFile ~/first\n IdentityFile ~/second\nHost *\n IdentityFile ~/first\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "dev");
    assert_eq!(
        resolved.identity_files,
        ["first", "second"].map(|key| directory.path().join(key))
    );
}

#[test]
fn canonicalize_reprocesses_includes_without_duplicating_identities() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nInclude canonical.conf\nHost dev\n HostName 192.0.2.10\n",
    );
    std::fs::write(
        directory.path().join(".ssh/canonical.conf"),
        "IdentityFile ~/shared\nHost 192.0.2.10\n Port 23456\n IdentityFile ~/matched-ip\n",
    )
    .unwrap();
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "192.0.2.10");
    assert_eq!(resolved.port, 23456);
    assert_eq!(
        resolved.identity_files,
        ["shared", "matched-ip"].map(|key| directory.path().join(key))
    );
}

#[test]
fn canonicalize_first_value_wins_across_host_defaults() {
    for (first, later, expected_port) in [
        ("no", "yes", 22),
        ("yes", "no", 23456),
        ("yes", "invalid", 23456),
    ] {
        let (directory, profile) = fixture(&format!(
            "Host dev\n CanonicalizeHostname {first}\n HostName 192.0.2.10\nHost *\n CanonicalizeHostname {later}\nHost 192.0.2.10\n Port 23456\n"
        ));
        assert_eq!(
            resolve_with_home(&profile, directory.path()).unwrap().port,
            expected_port
        );
    }
}

#[test]
fn canonicalize_invalid_and_multiple_values_are_rejected() {
    for value in ["potato", "1", "yes no", "always yes"] {
        let (directory, profile) = fixture(&format!("CanonicalizeHostname {value}\n"));
        let error = resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("CanonicalizeHostname"), "{error}");
        assert!(!error.contains("unsupported SSH directive"), "{error}");
    }
}

#[test]
fn canonicalize_inactive_settings_do_not_affect_the_selected_host() {
    let (directory, profile) = fixture(
        "Host other\n CanonicalizeHostname invalid\n CanonicalDomains unsupported.example\nHost dev\n HostName target.example\n User fixture\nHost target.example\n Port 23456\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "target.example");
    assert_eq!(resolved.port, 22);
}

#[test]
fn canonicalize_does_not_silently_ignore_unimplemented_dns_rules() {
    for setting in [
        "CanonicalDomains example.com",
        "CanonicalizePermittedCNAMEs *.example.com:*.example.net",
    ] {
        let (directory, profile) = fixture(&format!("CanonicalizeHostname yes\n{setting}\n"));
        let error = resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported SSH directive"), "{error}");
        assert!(error.contains(setting.split_whitespace().next().unwrap()));
    }
}

#[test]
fn canonicalize_preserves_proxy_jump_and_applies_second_pass_options() {
    for setting in ["yes", "always"] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname {setting}\nHost dev\n HostName 192.0.2.10\n ProxyJump jump-a,jump-b\nHost 192.0.2.10\n User target-user\n Port 23456\n ProxyJump changed-jump\n"
        ));
        let resolved = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(resolved.proxy_jump, ["jump-a", "jump-b"]);
        assert_eq!(resolved.user, "target-user");
        assert_eq!(resolved.port, 23456);
    }
}

#[test]
fn canonicalize_direct_trailing_dot_names_require_unsupported_dns_processing() {
    for mode in ["yes", "always"] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname {mode}\nHost dev\n HostName target.example.invalid.\n"
        ));
        let error = resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("CanonicalizeHostname"), "{error}");
        assert!(error.contains("trailing-dot"), "{error}");
        assert!(error.contains("DNS canonicalization"), "{error}");
    }
}

#[test]
fn canonicalize_yes_preserves_proxied_trailing_dot_name_and_reprocesses_it() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n HostName Target.Example.Invalid.\n ProxyJump jump\nHost target.example.invalid.\n User target-user\n Port 23456\n",
    );
    let resolved = resolve_with_home(&profile, directory.path()).unwrap();
    assert_eq!(resolved.host, "target.example.invalid.");
    assert_eq!(resolved.proxy_jump, ["jump"]);
    assert_eq!(resolved.user, "target-user");
    assert_eq!(resolved.port, 23456);
}

#[test]
fn canonicalize_always_rejects_trailing_dot_name_even_when_proxied() {
    let (directory, profile) = fixture(
        "CanonicalizeHostname always\nHost dev\n HostName target.example.invalid.\n ProxyJump jump\n",
    );
    let error = resolve_with_home(&profile, directory.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("trailing-dot"), "{error}");
    assert!(error.contains("DNS canonicalization"), "{error}");
}

#[test]
fn canonicalize_trailing_dot_checks_effective_proxy_jump_override() {
    let (directory, mut profile) = fixture(
        "CanonicalizeHostname yes\nHost dev\n HostName target.example.invalid.\n ProxyJump inherited-jump\n",
    );
    profile.proxy_jump = vec!["none".into()];
    let error = resolve_with_home(&profile, directory.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("trailing-dot"), "{error}");
    assert!(error.contains("DNS canonicalization"), "{error}");
}

#[test]
fn canonicalize_rejects_nonstandard_numeric_and_scoped_addresses() {
    for host in [
        "127.1",
        "2130706433",
        "0177.0.0.1",
        "0x7f000001",
        "0x7f.0.0.1",
        "192.0.2.999",
        "fe80::1%%en0",
        "fe80::1%%3",
    ] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname yes\nHost dev\n HostName {host}\n"
        ));
        let error = resolve_with_home(&profile, directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("CanonicalizeHostname"), "{host}: {error}");
        assert!(
            error.contains("standard IPv4/IPv6 literal"),
            "{host}: {error}"
        );
    }
}

#[test]
fn canonicalize_accepts_dns_labels_that_only_resemble_hexadecimal_addresses() {
    for host in ["0xhost.example", "0xdeadbeef.example", "srv.0xcompany.test"] {
        let (directory, profile) = fixture(&format!(
            "CanonicalizeHostname yes\nHost dev\n HostName {host}\nHost {host}\n Port 23456\n"
        ));
        let resolved = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(resolved.host, host);
        assert_eq!(resolved.port, 23456);
    }
}
