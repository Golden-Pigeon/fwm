use super::{
    args::{Mode, PortOptions},
    ports,
};
use anyhow::{Result, anyhow, bail};
use fwm_core::model::{ConnectionMode, Endpoint, Tunnel};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

impl From<Mode> for ConnectionMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Shared => Self::Shared,
            Mode::Dedicated => Self::Dedicated,
        }
    }
}

pub fn tunnels(
    local: Option<&str>,
    remote: Option<&str>,
    dynamic: Option<&str>,
    remote_dynamic: Option<&str>,
    ports: &impl PortOptions,
    existing: Option<&Tunnel>,
) -> Result<Vec<Tunnel>> {
    if [local, remote, dynamic, remote_dynamic]
        .into_iter()
        .flatten()
        .count()
        > 1
    {
        bail!("choose only one of --local, --remote, --dynamic and --remote-dynamic");
    }
    let ports = ports.values();
    let has_ports = ports.port.is_some() || ports.src.is_some() || ports.tgt.is_some();
    if ports.port.is_some() && (ports.src.is_some() || ports.tgt.is_some()) {
        bail!("--port cannot be combined with --src or --tgt");
    }
    if existing.is_none() && ports.src.is_some() != ports.tgt.is_some() {
        bail!("--src and --tgt must be supplied together");
    }
    if let Some(value) = dynamic.or(remote_dynamic) {
        if has_ports {
            bail!("--port, --src and --tgt only support local or remote forwarding");
        }
        let listen = listening(value)?;
        return Ok(vec![if remote_dynamic.is_some() {
            Tunnel::RemoteDynamic { listen }
        } else {
            Tunnel::Dynamic { listen }
        }]);
    }
    let (value, is_remote) = match (local, remote) {
        (Some(value), None) => (value, false),
        (None, Some(value)) => (value, true),
        (Some(_), Some(_)) => bail!("choose only one of --local and --remote"),
        (None, None) if !has_ports => return Ok(vec![]),
        (None, None) => match existing {
            Some(Tunnel::Local { .. }) => ("", false),
            Some(Tunnel::Remote { .. }) => ("", true),
            _ => bail!("choose --local or --remote when using --port or --src/--tgt"),
        },
    };
    if !value.is_empty() && has_ports {
        bail!(
            "a --local/--remote value cannot be combined with --port or --src/--tgt; omit the direction's value"
        );
    }
    if value.contains(':') {
        if is_remote && split_brackets(value)?.len() == 2 {
            bail!(
                "--remote requires a fixed target; use --remote-dynamic {value} for a reverse SOCKS proxy"
            );
        }
        let (mut listen, target) = forwarding(value)?;
        if let Some(previous) = existing
            && split_brackets(value)?.len() == 3
        {
            let port = listen.port();
            listen = previous.listen();
            listen.set_port(port);
        }
        return Ok(vec![directed(is_remote, listen, target)]);
    }
    if let Some(previous) = existing
        && let Some(port) = ports
            .port
            .as_deref()
            .or_else(|| (!value.is_empty()).then_some(value))
    {
        let port = ports::single(port)?;
        let mut listen = previous.listen();
        listen.set_port(port);
        let target = Endpoint {
            host: previous
                .target()
                .map_or_else(|| "localhost".into(), |target| target.host.clone()),
            port,
        };
        return Ok(vec![directed(is_remote, listen, target)]);
    }
    // Editing is a patch: omitted fields retain the existing bind address,
    // destination hostname and ports, including a bare direction change.
    if let Some(previous) = existing
        && value.is_empty()
        && ports.port.is_none()
    {
        let mut listen = previous.listen();
        if let Some(source) = ports.src.as_deref() {
            let (address, port) = source_parts(source)?;
            listen.set_port(ports::single(port)?);
            if let Some(address) = address {
                listen.set_ip(address);
            }
        }
        let previous_target = previous.target();
        let target = if let Some(value) = ports.tgt.as_deref() {
            // A bare port keeps the recorded host; an explicit host replaces it.
            let (host, port) = target_parts(value)?;
            Endpoint {
                host: host.unwrap_or_else(|| {
                    previous_target.map_or_else(|| "localhost".into(), |t| t.host.clone())
                }),
                port,
            }
        } else {
            Endpoint {
                    host: previous_target.map_or_else(|| "localhost".into(), |t| t.host.clone()),
                    port: previous_target
                        .map(|target| target.port)
                        .ok_or_else(|| {
                            anyhow!("switching a SOCKS proxy to local/remote forwarding requires --tgt [HOST:]PORT")
                        })?,
                }
        };
        return Ok(vec![directed(is_remote, listen, target)]);
    }
    let (sources, target) = if let Some(source) = ports.src.as_deref() {
        let (host, port) = target_parts(ports.tgt.as_deref().expect("validated pair"))?;
        (
            source,
            Some(Endpoint {
                host: host.unwrap_or_else(|| "localhost".into()),
                port,
            }),
        )
    } else {
        (ports.port.as_deref().unwrap_or(value), None)
    };
    if sources.is_empty() {
        bail!(
            "--local/--remote requires a forwarding specification, --port PORTS or --src PORTS --tgt [HOST:]PORT"
        );
    }
    let (address, sources) = if ports.src.is_some() {
        source_parts(sources)?
    } else {
        (None, sources)
    };
    Ok(ports::expand(sources)?
        .into_iter()
        .map(|source| {
            let target = target.clone().unwrap_or_else(|| Endpoint {
                host: "localhost".into(),
                port: source,
            });
            directed(
                is_remote,
                SocketAddr::new(address.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)), source),
                target,
            )
        })
        .collect())
}

