use std::{
    collections::HashSet,
    fmt,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    str::FromStr,
};

#[path = "model_forward.rs"]
mod forward_wire;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 3;

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl From<Endpoint> for String {
    fn from(value: Endpoint) -> Self {
        value.to_string()
    }
}

impl TryFrom<String> for Endpoint {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl FromStr for Endpoint {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (host, port) = value.rsplit_once(':').ok_or("expected host:port")?;
        let host = if host.starts_with('[') && host.ends_with(']') {
            let literal = &host[1..host.len() - 1];
            let (address, zone) = literal
                .split_once('%')
                .map_or((literal, None), |(address, zone)| (address, Some(zone)));
            address
                .parse::<Ipv6Addr>()
                .map_err(|_| "invalid IPv6 destination literal")?;
            if zone.is_some_and(|zone| {
                zone.is_empty()
                    || !zone
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            }) {
                return Err("invalid IPv6 destination scope".into());
            }
            literal
        } else {
            if host.contains([':', '[', ']']) {
                return Err("IPv6 addresses require brackets".into());
            }
            host
        };
        if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("target host must not be empty or contain whitespace".into());
        }
        let port: u16 = port.parse().map_err(|_| "invalid port")?;
        if port == 0 {
            return Err("target port must be between 1 and 65535".into());
        }
        Ok(Self {
            host: host.into(),
            port,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Tunnel {
    Local {
        listen: SocketAddr,
        target: Endpoint,
    },
    Remote {
        listen: SocketAddr,
        target: Endpoint,
    },
    Dynamic {
        listen: SocketAddr,
    },
    RemoteDynamic {
        listen: SocketAddr,
    },
}

impl Tunnel {
    pub fn listen(&self) -> SocketAddr {
        match self {
            Self::Local { listen, .. }
            | Self::Remote { listen, .. }
            | Self::Dynamic { listen }
            | Self::RemoteDynamic { listen } => *listen,
        }
    }
    pub fn target(&self) -> Option<&Endpoint> {
        match self {
            Self::Local { target, .. } | Self::Remote { target, .. } => Some(target),
            _ => None,
        }
    }
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::Remote { .. } => "remote",
            Self::Dynamic { .. } => "dynamic",
            Self::RemoteDynamic { .. } => "remote_dynamic",
        }
    }
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. } | Self::RemoteDynamic { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    #[default]
    Running,
    Stopped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    #[default]
    Shared,
    Dedicated,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCleanup {
    #[default]
    Off,
    Verified,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerProfile {
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub ssh_alias: Option<String>,
    #[serde(default)]
    pub ssh_config: Option<PathBuf>,
    #[serde(default)]
    pub identity_files: Vec<PathBuf>,
    #[serde(default)]
    pub known_hosts: Option<PathBuf>,
    /// Empty inherits SSH config; a sole `none` explicitly disables jump hosts.
    #[serde(default)]
    pub proxy_jump: Vec<String>,
}

impl ServerProfile {
    /// Validate persisted overrides before saving or resolving a direct profile.
    pub fn validate_connection_options(&self) -> Result<(), String> {
        if self.port == Some(0) {
            return Err("SSH port must be positive".into());
        }
        if self.host.is_none() && self.ssh_alias.is_none() {
            return Err(format!("server {} needs host or ssh_alias", self.name));
        }
        for (name, value) in [
            ("host", &self.host),
            ("user", &self.user),
            ("ssh_alias", &self.ssh_alias),
        ] {
            if let Some(value) = value
                && (value.is_empty() || value.chars().any(|c| c.is_control() || c.is_whitespace()))
            {
                return Err(format!(
                    "SSH {name} must not be empty or contain whitespace"
                ));
            }
        }
        for (name, path) in self
            .identity_files
            .iter()
            .map(|path| ("identity", path))
            .chain(self.ssh_config.iter().map(|path| ("ssh_config", path)))
            .chain(self.known_hosts.iter().map(|path| ("known_hosts", path)))
        {
            if path.as_os_str().is_empty() || path.to_string_lossy().chars().any(char::is_control) {
                return Err(format!(
                    "SSH {name} path must not be empty or contain control characters"
                ));
            }
        }
        if self.proxy_jump.iter().any(|jump| {
            jump.is_empty()
                || jump
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace() || c == ',')
        }) {
            return Err("SSH proxy_jump entries must be nonempty individual jump hosts".into());
        }
        if self.proxy_jump.len() > 1
            && self
                .proxy_jump
                .iter()
                .any(|jump| jump.eq_ignore_ascii_case("none"))
        {
            return Err("SSH proxy_jump 'none' cannot be combined with jump hosts".into());
        }
        Ok(())
    }

    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            host: None,
            user: None,
            port: None,
            ssh_alias: None,
            ssh_config: None,
            identity_files: vec![],
            known_hosts: None,
            proxy_jump: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "forward_wire::Forward")]
pub struct ForwardSpec {
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    /// A persistent batch name; individual rules retain independent identities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub server_id: String,
    #[serde(flatten)]
    pub tunnel: Tunnel,
    #[serde(default)]
    pub desired_state: DesiredState,
    #[serde(default)]
    pub connection_mode: ConnectionMode,
    #[serde(default)]
    pub remote_cleanup: RemoteCleanup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    pub keepalive_interval_secs: u64,
    pub keepalive_max: usize,
    pub connect_timeout_secs: u64,
    pub max_delay_secs: u64,
    pub stable_reset_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            keepalive_interval_secs: 5,
            keepalive_max: 3,
            connect_timeout_secs: 10,
            max_delay_secs: 30,
            stable_reset_secs: 60,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub retry: RetryPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub servers: Vec<ServerProfile>,
    #[serde(default)]
    pub forwards: Vec<ForwardSpec>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            revision: 0,
            defaults: Defaults::default(),
            servers: vec![],
            forwards: vec![],
        }
    }
}

