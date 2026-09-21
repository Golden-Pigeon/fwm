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
}

impl TryFrom<Forward> for ForwardSpec {
    type Error = String;
    fn try_from(wire: Forward) -> Result<Self, Self::Error> {
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
        };
        let remote_cleanup = wire.remote_cleanup.unwrap_or(if tunnel.is_remote() {
            RemoteCleanup::Verified
        } else {
            RemoteCleanup::Off
        });
        if remote_cleanup == RemoteCleanup::Verified && !tunnel.is_remote() {
            return Err("verified remote cleanup is only valid for remote forwards".into());
        }
        let connection_mode = if remote_cleanup == RemoteCleanup::Verified {
            ConnectionMode::Dedicated
        } else {
            wire.connection_mode.unwrap_or_default()
        };
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