/// One relay listener and its resolved target endpoint.
pub(super) struct RelaySpec {
    pub listen: SocketAddr,
    pub target: Endpoint,
}

/// Parse two-server relay endpoints: `--src [IP:]PORTS --tgt [HOST:]PORT`, or
/// `--port` for the same port on both sides. Ports are 1–65535 and never
/// reallocated.
pub fn relays(src: Option<&str>, tgt: Option<&str>, port: Option<&str>) -> Result<Vec<RelaySpec>> {
    if let Some(ports) = port {
        if src.is_some() || tgt.is_some() {
            bail!("--port cannot be combined with --src or --tgt");
        }
        return Ok(ports::expand(ports)?
            .into_iter()
            .map(|target_port| RelaySpec {
                listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), target_port),
                target: Endpoint {
                    host: "localhost".into(),
                    port: target_port,
                },
            })
            .collect());
    }
    let (source, target) = match (src, tgt) {
        (Some(source), Some(target)) => (source, target),
        _ => bail!(
            "a relay forward requires --src [IP:]PORTS and --tgt [HOST:]PORT, or --port PORTS"
        ),
    };
    let (host, target_port) = target_parts(target)?;
    let host = host.unwrap_or_else(|| "localhost".into());
    let (address, sources) = source_parts(source)?;
    Ok(ports::expand(sources)?
        .into_iter()
        .map(|listen_port| RelaySpec {
            listen: SocketAddr::new(
                address.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                listen_port,
            ),
            target: Endpoint {
                host: host.clone(),
                port: target_port,
            },
        })
        .collect())
}

/// `--tgt` mirrors `--src`'s address prefix: `[HOST:]PORT`. A bare port keeps
/// any existing destination host; an explicit host replaces it.
pub(super) fn target_parts(value: &str) -> Result<(Option<String>, u16)> {
    let parts = split_brackets(value.trim())?;
    match parts.as_slice() {
        [port] => Ok((None, ports::single(port)?)),
        [host, port] => Ok((Some(target_host(host)?), ports::single(port)?)),
        _ => bail!("--tgt expects [HOST:]PORT; put IPv6 addresses in brackets"),
    }
}

