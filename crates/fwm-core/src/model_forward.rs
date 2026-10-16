//! Strict flat wire format: serde flatten cannot reject misspelled fields, and
//! forwarding defaults depend on the direction rather than each field alone.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Forward {
    #[serde(default = "new_id")]
    id: String,
    name: String,
    #[serde(default)]
    group: Option<String>,
    server_id: String,
    kind: Kind,
    listen: SocketAddr,
    #[serde(default)]
    target: Option<Endpoint>,
    #[serde(default)]
    target_server_id: Option<String>,
    #[serde(default)]
    desired_state: DesiredState,
    #[serde(default)]
    connection_mode: Option<ConnectionMode>,
    #[serde(default)]
    remote_cleanup: Option<RemoteCleanup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Local,
    Remote,
    Dynamic,
    RemoteDynamic,
    Relay,
}

impl TryFrom<Forward> for ForwardSpec {
    type Error = String;
    fn try_from(wire: Forward) -> Result<Self, Self::Error> {
        if wire.kind.is_relay() != wire.target_server_id.is_some() {
            return Err(if wire.target_server_id.is_some() {
                "target_server_id is only valid for relay forwards".into()
            } else {
                "relay forward requires target_server_id".into()
            });
        }
        let tunnel = match wire.kind {
            Kind::Local => Tunnel::Local {
                listen: wire.listen,
                target: wire.target.ok_or("local forward requires target")?,
            },
            Kind::Remote => Tunnel::Remote {
                listen: wire.listen,
                target: wire.target.ok_or("remote forward requires target")?,
            },
            Kind::Dynamic => {
                if wire.target.is_some() {
                    return Err("dynamic forward does not accept target".into());
                }
                Tunnel::Dynamic {
                    listen: wire.listen,
                }
            }
            Kind::RemoteDynamic => {
                if wire.target.is_some() {
                    return Err("remote dynamic forward does not accept target".into());
                }
                Tunnel::RemoteDynamic {
                    listen: wire.listen,
                }
            }
            Kind::Relay => Tunnel::Relay {
                listen: wire.listen,
                target: wire.target.ok_or("relay forward requires target")?,
                target_server_id: wire.target_server_id.unwrap_or_default(),
            },
        };
        let remote_cleanup = wire.remote_cleanup.unwrap_or(if tunnel.is_remote() {
            RemoteCleanup::Verified
        } else {
            RemoteCleanup::Off
        });
        if remote_cleanup == RemoteCleanup::Verified && !tunnel.is_remote() {
            return Err("verified remote cleanup is only valid for remote forwards".into());
        }
        // A relay always isolates both endpoints on rule-dedicated sessions;
        // an explicit shared request is rejected instead of silently ignored.
        let dedicated =
            remote_cleanup == RemoteCleanup::Verified || matches!(tunnel, Tunnel::Relay { .. });
        let connection_mode = if dedicated {
            ConnectionMode::Dedicated
        } else {
            wire.connection_mode.unwrap_or_default()
        };
        if matches!(tunnel, Tunnel::Relay { .. })
            && wire.connection_mode == Some(ConnectionMode::Shared)
        {
            return Err(
                "relay forwards do not support connection_mode shared; both endpoints use rule-dedicated sessions"
                    .into(),
            );
        }
        Ok(Self {
            id: wire.id,
            name: wire.name,
            group: wire.group,
            server_id: wire.server_id,
            tunnel,
            desired_state: wire.desired_state,
            connection_mode,
            remote_cleanup,
        })
    }
}

impl Kind {
    fn is_relay(&self) -> bool {
        matches!(self, Self::Relay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(kind: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "rule",
            "name": "relay-rule",
            "server_id": "source",
            "kind": kind,
            "listen": "127.0.0.1:8080",
            "target": "localhost:9000",
        })
    }

    #[test]
    fn relay_wire_requires_target_and_target_server_and_forces_dedicated() {
        let mut wire = base("relay");
        wire["target_server_id"] = serde_json::json!("target");
        let spec: ForwardSpec = serde_json::from_value(wire).unwrap();
        assert_eq!(spec.tunnel.kind(), "relay");
        assert_eq!(spec.tunnel.target_server_id(), Some("target"));
        assert_eq!(spec.remote_cleanup, RemoteCleanup::Verified);
        assert_eq!(spec.connection_mode, ConnectionMode::Dedicated);
        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            serde_json::json!({
                "id": "rule",
                "name": "relay-rule",
                "server_id": "source",
                "kind": "relay",
                "listen": "127.0.0.1:8080",
                "target": "localhost:9000",
                "target_server_id": "target",
                "desired_state": "running",
                "connection_mode": "dedicated",
                "remote_cleanup": "verified",
            })
        );
    }

    #[test]
    fn relay_wire_rejects_missing_or_foreign_target_server_and_shared() {
        assert!(serde_json::from_value::<ForwardSpec>(base("relay")).is_err());
        let local = serde_json::json!({
            "id": "rule", "name": "l", "server_id": "s", "kind": "local",
            "listen": "127.0.0.1:8080", "target": "localhost:9000",
            "target_server_id": "target",
        });
        assert!(serde_json::from_value::<ForwardSpec>(local).is_err());
        let mut shared = base("relay");
        shared["target_server_id"] = serde_json::json!("target");
        shared["remote_cleanup"] = serde_json::json!("off");
        shared["connection_mode"] = serde_json::json!("shared");
        let error = serde_json::from_value::<ForwardSpec>(shared)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("dedicated"), "{error}");
    }

    #[test]
    fn relay_with_cleanup_off_stays_dedicated() {
        let mut wire = base("relay");
        wire["target_server_id"] = serde_json::json!("target");
        wire["remote_cleanup"] = serde_json::json!("off");
        let spec: ForwardSpec = serde_json::from_value(wire).unwrap();
        assert_eq!(spec.remote_cleanup, RemoteCleanup::Off);
        assert_eq!(spec.connection_mode, ConnectionMode::Dedicated);
    }
}
