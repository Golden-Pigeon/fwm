#[cfg(unix)]
#[path = "auth_agent.rs"]
mod agent;
#[path = "auth_fixture.rs"]
mod fixture;

use crate::ssh::SshError;
use fixture::{Fixture, certificate, ed25519, rsa};
use russh::keys::ssh_key::HashAlg;

fn attention(error: SshError, text: &str) {
    assert!(error.needs_attention(), "{error:?}");
    assert!(error.to_string().contains(text), "{error:?}");
}

#[tokio::test]
async fn private_files_negotiate_rsa_sha2_and_reject_sha1_before_authentication() {
    let key = rsa();
    for hash in [HashAlg::Sha256, HashAlg::Sha512] {
        let mut server = Fixture::new(vec![key.public_key().clone()], None, Some(Some(hash))).await;
        server.identity("rsa", &key);
        server.check().await.unwrap();
        assert_eq!(server.observations.lock().unwrap().authenticated.len(), 1);
    }
    let mut server = Fixture::new(vec![key.public_key().clone()], None, Some(None)).await;
    server.identity("rsa", &key);
    attention(server.check().await.unwrap_err(), "obsolete RSA/SHA-1");
    assert!(server.observations.lock().unwrap().offered.is_empty());
}

#[tokio::test]
async fn unsupported_rsa_identity_does_not_block_a_later_supported_ed25519_key() {
    let allowed = ed25519();
    let mut server = Fixture::new(vec![allowed.public_key().clone()], None, Some(None)).await;
    server.identity("old-rsa", &rsa());
    server.identity("ed25519", &allowed);
    server.check().await.unwrap();
    assert_eq!(server.observations.lock().unwrap().authenticated.len(), 1);
}

#[tokio::test]
async fn repeated_private_file_is_not_offered_twice() {
    let rejected = ed25519();
    let allowed = ed25519();
    let mut server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
    let path = server.identity("rejected", &rejected);
    server.profile.identity_files.push(path);
    server.identity("accepted", &allowed);
    server.check().await.unwrap();
    assert_eq!(server.observations.lock().unwrap().authenticated.len(), 2);
}

#[tokio::test]
async fn user_certificate_files_authenticate_and_rejected_certificate_can_fall_back_to_plain_key() {
    let key = ed25519();
    let ca = ed25519();
    let cert = certificate(&key, &ca);
    for accept_cert in [true, false] {
        let mut server = Fixture::new(
            if accept_cert {
                vec![]
            } else {
                vec![key.public_key().clone()]
            },
            accept_cert.then(|| ca.public_key().clone()),
            None,
        )
        .await;
        let path = server.identity("cert-key", &key);
        std::fs::write(
            super::suffix(&path, "-cert.pub"),
            cert.to_openssh().unwrap(),
        )
        .unwrap();
        server.check().await.unwrap();
        let observed = server.observations.lock().unwrap();
        assert_eq!(observed.certificates, 1);
        assert_eq!(
            observed.authenticated.len(),
            if accept_cert { 0 } else { 1 }
        );
    }
}

#[tokio::test]
async fn rsa_certificate_must_not_bypass_sha1_only_server_rejection() {
    let key = rsa();
    let ca = ed25519();
    let cert = certificate(&key, &ca);
    let mut server = Fixture::new(vec![], Some(ca.public_key().clone()), Some(None)).await;
    let path = server.identity("rsa-cert", &key);
    std::fs::write(
        super::suffix(&path, "-cert.pub"),
        cert.to_openssh().unwrap(),
    )
    .unwrap();
    attention(server.check().await.unwrap_err(), "obsolete RSA/SHA-1");
    assert_eq!(server.observations.lock().unwrap().certificates, 0);
}