impl Config {
    /// Version 1 never implemented cleanup, so its `off` value was not an
    /// intentional opt-out. Upgrade existing reverse rules to managed recovery.
    pub fn migrate(&mut self) -> Result<bool, String> {
        match self.schema_version {
            SCHEMA_VERSION => Ok(false),
            1 | 2 => {
                let legacy = self.schema_version;
                for forward in &mut self.forwards {
                    if legacy == 1 && forward.tunnel.is_remote() {
                        forward.remote_cleanup = RemoteCleanup::Verified;
                        forward.connection_mode = ConnectionMode::Dedicated;
                    }
                }
                self.infer_legacy_groups();
                self.schema_version = SCHEMA_VERSION;
                self.revision = self
                    .revision
                    .checked_add(1)
                    .ok_or("configuration revision exhausted during migration")?;
                Ok(true)
            }
            other => Err(format!("unsupported schema version {other}")),
        }
    }
    fn infer_legacy_groups(&mut self) {
        let names: HashSet<_> = self.forwards.iter().map(|rule| rule.name.clone()).collect();
        let ids: HashSet<_> = self.forwards.iter().map(|rule| rule.id.clone()).collect();
        let mut batches: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
        for (index, rule) in self.forwards.iter().enumerate() {
            if rule.group.is_some() {
                continue;
            }
            if let Some((prefix, suffix)) = rule.name.rsplit_once('-')
                && suffix == rule.tunnel.listen().port().to_string()
                && validate_name(prefix).is_ok()
                && !names.contains(prefix)
                && !ids.contains(prefix)
            {
                batches.entry(prefix.into()).or_default().push(index);
            }
        }
        for (name, members) in batches {
            if members.len() > 1 {
                let grouped: Vec<_> = self
                    .forwards
                    .iter()
                    .enumerate()
                    .filter(|(index, rule)| {
                        members.contains(index) || rule.group.as_deref() == Some(&name)
                    })
                    .map(|(_, rule)| rule)
                    .collect();
                let conflicts = grouped.iter().enumerate().any(|(index, a)| {
                    grouped[index + 1..].iter().any(|b| {
                        let same_side = if a.tunnel.is_remote() && b.tunnel.is_remote() {
                            a.server_id == b.server_id
                        } else {
                            !a.tunnel.is_remote() && !b.tunnel.is_remote()
                        };
                        same_side && overlaps(a.tunnel.listen(), b.tunnel.listen())
                    })
                });
                if conflicts {
                    continue;
                }
                for index in members {
                    self.forwards[index].group = Some(name.clone());
                }
            }
        }
    }

