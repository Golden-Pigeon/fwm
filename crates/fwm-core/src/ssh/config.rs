use std::{
    collections::HashMap,
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
};

use serde::Serialize;

use super::SshError;
use crate::model::ServerProfile;

/// The effective, deliberately supported subset of an OpenSSH client profile.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedServer {
    pub alias: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_files: Vec<PathBuf>,
    pub explicit_identity_files: bool,
    pub known_hosts: PathBuf,
    pub global_known_hosts: Vec<PathBuf>,
    pub proxy_jump: Vec<String>,
    pub identity_agent: Option<String>,
    pub identities_only: bool,
    pub host_key_alias: Option<String>,
    pub ssh_config: PathBuf,
}

impl ResolvedServer {
    pub fn trust_host(&self) -> &str {
        self.host_key_alias.as_deref().unwrap_or(&self.host)
    }
}

pub fn resolve(profile: &ServerProfile) -> Result<ResolvedServer, SshError> {
    let home = directories::BaseDirs::new()
        .ok_or_else(|| config_error("cannot determine the home directory"))?
        .home_dir()
        .to_path_buf();
    resolve_with_home(profile, &home)
}

fn resolve_with_home(profile: &ServerProfile, home: &Path) -> Result<ResolvedServer, SshError> {
    profile
        .validate_connection_options()
        .map_err(config_error)?;
    let alias = profile
        .ssh_alias
        .as_deref()
        .or(profile.host.as_deref())
        .ok_or_else(|| config_error("server needs a host or SSH alias"))?;
    let ssh_config = profile
        .ssh_config
        .as_ref()
        .map(|path| expand_home(&path.to_string_lossy(), home).map(PathBuf::from))
        .transpose()?
        .unwrap_or_else(|| home.join(".ssh/config"));
    let mut parsed = Parsed::default();
    if ssh_config.exists() {
        let mut active = true;
        parse_file(
            &ssh_config,
            alias,
            home,
            &mut active,
            &mut parsed,
            &mut Vec::new(),
        )?;
    } else if profile.ssh_config.is_some() {
        return Err(config_error(format!(
            "SSH config does not exist: {}",
            ssh_config.display()
        )));
    }
    let initial_user = profile
        .user
        .clone()
        .or_else(|| parsed.one("user"))
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok());
    let host = profile
        .host
        .clone()
        .or_else(|| parsed.one("hostname"))
        .unwrap_or_else(|| alias.to_owned());
    let mut host = expand_tokens(
        &host,
        alias,
        alias,
        initial_user.as_deref().unwrap_or_default(),
        profile.port.unwrap_or(22),
        home,
    )?;
    if let Some(mode) = parsed
        .one("canonicalizehostname")
        .filter(|mode| mode != "no")
    {
        // No CanonicalDomains/CNAME rules are accepted yet, so the supported
        // default needs no DNS here. Keep resolution out of the control lock.
        let proxied = !effective_proxy_jump(profile, &parsed).is_empty();
        host = canonical_host(&host, &mode, proxied)?;
        // OpenSSH pins the final destination before the second pass. Scalars
        // already obtained keep precedence; matching Host blocks can fill gaps.
        parsed.values.insert("hostname".into(), vec![host.clone()]);
        let mut active = true;
        parse_file(
            &ssh_config,
            &host,
            home,
            &mut active,
            &mut parsed,
            &mut Vec::new(),
        )?;
    }
    let user = profile
        .user
        .clone()
        .or_else(|| parsed.one("user"))
        .or(initial_user)
        .ok_or_else(|| config_error("no SSH user configured and USER/USERNAME is unset"))?;
    let port = profile
        .port
        .or(parsed
            .one("port")
            .map(|v| {
                v.parse::<u16>()
                    .map_err(|_| config_error("invalid SSH Port"))
            })
            .transpose()?)
        .unwrap_or(22);
    if host.trim().is_empty()
        || user.trim().is_empty()
        || port == 0
        || host.contains(['\n', '\r', '\0'])
    {
        return Err(config_error(
            "host/user must be nonempty and port must be between 1 and 65535",
        ));
    }
    let expand_path = |s: &str| -> Result<PathBuf, SshError> {
        Ok(PathBuf::from(expand_tokens(
            s, alias, &host, &user, port, home,
        )?))
    };
    let mut identity_files = if !profile.identity_files.is_empty() {
        profile
            .identity_files
            .iter()
            .map(|p| expand_path(&p.to_string_lossy()))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        parsed
            .identities
            .iter()
            .filter(|v| !v.eq_ignore_ascii_case("none"))
            .map(|v| expand_path(v))
            .collect::<Result<Vec<_>, _>>()?
    };
    if identity_files.is_empty()
        && parsed.identities.is_empty()
        && profile.identity_files.is_empty()
    {
        identity_files
            .extend(["id_ed25519", "id_ecdsa", "id_rsa"].map(|name| home.join(".ssh").join(name)));
    }
    let known_hosts = if let Some(path) = &profile.known_hosts {
        expand_path(&path.to_string_lossy())?
    } else if let Some(paths) = parsed.values.get("userknownhostsfile") {
        if paths.len() != 1 || paths[0].eq_ignore_ascii_case("none") {
            return Err(config_error(
                "UserKnownHostsFile needs exactly one real path; disabling verification is not supported",
            ));
        }
        expand_path(&paths[0])?
    } else {
        home.join(".ssh/known_hosts")
    };
    let global_known_hosts = if let Some(paths) = parsed.values.get("globalknownhostsfile") {
        paths
            .iter()
            .filter(|p| !p.eq_ignore_ascii_case("none"))
            .map(|p| expand_path(p))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        #[cfg(unix)]
        {
            vec![
                PathBuf::from("/etc/ssh/ssh_known_hosts"),
                PathBuf::from("/etc/ssh/ssh_known_hosts2"),
            ]
        }
        #[cfg(windows)]
        {
            vec![]
        }
    };
    let proxy_jump = effective_proxy_jump(profile, &parsed);
    let identity_agent = parsed
        .one("identityagent")
        .map(|s| {
            if s == "SSH_AUTH_SOCK" {
                return Ok(std::env::var("SSH_AUTH_SOCK").unwrap_or_default());
            }
            expand_tokens(&s, alias, &host, &user, port, home)
        })
        .transpose()?;
    Ok(ResolvedServer {
        alias: alias.into(),
        host,
        user,
        port,
        identity_files,
        explicit_identity_files: !profile.identity_files.is_empty()
            || !parsed.identities.is_empty(),
        known_hosts,
        global_known_hosts,
        proxy_jump,
        identity_agent,
        identities_only: parsed
            .one("identitiesonly")
            .map(|v| boolean("IdentitiesOnly", &v))
            .transpose()?
            .unwrap_or(false),
        host_key_alias: parsed.one("hostkeyalias"),
        ssh_config,
    })
}