/// A destination host is resolved by the target side; accept a bare IPv6
/// literal (with or without brackets) exactly like `--tgt [::1]:PORT`.
pub fn target_host(value: &str) -> Result<String> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("--tgt host must not be empty or contain whitespace");
    }
    let host = value
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(value);
    if host.contains(':') && host.parse::<std::net::Ipv6Addr>().is_err() {
        bail!("--tgt host {value:?} is not a valid IPv6 address; bracket IPv6 literals",);
    }
    Ok(host.to_owned())
}

/// An optional IP prefix applies to the entire source port list.
pub(super) fn source_parts(value: &str) -> Result<(Option<IpAddr>, &str)> {
    let parts = split_brackets(value.trim())?;
    match parts.as_slice() {
        [ports] => Ok((None, ports)),
        [address, ports] => {
            let address = listening(&format!("{}:1", address.trim()))?.ip();
            Ok((Some(address), ports))
        }
        _ => bail!("--src expects [IP:]PORTS; put IPv6 addresses in brackets"),
    }
}

fn directed(is_remote: bool, listen: SocketAddr, target: Endpoint) -> Tunnel {
    if is_remote {
        Tunnel::Remote { listen, target }
    } else {
        Tunnel::Local { listen, target }
    }
}

fn listening(value: &str) -> Result<SocketAddr> {
    let address = if !value.contains(':') {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            value.parse().map_err(|_| {
                anyhow!("invalid listen port {value:?}; choose an integer between 1 and 65535")
            })?,
        )
    } else {
        value.parse().map_err(|_| {
            anyhow!("invalid listen address {value:?}; use an IP address and brackets around IPv6")
        })?
    };
    if address.port() == 0 {
        bail!("listen port must be between 1 and 65535");
    }
    Ok(address)
}

fn forwarding(value: &str) -> Result<(SocketAddr, Endpoint)> {
    let parts = split_brackets(value)?;
    let (listen, target) = match parts.as_slice() {
        [port, host, target_port] => (listening(port)?, format!("{host}:{target_port}")),
        [address, port, host, target_port] => (
            listening(&format!("{address}:{port}"))?,
            format!("{host}:{target_port}"),
        ),
        _ => bail!(
            "expected [bind_address:]port:target_host:target_port; put IPv6 addresses in brackets"
        ),
    };
    Ok((
        listen,
        target.parse().map_err(|message: String| anyhow!(message))?,
    ))
}

fn split_brackets(value: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut bracket = false;
    let mut start = 0;
    for (position, character) in value.char_indices() {
        match character {
            '[' if !bracket => bracket = true,
            ']' if bracket => bracket = false,
            '[' | ']' => bail!("unbalanced IPv6 brackets"),
            ':' if !bracket => {
                parts.push(&value[start..position]);
                start = position + 1;
            }
            _ => {}
        }
    }
    if bracket {
        bail!("unbalanced IPv6 brackets");
    }
    parts.push(&value[start..]);
    Ok(parts)
}

