use super::*;

fn fixture(configuration: &str) -> (tempfile::TempDir, ServerProfile) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join(".ssh")).unwrap();
    std::fs::write(directory.path().join(".ssh/config"), configuration).unwrap();
    let mut profile = ServerProfile::new("dev");
    profile.ssh_alias = Some("dev".into());
    (directory, profile)
}

#[test]
fn preferred_authentications_accepts_publickey_alone_or_in_a_method_list() {
    for methods in [
        "publickey",
        "publickey,password",
        "password,publickey",
        "gssapi-with-mic,hostbased,publickey,keyboard-interactive,password",
    ] {
        let (directory, profile) = fixture(&format!(
            "Host dev\n PreferredAuthentications {methods}\n IdentityFile ~/key\n IdentityAgent none\n IdentitiesOnly yes\n"
        ));
        let server = resolve_with_home(&profile, directory.path()).unwrap();
        assert_eq!(server.identity_files, [directory.path().join("key")]);
        assert_eq!(server.identity_agent.as_deref(), Some("none"));
        assert!(server.identities_only);
    }
}

#[test]
fn preferred_authentications_does_not_enable_an_excluded_method() {
    for methods in [
        "password",
        "keyboard-interactive,password",
        "none",
        "not-publickey",
    ] {
        let (directory, profile) = fixture(&format!(
            "Host dev\n PreferredAuthentications {methods}\nHost *\n PreferredAuthentications publickey\n"
        ));
        let error = resolve_with_home(&profile, directory.path()).unwrap_err();
        assert!(matches!(error, SshError::Configuration(_)));
        let message = error.to_string();
        assert!(message.contains("config:2:"), "{message}");
        assert!(
            message.contains("PreferredAuthentications excludes publickey"),
            "{message}"
        );
    }
}

#[test]
fn preferred_authentications_respects_host_include_and_first_value_precedence() {
    let (directory, profile) = fixture(
        "Host unrelated\n PreferredAuthentications password\nHost dev\n Include auth.conf\nHost *\n PreferredAuthentications password\n",
    );
    std::fs::write(
        directory.path().join(".ssh/auth.conf"),
        "pReFeRrEdAuThEnTiCaTiOnS=publickey\n",
    )
    .unwrap();
    assert!(resolve_with_home(&profile, directory.path()).is_ok());
}

#[test]
fn preferred_authentications_requires_one_comma_separated_argument() {
    for setting in [
        "PreferredAuthentications",
        "PreferredAuthentications publickey password",
    ] {
        let (directory, profile) = fixture(&format!("Host dev\n {setting}\n"));
        assert!(resolve_with_home(&profile, directory.path()).is_err());
    }
}