#[derive(Default)]
struct Parsed {
    values: HashMap<String, Vec<String>>,
    identities: Vec<String>,
}

impl Parsed {
    fn one(&self, key: &str) -> Option<String> {
        self.values.get(key).and_then(|v| v.first()).cloned()
    }
}

fn effective_proxy_jump(profile: &ServerProfile, parsed: &Parsed) -> Vec<String> {
    if profile.proxy_jump.len() == 1 && profile.proxy_jump[0].eq_ignore_ascii_case("none") {
        vec![]
    } else if !profile.proxy_jump.is_empty() {
        profile.proxy_jump.clone()
    } else {
        parsed
            .one("proxyjump")
            .filter(|value| !value.eq_ignore_ascii_case("none"))
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

fn canonical_host(host: &str, mode: &str, proxied: bool) -> Result<String, SshError> {
    if let Ok(address) = host.parse::<IpAddr>() {
        // OpenSSH's numeric resolver uses mixed notation for IPv4-compatible
        // IPv6 addresses when the first nonzero 16-bit group is the seventh.
        if let IpAddr::V6(ipv6) = address
            && ipv6.segments()[..6] == [0; 6]
            && ipv6.segments()[6] != 0
            && let Some(ipv4) = ipv6.to_ipv4()
        {
            return Ok(format!("::{ipv4}"));
        }
        return Ok(address.to_string());
    }
    let legacy_numeric = host.split('.').all(|part| {
        let part = part.to_ascii_lowercase();
        if let Some(hex) = part.strip_prefix("0x") {
            !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        } else {
            !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
        }
    });
    if host.contains(':') || legacy_numeric {
        return Err(config_error(
            "CanonicalizeHostname requires a standard IPv4/IPv6 literal; scoped or legacy numeric address forms are unsupported",
        ));
    }
    // For a direct absolute DNS name, OpenSSH removes the trailing dot only
    // after a successful DNS lookup. Do not silently emulate a different host.
    if host.ends_with('.') && (mode == "always" || !proxied) {
        return Err(config_error(
            "CanonicalizeHostname for a trailing-dot DNS name requires DNS canonicalization, which is unsupported; use a standard IP address or a dedicated --ssh-config with CanonicalizeHostname no",
        ));
    }
    Ok(host.to_ascii_lowercase())
}

fn parse_file(
    path: &Path,
    alias: &str,
    home: &Path,
    active: &mut bool,
    result: &mut Parsed,
    stack: &mut Vec<PathBuf>,
) -> Result<(), SshError> {
    let canonical = std::fs::canonicalize(path)?;
    if stack.len() >= 16 || stack.contains(&canonical) {
        return Err(config_error(
            "recursive SSH Include or more than 16 include levels",
        ));
    }
    stack.push(canonical.clone());
    // Never open a FIFO/device as a configuration stream. Includes use the
    // same check, so a broken external file cannot stall the control loop.
    if !std::fs::metadata(path)?.is_file() {
        return Err(config_error(format!(
            "SSH config must be a regular file: {}",
            path.display()
        )));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(config_error("SSH config changed to a non-regular file"));
    }
    let mut contents = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut contents)?;
    if contents.len() > 1024 * 1024 {
        return Err(config_error("SSH config exceeds 1 MiB"));
    }
    for (line_number, line) in contents.lines().enumerate() {
        let tokens = tokenize(line)
            .map_err(|e| config_error(format!("{}:{}: {e}", path.display(), line_number + 1)))?;
        if tokens.is_empty() {
            continue;
        }
        let key = tokens[0].to_ascii_lowercase();
        let args = &tokens[1..];
        if args.is_empty() {
            return Err(config_error(format!(
                "{}:{}: {} requires a value",
                path.display(),
                line_number + 1,
                tokens[0]
            )));
        }
        // OpenSSH selects the first obtained scalar value. Unsupported defaults
        // that are shadowed by an earlier supported value have no effect.
        if *active && result.values.contains_key(&key) {
            continue;
        }
        if *active
            && args.len() != 1
            && matches!(
                key.as_str(),
                "hostname"
                    | "user"
                    | "port"
                    | "proxyjump"
                    | "identityagent"
                    | "identitiesonly"
                    | "hostkeyalias"
                    | "stricthostkeychecking"
                    | "forwardagent"
                    | "forwardx11"
                    | "forwardx11trusted"
                    | "compression"
                    | "usekeychain"
                    | "canonicalizehostname"
            )
        {
            return Err(config_error(format!(
                "{}:{}: {} expects exactly one value",
                path.display(),
                line_number + 1,
                tokens[0]
            )));
        }
        match key.as_str() {
            "host" => *active = matches_patterns(alias, args.iter().map(String::as_str)),
            "match" => {
                return Err(config_error(format!(
                    "{}:{}: Match is unsupported; use Host blocks or a dedicated --ssh-config file",
                    path.display(),
                    line_number + 1
                )));
            }
            "include" if *active => {
                for arg in args {
                    let expanded = expand_home(arg, home)?;
                    let include = if Path::new(&expanded).is_absolute() {
                        PathBuf::from(expanded)
                    } else {
                        home.join(".ssh").join(expanded)
                    };
                    let mut paths = glob::glob(&include.to_string_lossy())
                        .map_err(|e| config_error(e.to_string()))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| config_error(e.to_string()))?;
                    paths.sort();
                    for include in paths {
                        let mut included_active = *active;
                        parse_file(&include, alias, home, &mut included_active, result, stack)?;
                    }
                }
            }
            _ if !*active => {}
            "identityfile" => {
                for arg in args {
                    let path = source_path(arg, &canonical)?;
                    if !result.identities.contains(&path) {
                        result.identities.push(path);
                    }
                }
            }
            "identityagent" | "userknownhostsfile" | "globalknownhostsfile" => {
                let paths = args
                    .iter()
                    .map(|arg| {
                        if key == "identityagent" && arg.starts_with('$') {
                            Err(config_error("IdentityAgent $ENV expansion is unsupported; use SSH_AUTH_SOCK or an explicit path"))
                        } else {
                            source_path(arg, &canonical)
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                result.values.entry(key).or_insert(paths);
            }
            "hostname" | "user" | "port" | "proxyjump" | "hostkeyalias" => {
                result.values.entry(key).or_insert_with(|| args.to_vec());
            }
            "identitiesonly" => {
                let value = boolean("IdentitiesOnly", &args[0])?;
                result
                    .values
                    .entry(key)
                    .or_insert_with(|| vec![if value { "yes" } else { "no" }.into()]);
            }
            "canonicalizehostname" => {
                let value = match args[0].to_ascii_lowercase().as_str() {
                    "no" | "false" => "no",
                    "yes" | "true" => "yes",
                    "always" => "always",
                    _ => {
                        return Err(config_error(format!(
                            "{}:{}: CanonicalizeHostname expects no, yes or always",
                            path.display(),
                            line_number + 1
                        )));
                    }
                };
                result.values.insert(key, vec![value.into()]);
            }
            // These control the OpenSSH process/UI or features the forwarding
            // manager itself owns. They do not weaken authentication or trust.
            "addkeystoagent"
            | "loglevel"
            | "visualhostkey"
            | "hashknownhosts"
            | "controlmaster"
            | "controlpath"
            | "controlpersist"
            | "serveraliveinterval"
            | "serveralivecountmax"
            | "tcpkeepalive"
            | "exitonforwardfailure"
            | "requesttty"
            | "sessiontype"
            | "batchmode" => {}
            "stricthostkeychecking" => {
                if !["yes", "true", "ask"].contains(&args[0].to_ascii_lowercase().as_str()) {
                    return Err(config_error(
                        "StrictHostKeyChecking may not disable explicit trust",
                    ));
                }
                result.values.insert(key, args.to_vec());
            }
            "forwardagent" | "forwardx11" | "forwardx11trusted" | "compression" | "usekeychain" => {
                let enabled = if key == "compression" {
                    match args[0].to_ascii_lowercase().as_str() {
                        "yes" => true,
                        "no" => false,
                        _ => return Err(config_error("Compression expects yes or no")),
                    }
                } else {
                    boolean(&key, &args[0])?
                };
                if enabled {
                    return Err(config_error(format!("{key}={} is unsupported", args[0])));
                }
                result.values.insert(key, args.to_vec());
            }
            "proxycommand" if args[0].eq_ignore_ascii_case("none") => {
                result.values.insert(key, args.to_vec());
            }
            _ => {
                return Err(config_error(format!(
                    "{}:{}: unsupported SSH directive {}; use a dedicated --ssh-config file",
                    path.display(),
                    line_number + 1,
                    tokens[0]
                )));
            }
        }
    }
    stack.pop();
    Ok(())
}

fn boolean(name: &str, value: &str) -> Result<bool, SshError> {
    match value.to_ascii_lowercase().as_str() {
        "yes" | "true" => Ok(true),
        "no" | "false" => Ok(false),
        _ => Err(config_error(format!(
            "{name} expects yes/no or true/false, got {value:?}"
        ))),
    }
}

fn source_path(value: &str, source: &Path) -> Result<String, SshError> {
    if value.eq_ignore_ascii_case("none") || value == "SSH_AUTH_SOCK" {
        return Ok(value.into());
    }
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    Ok(
        super::path_options::normalized_path(Path::new(value), parent)?
            .to_string_lossy()
            .into_owned(),
    )
}

/// SSH Host patterns, including exclusions. Unlike filesystem glob, `[` and
/// `]` are literal characters (important for IPv6/known_hosts port syntax).
pub(super) fn matches_patterns<'a>(
    value: &str,
    patterns: impl IntoIterator<Item = &'a str>,
) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negative) = pattern.strip_prefix('!') {
            if wildcard_match_case(negative, value, false) {
                return false;
            }
        } else if wildcard_match_case(pattern, value, false) {
            matched = true;
        }
    }
    matched
}