#[cfg(test)]
#[path = "parse_patch_tests.rs"]
mod patch_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_same_port_changes_ports_without_resetting_existing_hosts() {
        let existing = Tunnel::Local {
            listen: "[::1]:1234".parse().unwrap(),
            target: "internal.example:5678".parse().unwrap(),
        };
        let ports = super::super::args::EditPortArgs {
            port: Some("8080".into()),
            ..Default::default()
        };
        let changed = tunnels(None, None, None, None, &ports, Some(&existing)).unwrap();
        assert_eq!(changed[0].listen().to_string(), "[::1]:8080");
        assert_eq!(
            changed[0].target().unwrap().to_string(),
            "internal.example:8080"
        );
    }
    use crate::cli::args::{EditPortArgs, PortArgs};
    #[test]
    fn edit_changes_only_requested_ports_or_direction_including_ipv6() {
        let original = Tunnel::Local {
            listen: "[::1]:12345".parse().unwrap(),
            target: Endpoint {
                host: "internal.example".into(),
                port: 8080,
            },
        };
        let target_only = EditPortArgs {
            tgt: Some("8081".into()),
            ..Default::default()
        };
        let changed = tunnels(None, None, None, None, &target_only, Some(&original))
            .unwrap()
            .remove(0);
        assert_eq!(changed.listen(), original.listen());
        assert_eq!(
            changed.target().unwrap().to_string(),
            "internal.example:8081"
        );
        let source_only = EditPortArgs {
            src: Some("12346".into()),
            ..Default::default()
        };
        let changed = tunnels(None, None, None, None, &source_only, Some(&original))
            .unwrap()
            .remove(0);
        assert_eq!(changed.listen().to_string(), "[::1]:12346");
        assert_eq!(changed.target(), original.target());
        let changed = tunnels(
            None,
            Some(""),
            None,
            None,
            &EditPortArgs::default(),
            Some(&original),
        )
        .unwrap()
        .remove(0);
        assert!(changed.is_remote());
        assert_eq!(changed.listen(), original.listen());
        assert_eq!(changed.target(), original.target());
        let invalid = EditPortArgs {
            src: Some("3000-3001".into()),
            ..Default::default()
        };
        assert!(tunnels(None, None, None, None, &invalid, Some(&original)).is_err());
        let invalid = EditPortArgs {
            tgt: Some("0".into()),
            ..Default::default()
        };
        assert!(tunnels(None, None, None, None, &invalid, Some(&original)).is_err());
    }

    #[test]
    fn switching_a_dynamic_proxy_requires_a_destination_but_keeps_its_listen_address() {
        let original = Tunnel::Dynamic {
            listen: "[::1]:1080".parse().unwrap(),
        };
        assert!(
            tunnels(
                Some(""),
                None,
                None,
                None,
                &EditPortArgs::default(),
                Some(&original)
            )
            .is_err()
        );
        let ports = EditPortArgs {
            tgt: Some("80".into()),
            ..Default::default()
        };
        let changed = tunnels(Some(""), None, None, None, &ports, Some(&original))
            .unwrap()
            .remove(0);
        assert_eq!(changed.listen(), original.listen());
        assert_eq!(changed.target().unwrap().to_string(), "localhost:80");
    }

    #[test]
    fn expands_local_and_remote_shorthand_with_localhost_targets() {
        let ports = PortArgs {
            port: Some("3001-3003,3000,3002".into()),
            ..Default::default()
        };
        for (local, remote) in [(Some(""), None), (None, Some(""))] {
            let rules = tunnels(local, remote, None, None, &ports, None).unwrap();
            assert_eq!(
                rules
                    .iter()
                    .map(|rule| rule.listen().port())
                    .collect::<Vec<_>>(),
                [3000, 3001, 3002, 3003]
            );
            for rule in rules {
                assert_eq!(rule.is_remote(), remote.is_some());
                assert_eq!(rule.listen().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
                let target = rule.target().unwrap();
                assert_eq!(target.host, "localhost");
                assert_eq!(target.port, rule.listen().port());
            }
        }
        let direct = tunnels(Some("3000"), None, None, None, &PortArgs::default(), None).unwrap();
        assert_eq!(direct[0].target().unwrap().to_string(), "localhost:3000");
    }

    #[test]
    fn tgt_accepts_optional_host_on_add_and_edit() {
        let ports = PortArgs {
            src: Some("5000-5001".into()),
            tgt: Some("db.internal:5432".into()),
            ..Default::default()
        };
        let rules = tunnels(None, Some(""), None, None, &ports, None).unwrap();
        assert!(rules.iter().all(
            |rule| rule.is_remote() && rule.target().unwrap().to_string() == "db.internal:5432"
        ));
        let existing = Tunnel::Local {
            listen: "[::1]:12345".parse().unwrap(),
            target: Endpoint {
                host: "internal.example".into(),
                port: 8080,
            },
        };
        // A bare port keeps the recorded host; an explicit host replaces it.
        let bare = EditPortArgs {
            tgt: Some("8081".into()),
            ..Default::default()
        };
        let changed = tunnels(None, None, None, None, &bare, Some(&existing))
            .unwrap()
            .remove(0);
        assert_eq!(
            changed.target().unwrap().to_string(),
            "internal.example:8081"
        );
        let explicit = EditPortArgs {
            tgt: Some("[2001:db8::1]:443".into()),
            ..Default::default()
        };
        let changed = tunnels(None, None, None, None, &explicit, Some(&existing))
            .unwrap()
            .remove(0);
        assert_eq!(changed.target().unwrap().to_string(), "[2001:db8::1]:443");
        for invalid in [
            "::1:8080",
            "db.internal:0",
            "db.internal:8080:9090",
            ":8080",
        ] {
            let bad = EditPortArgs {
                tgt: Some(invalid.into()),
                ..Default::default()
            };
            assert!(
                tunnels(None, None, None, None, &bad, Some(&existing)).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn maps_multiple_sources_to_one_target() {
        let ports = PortArgs {
            src: Some("5000-5002,6000".into()),
            tgt: Some("3000".into()),
            ..Default::default()
        };
        let rules = tunnels(None, Some(""), None, None, &ports, None).unwrap();
        assert_eq!(
            rules
                .iter()
                .map(|rule| rule.listen().port())
                .collect::<Vec<_>>(),
            [5000, 5001, 5002, 6000]
        );
        assert!(
            rules
                .iter()
                .all(|rule| rule.is_remote()
                    && rule.target().unwrap().to_string() == "localhost:3000")
        );
    }

    #[test]
    fn rejects_explicit_spec_conflicts_and_bare_direction() {
        let ports = PortArgs {
            port: Some("3000".into()),
            ..Default::default()
        };
        assert!(tunnels(Some("3001:localhost:3001"), None, None, None, &ports, None).is_err());
        assert!(tunnels(None, Some("3001"), None, None, &ports, None).is_err());
        assert!(tunnels(Some(""), None, None, None, &PortArgs::default(), None).is_err());
        let invalid_target = PortArgs {
            src: Some("3000".into()),
            tgt: Some("3001-3002".into()),
            ..Default::default()
        };
        assert!(tunnels(Some(""), None, None, None, &invalid_target, None).is_err());
    }

    #[test]
    fn edit_preserves_direction_and_metadata_only_edits() {
        let original = tunnels(
            None,
            Some("4000:host:4001"),
            None,
            None,
            &PortArgs::default(),
            None,
        )
        .unwrap()
        .remove(0);
        let ports = PortArgs {
            port: Some("3000".into()),
            ..Default::default()
        };
        let edited = tunnels(None, None, None, None, &ports, Some(&original)).unwrap();
        assert!(edited[0].is_remote());
        assert_eq!(edited[0].target().unwrap().to_string(), "host:3000");
        assert!(
            tunnels(
                None,
                None,
                None,
                None,
                &PortArgs::default(),
                Some(&original)
            )
            .unwrap()
            .is_empty()
        );
        let dynamic = Tunnel::Dynamic {
            listen: "127.0.0.1:1080".parse().unwrap(),
        };
        assert!(tunnels(None, None, None, None, &ports, Some(&dynamic)).is_err());
    }
    #[test]
    fn defaults_to_loopback_and_preserves_remote_dns() {
        let (listen, target) = forwarding("3000:internal.example:8080").unwrap();
        assert_eq!(listen.to_string(), "127.0.0.1:3000");
        assert_eq!(target.host, "internal.example");
        assert_eq!(target.port, 8080);
    }
    #[test]
    fn accepts_ipv6_on_both_sides() {
        let (listen, target) = forwarding("[::1]:3000:[2001:db8::1]:8080").unwrap();
        assert_eq!(listen.to_string(), "[::1]:3000");
        assert_eq!(target.to_string(), "[2001:db8::1]:8080");
    }
    #[test]
    fn rejects_ambiguous_addresses_and_zero_ports() {
        for value in [
            "::1:3000:host:80",
            "[::1:3000:host:80",
            "3000:host:0",
            "0:host:80",
            "bad:3000:host:80",
        ] {
            assert!(forwarding(value).is_err(), "{value}");
        }
        assert!(listening("0").is_err());
        assert!(listening("65536").is_err());
    }
}