    pub fn select_forwards(&self, selector: &str) -> Result<Vec<String>, String> {
        if let Some(rule) = self.forward(selector) {
            return Ok(vec![rule.id.clone()]);
        }
        self.select_group_forwards(selector)
            .map_err(|_| format!("unknown forward or group {selector:?}"))
    }

    pub fn select_group_forwards(&self, group: &str) -> Result<Vec<String>, String> {
        let ids: Vec<_> = self
            .forwards
            .iter()
            .filter(|rule| rule.group.as_deref() == Some(group))
            .map(|rule| rule.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(format!("unknown or empty group {group:?}"));
        }
        Ok(ids)
    }

    pub fn select_server_forwards(&self, selector: &str) -> Result<Vec<String>, String> {
        let server = self
            .server(selector)
            .ok_or_else(|| format!("unknown server {selector:?}"))?;
        Ok(self
            .forwards
            .iter()
            .filter(|rule| rule.server_id == server.id)
            .map(|rule| rule.id.clone())
            .collect())
    }
    pub fn server(&self, selector: &str) -> Option<&ServerProfile> {
        self.servers
            .iter()
            .find(|s| s.id == selector)
            .or_else(|| self.servers.iter().find(|s| s.name == selector))
    }
    pub fn forward(&self, selector: &str) -> Option<&ForwardSpec> {
        self.forwards
            .iter()
            .find(|f| f.id == selector)
            .or_else(|| self.forwards.iter().find(|f| f.name == selector))
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.revision > i64::MAX as u64 {
            return Err(
                "configuration revision exhausted (maximum TOML revision is i64::MAX)".into(),
            );
        }
        if self.servers.len() > 128 || self.forwards.len() > 512 {
            return Err(
                "this release supports at most 128 server profiles and 512 forwards".into(),
            );
        }
        if serde_json::to_vec(self)
            .map_err(|error| error.to_string())?
            .len()
            > 256 * 1024
        {
            return Err("configuration exceeds the 256 KiB management API budget".into());
        }
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema version {}",
                self.schema_version
            ));
        }
        let policy = &self.defaults.retry;
        if policy.keepalive_interval_secs == 0
            || policy.keepalive_max == 0
            || policy.connect_timeout_secs == 0
            || policy.max_delay_secs == 0
            || policy.stable_reset_secs == 0
        {
            return Err("timeouts, heartbeat limits and retry intervals must be positive".into());
        }
        if [
            policy.keepalive_interval_secs,
            policy.connect_timeout_secs,
            policy.max_delay_secs,
            policy.stable_reset_secs,
        ]
        .into_iter()
        .any(|value| value > 31_536_000)
        {
            return Err("timeouts, heartbeat and retry intervals must not exceed one year".into());
        }
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        for server in &self.servers {
            validate_name(&server.name)?;
            validate_id(&server.id)?;
            if server.id.is_empty() || !ids.insert(&server.id) || !names.insert(&server.name) {
                return Err("duplicate or empty server identity/name".into());
            }
            server.validate_connection_options()?;
        }
        for server in &self.servers {
            if server.name != server.id && ids.contains(&server.name) {
                let other = self
                    .servers
                    .iter()
                    .find(|other| other.id == server.name)
                    .unwrap();
                return Err(format!(
                    "server name {:?} on ID {:?} conflicts with another server's ID {:?} (name {:?}); choose a different name, or repair the name in config.toml without changing IDs",
                    server.name, server.id, other.id, other.name
                ));
            }
        }
        let mut forward_ids = HashSet::new();
        let mut forward_names = HashSet::new();
        let mut group_names = HashSet::new();
        for forward in &self.forwards {
            validate_name(&forward.name)?;
            validate_id(&forward.id)?;
            if let Some(group) = &forward.group {
                validate_name(group)?;
                group_names.insert(group);
            }
            if forward.id.is_empty()
                || !forward_ids.insert(&forward.id)
                || !forward_names.insert(&forward.name)
            {
                return Err("duplicate or empty forward identity/name".into());
            }
            if !ids.contains(&forward.server_id) {
                return Err(format!("{} references unknown server", forward.name));
            }
            if forward.tunnel.listen().port() == 0 {
                return Err(
                    "automatic listen port allocation is not supported; choose a fixed port".into(),
                );
            }
            if let Some(target) = forward.tunnel.target() {
                target.to_string().parse::<Endpoint>()?;
            }
            if forward.remote_cleanup == RemoteCleanup::Verified && !forward.tunnel.is_remote() {
                return Err("verified remote cleanup is only valid for remote forwards".into());
            }
        }
        if let Some(conflict) = group_names.intersection(&forward_names).next() {
            return Err(format!(
                "group name {conflict:?} conflicts with an individual forward"
            ));
        }
        for forward in &self.forwards {
            if forward.name != forward.id && forward_ids.contains(&forward.name) {
                let other = self
                    .forwards
                    .iter()
                    .find(|other| other.id == forward.name)
                    .unwrap();
                return Err(format!(
                    "forward name {:?} on ID {:?} conflicts with another forward's ID {:?} (name {:?}); choose a different name, or repair the name in config.toml without changing IDs",
                    forward.name, forward.id, other.id, other.name
                ));
            }
        }
        if let Some(conflict) = group_names.intersection(&forward_ids).next() {
            let forward = self
                .forwards
                .iter()
                .find(|forward| &forward.id == *conflict)
                .unwrap();
            return Err(format!(
                "group name {conflict:?} conflicts with a forward ID (name {:?}); choose a different group name, or repair the group in config.toml without changing forward IDs",
                forward.name
            ));
        }
        for (i, a) in self.forwards.iter().enumerate() {
            for b in &self.forwards[i + 1..] {
                // A group is operated as one unit, so its members must be able
                // to run together even when currently stopped. Separate,
                // stopped alternatives may still reuse a listening address.
                let same_group = a.group.is_some() && a.group == b.group;
                let both_running = a.desired_state == DesiredState::Running
                    && b.desired_state == DesiredState::Running;
                if !same_group && !both_running {
                    continue;
                }
                let same_side = if a.tunnel.is_remote() && b.tunnel.is_remote() {
                    a.server_id == b.server_id
                } else {
                    !a.tunnel.is_remote() && !b.tunnel.is_remote()
                };
                if same_side && overlaps(a.tunnel.listen(), b.tunnel.listen()) {
                    if let Some(group) = a.group.as_deref().filter(|_| same_group) {
                        return Err(format!(
                            "listen addresses conflict within group {group:?}: {} and {}; group members need distinct listening addresses to start together",
                            a.name, b.name
                        ));
                    }
                    return Err(format!(
                        "listen addresses conflict: {} and {}",
                        a.name, b.name
                    ));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn overlaps(a: SocketAddr, b: SocketAddr) -> bool {
    if a.port() != b.port() {
        return false;
    }
    let canonical = |address: IpAddr| match address {
        IpAddr::V6(address) => address
            .to_ipv4_mapped()
            .map_or(IpAddr::V6(address), IpAddr::V4),
        address => address,
    };
    match (canonical(a.ip()), canonical(b.ip())) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a == b || a.is_unspecified() || b.is_unspecified(),
        (IpAddr::V6(a), IpAddr::V6(b)) => a == b || a.is_unspecified() || b.is_unspecified(),
        (IpAddr::V4(_), IpAddr::V6(v6)) | (IpAddr::V6(v6), IpAddr::V4(_)) => v6.is_unspecified(),
    }
}

/// Keep escaped identity metadata within both history and IPC record budgets.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 {
        return Err("resource IDs must contain 1–128 bytes".into());
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 100
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
    {
        return Err(
            "names must contain 1–100 letters, numbers, dots, hyphens or underscores".into(),
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    Stopped,
    Starting,
    Established,
    Backoff,
    NeedsAttention,
    Stopping,
    Unverified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForwardStatus {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    pub server: String,
    pub kind: String,
    pub listen: String,
    pub target: Option<String>,
    pub desired_state: DesiredState,
    pub state: RuntimeState,
    pub retry_count: u32,
    pub next_retry_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub active_connections: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<EventContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub forward_id: Option<String>,
    pub message: String,
}

/// Labels captured by the producer, before configuration changes can relabel an event.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventContext {
    pub forward_name: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub group: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OperationReport {
    pub affected: Vec<String>,
    pub skipped: Vec<SkippedForward>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkippedForward {
    pub id: String,
    pub name: String,
    pub reason: String,
}

#[cfg(test)]
#[path = "group_validation_tests.rs"]
mod group_validation_tests;

#[cfg(test)]
#[path = "model_audit_tests.rs"]
mod audit_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_batches_gain_groups_without_changing_member_identity() {
        let mut server = ServerProfile::new("dev");
        server.host = Some("127.0.0.1".into());
        let mut config = Config {
            schema_version: 2,
            revision: 7,
            servers: vec![server.clone()],
            ..Config::default()
        };
        for port in [3000, 3001] {
            config.forwards.push(ForwardSpec {
                id: format!("member-{port}"),
                name: format!("web-{port}"),
                group: None,
                server_id: server.id.clone(),
                tunnel: Tunnel::Local {
                    listen: ([127, 0, 0, 1], port).into(),
                    target: format!("localhost:{port}").parse().unwrap(),
                },
                desired_state: DesiredState::Stopped,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            });
        }
        let ids = config
            .forwards
            .iter()
            .map(|rule| rule.id.clone())
            .collect::<Vec<_>>();
        assert!(config.migrate().unwrap());
        assert_eq!(config.revision, 8);
        assert_eq!(config.select_forwards("web").unwrap(), ids);
        assert!(
            config
                .forwards
                .iter()
                .all(|rule| rule.group.as_deref() == Some("web")
                    && rule.desired_state == DesiredState::Stopped)
        );
        assert!(!config.migrate().unwrap());
        config.validate().unwrap();
    }
    #[test]
    fn endpoint_handles_ipv6_and_rejects_ambiguous_or_invalid_input() {
        let target: Endpoint = "[::1]:5432".parse().unwrap();
        assert_eq!(target.host, "::1");
        assert_eq!(target.to_string(), "[::1]:5432");
        for invalid in ["::1:5432", "host:0", "host:65536", "bad host:22"] {
            assert!(invalid.parse::<Endpoint>().is_err());
        }
    }
    #[test]
    fn dangling_server_and_wildcard_conflicts_are_rejected() {
        let mut config = Config::default();
        let mut server = ServerProfile::new("dev");
        server.host = Some("example.com".into());
        config.servers.push(server.clone());
        let first = ForwardSpec {
            group: None,
            id: new_id(),
            name: "a".into(),
            server_id: server.id,
            tunnel: Tunnel::Dynamic {
                listen: "127.0.0.1:8080".parse().unwrap(),
            },
            desired_state: DesiredState::Running,
            connection_mode: ConnectionMode::Shared,
            remote_cleanup: RemoteCleanup::Off,
        };
        config.forwards.push(first.clone());
        assert!(config.validate().is_ok());
        let mut second = first;
        second.id = new_id();
        second.name = "b".into();
        second.tunnel = Tunnel::Dynamic {
            listen: "0.0.0.0:8080".parse().unwrap(),
        };
        config.forwards.push(second);
        assert!(config.validate().unwrap_err().contains("conflict"));
        config.forwards[1].desired_state = DesiredState::Stopped;
        assert!(config.validate().is_ok());
        config.servers.clear();
        assert!(config.validate().is_err());
    }
}