#[tokio::test]
async fn local_certificate_signatures_verify_with_exact_negotiated_rsa_hash() {
    use russh::{
        Signer as _,
        keys::{
            signature::Verifier,
            ssh_key::{Algorithm, Signature},
        },
    };
    let key = rsa();
    let cert = certificate(&key, &ed25519());
    let identity = cert.clone().into();
    let challenge = b"real SSH certificate authentication challenge";
    for hash in [HashAlg::Sha256, HashAlg::Sha512] {
        let mut signer = super::signer::LocalSigner(key.clone());
        let signed = signer
            .auth_sign(&identity, Some(hash), challenge.to_vec())
            .await
            .unwrap();
        assert_eq!(&signed[..challenge.len()], challenge);
        let encoded = &signed[challenge.len()..];
        let length = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
        assert_eq!(length, encoded.len() - 4);
        let algorithm_length = u32::from_be_bytes(encoded[4..8].try_into().unwrap()) as usize;
        let algorithm = &encoded[8..8 + algorithm_length];
        assert_eq!(
            algorithm,
            Algorithm::Rsa { hash: Some(hash) }.as_str().as_bytes()
        );
        let raw = &encoded[12 + algorithm_length..];
        let signature = Signature::new(Algorithm::Rsa { hash: Some(hash) }, raw.to_vec()).unwrap();
        Verifier::verify(key.public_key(), challenge, &signature).unwrap();
        let mut server = Fixture::new(
            vec![],
            Some(cert.signature_key().clone().into()),
            Some(Some(hash)),
        )
        .await;
        let path = server.identity("rsa-cert", &key);
        std::fs::write(
            super::suffix(&path, "-cert.pub"),
            cert.to_openssh().unwrap(),
        )
        .unwrap();
        server.check().await.unwrap();
        assert_eq!(server.observations.lock().unwrap().certificates, 1);
    }
    let mut signer = super::signer::LocalSigner(key);
    assert!(
        signer
            .auth_sign(&identity, None, challenge.to_vec())
            .await
            .is_err()
    );
    let other_identity = ed25519().public_key().clone().into();
    assert!(
        signer
            .auth_sign(&other_identity, Some(HashAlg::Sha512), challenge.to_vec())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn encrypted_and_malformed_private_files_report_attention_without_interaction() {
    let key = ed25519();
    let mut server = Fixture::new(vec![key.public_key().clone()], None, None).await;
    let encrypted = key
        .encrypt(&mut russh::keys::key::safe_rng(), "fixture-secret")
        .unwrap();
    server.identity("encrypted", &encrypted);
    let error = server.check().await.unwrap_err();
    assert!(!error.to_string().contains("fixture-secret"));
    attention(error, "if this private key is encrypted");
    std::fs::write(&server.profile.identity_files[0], "not a private key").unwrap();
    attention(
        server.check().await.unwrap_err(),
        "if this private key is encrypted",
    );
}

#[tokio::test]
async fn malformed_mismatched_and_untrusted_file_certificates_do_not_authenticate() {
    let key = ed25519();
    let ca = ed25519();
    for mode in ["malformed", "mismatched", "untrusted"] {
        let mut server = Fixture::new(vec![], Some(ca.public_key().clone()), None).await;
        let path = server.identity("certificate", &key);
        let encoded = match mode {
            "malformed" => "not an OpenSSH certificate".into(),
            "mismatched" => certificate(&ed25519(), &ca).to_openssh().unwrap(),
            _ => certificate(&key, &ed25519()).to_openssh().unwrap(),
        };
        std::fs::write(super::suffix(&path, "-cert.pub"), encoded).unwrap();
        let error = server.check().await.unwrap_err();
        assert!(error.needs_attention(), "{mode}: {error:?}");
        if mode == "mismatched" {
            assert!(error.to_string().contains("certificate signing failed"));
        }
        assert_eq!(
            server.observations.lock().unwrap().certificates,
            usize::from(mode == "untrusted")
        );
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use agent::{Agent, Behavior, Identity};

    #[tokio::test]
    async fn public_identity_file_selects_agent_key_with_identities_only_true() {
        let allowed = ed25519();
        let mut server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let public = server.directory.path().join("identity.pub");
        allowed.public_key().write_openssh_file(&public).unwrap();
        server.profile.identity_files = vec![public];
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity::key(ed25519()), Identity::key(allowed)],
            false,
        )
        .await;
        server.configure(Some(&agent.path), true);
        let path = server.profile.ssh_config.as_ref().unwrap();
        std::fs::write(
            path,
            std::fs::read_to_string(path)
                .unwrap()
                .replace("IdentitiesOnly yes", "IdentitiesOnly true"),
        )
        .unwrap();
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(1, 0)]);
    }

    #[tokio::test]
    async fn missing_explicit_identity_and_agent_socket_are_identified() {
        let mut server = Fixture::new(vec![], None, None).await;
        let missing = server.directory.path().join("missing-private-key");
        server.profile.identity_files = vec![missing.clone()];
        let socket = server.directory.path().join("missing-agent.sock");
        server.configure(Some(&socket), false);
        let error = server.check().await.unwrap_err().to_string();
        assert!(error.contains(missing.to_str().unwrap()));
        assert!(error.contains(socket.to_str().unwrap()));
        assert!(error.contains("IdentityAgent"));
        assert!(error.contains("daemon restart"));
    }

    #[tokio::test]
    async fn agent_signs_successfully_and_identities_only_filters_unlisted_keys() {
        let allowed = ed25519();
        let other = ed25519();
        let mut server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity::key(other), Identity::key(allowed.clone())],
            false,
        )
        .await;
        let public_only = server.directory.path().join("identity-missing-private");
        allowed
            .public_key()
            .write_openssh_file(super::super::suffix(&public_only, ".pub"))
            .unwrap();
        server.profile.identity_files = vec![public_only];
        server.configure(Some(&agent.path), true);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(1, 0)]);
        assert_eq!(server.observations.lock().unwrap().offered.len(), 1);
    }

    #[tokio::test]
    async fn identities_only_without_matching_sidecar_sends_no_agent_key() {
        let allowed = ed25519();
        let server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let agent = Agent::new(server.directory.path(), vec![Identity::key(allowed)], false).await;
        server.configure(Some(&agent.path), true);
        attention(
            server.check().await.unwrap_err(),
            "rejected all available identities",
        );
        assert!(agent.signed.lock().unwrap().is_empty());
        assert!(server.observations.lock().unwrap().offered.is_empty());
    }

    #[tokio::test]
    async fn encrypted_private_key_is_selected_from_unlocked_agent_with_identities_only() {
        let allowed = ed25519();
        let mut server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let encrypted = allowed
            .encrypt(&mut russh::keys::key::safe_rng(), "fixture-secret")
            .unwrap();
        server.identity("encrypted", &encrypted);
        let agent = Agent::new(server.directory.path(), vec![Identity::key(allowed)], false).await;
        server.configure(Some(&agent.path), true);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0)]);
    }

    #[tokio::test]
    async fn rejected_file_identity_is_not_signed_again_by_agent() {
        let rejected = ed25519();
        let allowed = ed25519();
        let mut server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        server.identity("rejected", &rejected);
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity::key(rejected), Identity::key(allowed)],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(1, 0)]);
        assert_eq!(server.observations.lock().unwrap().authenticated.len(), 2);
    }

    #[tokio::test]
    async fn duplicate_agent_identities_are_not_offered_or_signed_repeatedly() {
        let rejected = ed25519();
        let allowed = ed25519();
        let server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![
                Identity::key(rejected.clone()),
                Identity::key(rejected),
                Identity::key(allowed),
            ],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0), (2, 0)]);
        assert_eq!(server.observations.lock().unwrap().offered.len(), 2);
    }

    #[tokio::test]
    async fn unavailable_or_rejected_agent_list_reports_attention() {
        let server = Fixture::new(vec![], None, None).await;
        server.configure(Some(&server.directory.path().join("missing.sock")), false);
        attention(server.check().await.unwrap_err(), "SSH agent unavailable");
        let agent = Agent::new(server.directory.path(), vec![], true).await;
        server.configure(Some(&agent.path), false);
        attention(
            server.check().await.unwrap_err(),
            "SSH agent returned no identities",
        );
    }

    #[tokio::test]
    async fn agent_signing_failure_malformed_response_and_disconnect_report_attention() {
        for behavior in [Behavior::Refuse, Behavior::Malformed, Behavior::Disconnect] {
            let key = ed25519();
            let server = Fixture::new(vec![key.public_key().clone()], None, None).await;
            let agent = Agent::new(
                server.directory.path(),
                vec![Identity {
                    behavior,
                    ..Identity::key(key)
                }],
                false,
            )
            .await;
            server.configure(Some(&agent.path), false);
            attention(server.check().await.unwrap_err(), "agent signing failed");
            assert_eq!(agent.signed.lock().unwrap().len(), 1);
            assert!(server.observations.lock().unwrap().authenticated.is_empty());
        }
    }

    #[tokio::test]
    async fn corrupt_agent_signature_is_rejected_by_server() {
        let key = ed25519();
        let server = Fixture::new(vec![key.public_key().clone()], None, None).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity {
                behavior: Behavior::Corrupt,
                ..Identity::key(key)
            }],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        attention(
            server.check().await.unwrap_err(),
            "rejected all available identities",
        );
        assert!(server.observations.lock().unwrap().authenticated.is_empty());
    }

    #[tokio::test]
    async fn refused_agent_signature_fails_promptly_without_reusing_incomplete_session() {
        let rejected = ed25519();
        let allowed = ed25519();
        let server = Fixture::new(vec![allowed.public_key().clone()], None, None).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![
                Identity {
                    behavior: Behavior::Refuse,
                    ..Identity::key(rejected)
                },
                Identity::key(allowed),
            ],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        let started = std::time::Instant::now();
        attention(server.check().await.unwrap_err(), "agent signing failed");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0)]);
    }

    #[tokio::test]
    async fn fresh_connection_recovers_after_agent_key_is_authorized() {
        let key = ed25519();
        let server = Fixture::new(vec![key.public_key().clone()], None, None).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity {
                behavior: Behavior::RefuseOnce,
                ..Identity::key(key)
            }],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        attention(server.check().await.unwrap_err(), "agent signing failed");
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0), (0, 0)]);
        assert_eq!(server.observations.lock().unwrap().authenticated.len(), 1);
    }

    #[tokio::test]
    async fn agent_disconnect_during_identity_listing_is_explicit_attention() {
        use tokio::{io::AsyncReadExt, net::UnixListener};
        let server = Fixture::new(vec![], None, None).await;
        let path = server.directory.path().join("broken-agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.read_u32().await;
        });
        server.configure(Some(&path), false);
        attention(
            server.check().await.unwrap_err(),
            "cannot list agent identities",
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn rsa_agent_signatures_use_negotiated_sha256_and_sha512_and_never_sha1() {
        let key = rsa();
        for hash in [Some(HashAlg::Sha256), Some(HashAlg::Sha512), None] {
            let server = Fixture::new(vec![key.public_key().clone()], None, Some(hash)).await;
            let agent = Agent::new(
                server.directory.path(),
                vec![Identity::key(key.clone())],
                false,
            )
            .await;
            server.configure(Some(&agent.path), false);
            if let Some(hash) = hash {
                server.check().await.unwrap();
                assert_eq!(
                    *agent.signed.lock().unwrap(),
                    [(0, if hash == HashAlg::Sha256 { 2 } else { 4 })]
                );
            } else {
                attention(server.check().await.unwrap_err(), "obsolete RSA/SHA-1");
                assert!(agent.signed.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn unsupported_rsa_agent_identity_does_not_hide_a_usable_later_key() {
        let allowed = ed25519();
        let server = Fixture::new(vec![allowed.public_key().clone()], None, Some(None)).await;
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity::key(rsa()), Identity::key(allowed)],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(1, 0)]);
    }

    #[tokio::test]
    async fn encrypted_user_certificate_uses_matching_agent_identity() {
        let key = ed25519();
        let ca = ed25519();
        let cert = certificate(&key, &ca);
        let mut server = Fixture::new(vec![], Some(ca.public_key().clone()), None).await;
        let encrypted = key
            .encrypt(&mut russh::keys::key::safe_rng(), "fixture-secret")
            .unwrap();
        server.identity("encrypted", &encrypted);
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity {
                key,
                certificate: Some(cert),
                behavior: Behavior::Sign,
            }],
            false,
        )
        .await;
        server.configure(Some(&agent.path), true);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0)]);
        assert_eq!(server.observations.lock().unwrap().certificates, 1);
    }

    #[tokio::test]
    async fn ecdsa_file_and_agent_signatures_both_authenticate() {
        use russh::keys::ssh_key::{Algorithm, EcdsaCurve, PrivateKey};
        let key = std::sync::Arc::new(
            PrivateKey::random(
                &mut russh::keys::key::safe_rng(),
                Algorithm::Ecdsa {
                    curve: EcdsaCurve::NistP256,
                },
            )
            .unwrap(),
        );
        let mut server = Fixture::new(vec![key.public_key().clone()], None, None).await;
        server.identity("ecdsa", &key);
        server.check().await.unwrap();
        server.profile.identity_files.clear();
        let agent = Agent::new(server.directory.path(), vec![Identity::key(key)], false).await;
        server.configure(Some(&agent.path), false);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 0)]);
        assert_eq!(server.observations.lock().unwrap().authenticated.len(), 2);
    }

    #[tokio::test]
    async fn agent_user_certificate_is_accepted_even_when_plain_key_was_rejected() {
        let key = ed25519();
        let ca = ed25519();
        let cert = certificate(&key, &ca);
        let mut server = Fixture::new(vec![], Some(ca.public_key().clone()), None).await;
        server.identity("plain", &key);
        let agent = Agent::new(
            server.directory.path(),
            vec![
                Identity::key(key.clone()),
                Identity {
                    key,
                    certificate: Some(cert),
                    behavior: Behavior::Sign,
                },
            ],
            false,
        )
        .await;
        server.configure(Some(&agent.path), true);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(1, 0)]);
        assert_eq!(server.observations.lock().unwrap().certificates, 1);
    }

    #[tokio::test]
    async fn rsa_agent_certificate_uses_sha256_server_choice() {
        let key = rsa();
        let ca = ed25519();
        let cert = certificate(&key, &ca);
        let server = Fixture::new(
            vec![],
            Some(ca.public_key().clone()),
            Some(Some(HashAlg::Sha256)),
        )
        .await;
        let agent = Agent::new(
            server.directory.path(),
            vec![Identity {
                key,
                certificate: Some(cert),
                behavior: Behavior::Sign,
            }],
            false,
        )
        .await;
        server.configure(Some(&agent.path), false);
        server.check().await.unwrap();
        assert_eq!(*agent.signed.lock().unwrap(), [(0, 2)]);
        assert_eq!(server.observations.lock().unwrap().certificates, 1);
    }
}