pub(super) fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_case(pattern, value, true)
}

fn wildcard_match_case(pattern: &str, value: &str, ignore_case: bool) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut checkpoint) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len()
            && (pattern[p] == b'?'
                || pattern[p] == value[v]
                || (ignore_case && pattern[p].eq_ignore_ascii_case(&value[v])))
        {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            checkpoint = v;
        } else if let Some(s) = star {
            checkpoint += 1;
            v = checkpoint;
            p = s + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn tokenize(line: &str) -> Result<Vec<String>, &'static str> {
    let (mut output, mut current, mut quote, mut escaped) =
        (Vec::new(), String::new(), None, false);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        if c == '\\'
            && chars
                .peek()
                .is_some_and(|next| *next == '\\' || *next == '"' || *next == '\'')
        {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '#' {
            break;
        } else if c.is_whitespace()
            || (c == '=' && (output.is_empty() || (output.len() == 1 && current.is_empty())))
        {
            if !current.is_empty() {
                output.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if escaped || quote.is_some() {
        return Err("unterminated quote or escape");
    }
    if !current.is_empty() {
        output.push(current);
    }
    Ok(output)
}

fn expand_home(value: &str, home: &Path) -> Result<String, SshError> {
    if value == "~" {
        Ok(home.to_string_lossy().into_owned())
    } else if let Some(rest) = value.strip_prefix("~/") {
        Ok(home.join(rest).to_string_lossy().into_owned())
    } else if value.starts_with('~') {
        Err(config_error("~otheruser expansion is not supported"))
    } else {
        Ok(value.to_owned())
    }
}

fn expand_tokens(
    value: &str,
    alias: &str,
    host: &str,
    user: &str,
    port: u16,
    home: &Path,
) -> Result<String, SshError> {
    let expanded = expand_home(value, home)?;
    let mut chars = expanded.chars();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if c != '%' {
            result.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => result.push('%'),
            Some('h') => result.push_str(host),
            Some('n') => result.push_str(alias),
            Some('r') => result.push_str(user),
            Some('p') => result.push_str(&port.to_string()),
            Some('d') => result.push_str(&home.to_string_lossy()),
            token => return Err(config_error(format!("unsupported SSH token %{token:?}"))),
        }
    }
    if result.contains("${") {
        return Err(config_error(
            "SSH ${ENV} expansion is unsupported; use an explicit path",
        ));
    }
    Ok(result)
}

fn config_error(message: impl Into<String>) -> SshError {
    SshError::Configuration(message.into())
}

#[cfg(test)]
#[path = "config_override_tests.rs"]
mod override_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_patterns_and_quotes() {
        assert!(matches_patterns("dev.example", ["*.example", "!prod.*"]));
        assert!(!matches_patterns("prod.example", ["*.example", "!prod.*"]));
        assert!(wildcard_match("[::1]:2222", "[::1]:2222"));
        assert_eq!(
            tokenize("IdentityFile = \"/a b/key\" # comment").unwrap(),
            vec!["IdentityFile", "/a b/key"]
        );
        assert_eq!(
            tokenize("HostName=example.org").unwrap(),
            vec!["HostName", "example.org"]
        );
    }

    #[test]
    fn includes_preserve_first_value_and_excluded_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("parts")).unwrap();
        std::fs::write(ssh.join("config"), "Host dev\n HostName 127.0.0.1\n User alice\n Include parts/*.conf\nHost * !dev\n ProxyCommand dangerous ignored on other hosts\nHost *\n User fallback\n").unwrap();
        std::fs::write(
            ssh.join("parts/a.conf"),
            "Port 2200\nIdentityFile \"~/keys/my key\"\nProxyJump jump\n",
        )
        .unwrap();
        let mut profile = ServerProfile::new("dev");
        profile.ssh_alias = Some("dev".into());
        let server = resolve_with_home(&profile, dir.path()).unwrap();
        assert_eq!(server.user, "alice");
        assert_eq!(server.host, "127.0.0.1");
        assert_eq!(server.port, 2200);
        assert_eq!(server.proxy_jump, vec!["jump"]);
        assert_eq!(server.identity_files, vec![dir.path().join("keys/my key")]);
    }

    #[test]
    fn unsafe_or_unimplemented_connection_options_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let mut profile = ServerProfile::new("dev");
        profile.ssh_alias = Some("dev".into());
        for setting in [
            "ProxyCommand nc %h %p",
            "StrictHostKeyChecking no",
            "CertificateFile cert.pub",
            "Match exec echo",
        ] {
            std::fs::write(ssh.join("config"), format!("Host dev\n {setting}\n")).unwrap();
            assert!(
                resolve_with_home(&profile, dir.path()).is_err(),
                "accepted unsupported setting {setting}"
            );
        }
    }

    #[test]
    fn include_cycles_fail_without_recursing_forever() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Include config\n").unwrap();
        let mut profile = ServerProfile::new("dev");
        profile.ssh_alias = Some("dev".into());
        assert!(
            resolve_with_home(&profile, dir.path())
                .unwrap_err()
                .to_string()
                .contains("recursive")
        );
    }
}

#[cfg(test)]
#[path = "config_canonical_tests.rs"]
mod canonical_tests;
