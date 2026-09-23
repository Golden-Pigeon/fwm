use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(
    name = "fwm",
    version,
    about = "Manage persistent SSH port forwards across servers"
)]
pub struct Cli {
    /// Use a separate configuration directory and daemon instance (default: ~/.fwm).
    #[arg(long, global = true)]
    pub config_dir: Option<PathBuf>,
    /// Emit machine-readable JSON (one object per update for watch commands).
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Keep legacy forwarding specifications compatible without letting a
    /// direction flag consume a positional rule name.
    pub fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(super::input::normalize(arguments))
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print the project and bundled-component license notices.
    Licenses,
    /// Print a shell script for dynamic command, server, rule and group completion.
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Manage SSH server profiles and trusted host keys.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Save one or more forwards and start them unless --disabled is supplied.
    Add(AddArgs),
    /// Modify a forward or group without restarting unrelated forwards.
    Edit(EditArgs),
    /// Discover saved forwarding groups and their members.
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Show actual connection and listener state.
    Status(StatusArgs),
    /// Persist running intent; optionally wait for listeners to be ready.
    Up(UpArgs),
    /// Persist stopped intent and close selected forwards.
    Down(SelectionArgs),
    /// Retry selected running forwards immediately.
    Retry(SelectionArgs),
    /// Restart selected forwards; optionally wait for their listeners.
    Restart(UpArgs),
    /// Delete selected forwards and stop their listeners.
    Remove(SelectionArgs),
    /// Display recent events from the daemon.
    Logs(LogsArgs),
    /// Check configuration, SSH connectivity, trust and authentication.
    Doctor {
        #[arg(long)]
        server: Option<String>,
        /// SSH config file when checking a direct alias.
        #[arg(long, requires = "server", value_name = "PATH")]
        ssh_config: Option<PathBuf>,
    },
    /// Validate, apply, or export configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Install or remove login startup for the current user.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    Add(ServerArgs),
    /// Update or rename a saved server without replacing its identity.
    Edit(ServerEditArgs),
    List,
    Remove {
        name: String,
    },
    /// Inspect a host key and explicitly trust its fingerprint.
    Trust {
        name: String,
        /// Exact expected SHA256 fingerprint; required to trust a new key unattended.
        #[arg(long)]
        fingerprint: Option<String>,
        /// SSH config file when using an unregistered alias.
        #[arg(long)]
        ssh_config: Option<PathBuf>,
        /// Trust one jump using this target's route and trust database; alias or 1-based hop number.
        #[arg(long)]
        hop: Option<String>,
    },
    /// Check a saved server or SSH alias directly.
    Check {
        name: String,
        #[arg(long)]
        ssh_config: Option<PathBuf>,
    },
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("address").required(true).args(["ssh", "host"])))]
pub struct ServerArgs {
    pub name: String,
    /// Resolve a Host entry in the SSH config.
    #[arg(long)]
    pub ssh: Option<String>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long = "identity")]
    pub identity_files: Vec<PathBuf>,
    #[arg(long)]
    pub ssh_config: Option<PathBuf>,
    #[arg(long)]
    pub known_hosts: Option<PathBuf>,
    /// Comma-separated jump hosts or SSH aliases; 'none' disables inherited jumps.
    #[arg(long, value_delimiter = ',')]
    pub proxy_jump: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ServerEditArgs {
    /// Existing server name or ID.
    pub name: String,
    #[arg(long)]
    pub rename: Option<String>,
    /// Resolve this SSH alias instead of the previous address.
    #[arg(long, conflicts_with = "host")]
    pub ssh: Option<String>,
    #[arg(long, conflicts_with = "ssh")]
    pub host: Option<String>,
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    /// Replace the list of identity files (repeat to use multiple files).
    #[arg(long = "identity")]
    pub identity_files: Option<Vec<PathBuf>>,
    #[arg(long)]
    pub ssh_config: Option<PathBuf>,
    #[arg(long)]
    pub known_hosts: Option<PathBuf>,
    /// Replace the jump hosts; 'none' disables inherited jumps.
    #[arg(long, value_delimiter = ',')]
    pub proxy_jump: Option<Vec<String>>,
    /// Remove overrides and inherit SSH config/defaults (comma-separated or repeated).
    #[arg(long, value_enum, value_delimiter = ',')]
    pub unset: Vec<ServerField>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ServerField {
    User,
    Port,
    Identity,
    SshConfig,
    KnownHosts,
    ProxyJump,
}

#[derive(Debug, Args)]
#[command(
    group(ArgGroup::new("tunnel").required(true).args(["local", "remote", "dynamic", "remote_dynamic"])),
    override_usage = "fwm add --server SERVER [OPTIONS] --local|--remote --port PORTS\n       fwm add --server SERVER [OPTIONS] --local|--remote --src PORTS --tgt PORT\n       fwm add --server SERVER [OPTIONS] --dynamic|--remote-dynamic [bind:]PORT",
    after_help = "Examples:\n  fwm add --server example-cluster --remote --src 12222 --tgt 22\n  fwm add --server dev --local --port 3000-3003,8080\n  fwm add --server dev --remote --port 7890 --name proxy\n  fwm add --server dev --local=3000:localhost:8080\n  fwm add --server example-cluster --remote-dynamic 127.0.0.1:7897\n\nNames default to a random English word. --server accepts an existing profile or\nan SSH alias/hostname directly. --server is required for every add.\nLegacy positional names and -L/-R SPEC remain supported.\nMulti-port adds also create a group: NAME, or a random English word by default.\nUse --group GROUP to add any number of members to a new or existing group.\n--name independently sets the rule name or multi-port name prefix.\nExample: fwm add --server dev --local --port 8080 --group web\nExample: fwm down --group web"
)]
pub struct AddArgs {
    /// Optional legacy positional name; --name is preferred.
    #[arg(value_name = "NAME", conflicts_with = "explicit_name")]
    pub name: Option<String>,
    /// Rule name or multi-port name prefix; defaults to a random English word.
    #[arg(long = "name", value_name = "NAME", conflicts_with = "name")]
    pub explicit_name: Option<String>,
    /// Put one or more new forwards in this group, creating it if needed.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,
    /// Saved server profile or SSH alias/hostname (required).
    #[arg(long, value_name = "SERVER")]
    pub server: String,
    /// SSH config file for a new alias/hostname profile.
    #[arg(long, value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,
    /// Listen locally; use --port/--src, or --local=[bind:]port:host:port.
    #[arg(long, short = 'L', num_args = 0..=1, default_missing_value = "", require_equals = true, value_name = "SPEC")]
    pub local: Option<String>,
    /// Listen remotely; use --port/--src, or --remote=[bind:]port:host:port.
    #[arg(long, short = 'R', num_args = 0..=1, default_missing_value = "", require_equals = true, value_name = "SPEC")]
    pub remote: Option<String>,
    /// Local SOCKS5 CONNECT proxy at [bind_address:]port, with remote egress.
    #[arg(long, short = 'D')]
    pub dynamic: Option<String>,
    /// Remote SOCKS5 CONNECT proxy at [bind_address:]port, with local egress.
    #[arg(long, value_name = "SPEC")]
    pub remote_dynamic: Option<String>,
    #[command(flatten)]
    pub ports: PortArgs,
    #[arg(long)]
    pub disabled: bool,
    /// SSH connection sharing; verified remote cleanup always uses dedicated.
    #[arg(long, value_enum, default_value_t = Mode::Shared)]
    pub connection_mode: Mode,
    /// Reclaim this rule's registered stale SSH sessions (remote default: verified).
    #[arg(long, value_enum, long_help = REMOTE_CLEANUP_HELP)]
    pub remote_cleanup: Option<CleanupMode>,
    #[arg(long)]
    pub wait: bool,
    /// Wait up to this duration; implies --wait. --wait alone uses 20s.
    #[arg(long, value_parser = duration)]
    pub timeout: Option<Duration>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Mode {
    Shared,
    Dedicated,
}

const REMOTE_CLEANUP_HELP: &str = "Reclaim only stale SSH sessions registered by fwm for this remote forwarding rule. Remote forwards, including remote dynamic SOCKS, default to verified; local forwards and local dynamic SOCKS use off. Verified cleanup always uses a dedicated SSH connection, including when --connection-mode shared is requested. It requires command execution on a supported Linux, macOS, or Windows server, without sudo. Use off for restricted SSH servers which cannot run the helper; fwm never silently disables verified cleanup.";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CleanupMode {
    Verified,
    Off,
}

#[derive(Clone, Debug, Default, Args)]
pub struct PortArgs {
    /// Forward each port to localhost at the same port (e.g. 3000-3003,8080).
    #[arg(long, value_name = "PORTS", conflicts_with_all = ["src", "tgt", "dynamic", "remote_dynamic"])]
    pub port: Option<String>,
    /// Listen on these ports and forward all of them to --tgt (e.g. 5000-5003,6000).
    #[arg(long, value_name = "PORTS", requires = "tgt", conflicts_with_all = ["port", "dynamic", "remote_dynamic"])]
    pub src: Option<String>,
    /// One localhost destination port for all --src ports.
    #[arg(long, value_name = "PORT", requires = "src", conflicts_with_all = ["port", "dynamic", "remote_dynamic"])]
    pub tgt: Option<String>,
}

#[derive(Debug, Default, Args)]
pub struct EditPortArgs {
    /// Replace listen and destination ports with the same port.
    #[arg(long, value_name = "PORT", conflicts_with_all = ["src", "tgt", "dynamic", "remote_dynamic"])]
    pub port: Option<String>,
    /// Change only the listening port; preserve bind address and destination.
    #[arg(long, value_name = "PORT", conflicts_with_all = ["port", "dynamic", "remote_dynamic"])]
    pub src: Option<String>,
    /// Change only the destination port; preserve its host and listening address.
    #[arg(long, value_name = "PORT", conflicts_with_all = ["port", "dynamic", "remote_dynamic"])]
    pub tgt: Option<String>,
}

pub trait PortOptions {
    fn values(&self) -> PortArgs;
}
impl PortOptions for PortArgs {
    fn values(&self) -> PortArgs {
        self.clone()
    }
}
impl PortOptions for EditPortArgs {
    fn values(&self) -> PortArgs {
        PortArgs {
            port: self.port.clone(),
            src: self.src.clone(),
            tgt: self.tgt.clone(),
        }
    }
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("tunnel").args(["local", "remote", "dynamic", "remote_dynamic"])))]
pub struct EditArgs {
    /// Existing forward name, ID, or group.
    pub name: String,
    /// Rename a rule or group while preserving member identities.
    #[arg(long)]
    pub rename: Option<String>,
    /// Add this rule to a group, or explicitly move selected group members.
    #[arg(long, conflicts_with = "ungroup", value_name = "GROUP")]
    pub group: Option<String>,
    /// Remove selected rules from their group, preserving names and identities.
    #[arg(long, conflicts_with = "group")]
    pub ungroup: bool,
    /// Move to a saved server or SSH alias.
    #[arg(long)]
    pub server: Option<String>,
    /// SSH config file when moving to a new server alias.
    #[arg(long, requires = "server", value_name = "PATH")]
    pub ssh_config: Option<PathBuf>,
    /// Switch to local forwarding, preserving unspecified addresses and ports.
    #[arg(long, short = 'L', num_args = 0..=1, default_missing_value = "", require_equals = true, value_name = "SPEC")]
    pub local: Option<String>,
    /// Switch to remote forwarding, preserving unspecified addresses and ports.
    #[arg(long, short = 'R', num_args = 0..=1, default_missing_value = "", require_equals = true, value_name = "SPEC")]
    pub remote: Option<String>,
    /// Replace with a local SOCKS5 CONNECT proxy at [bind_address:]port.
    #[arg(long, short = 'D')]
    pub dynamic: Option<String>,
    /// Remote SOCKS5 CONNECT proxy at [bind_address:]port, with local egress.
    #[arg(long, value_name = "SPEC")]
    pub remote_dynamic: Option<String>,
    #[command(flatten)]
    pub ports: EditPortArgs,
    /// SSH connection sharing; verified remote cleanup always uses dedicated.
    #[arg(long, value_enum)]
    pub connection_mode: Option<Mode>,
    /// Reclaim this rule's registered stale SSH sessions (remote default: verified).
    #[arg(long, value_enum, long_help = REMOTE_CLEANUP_HELP)]
    pub remote_cleanup: Option<CleanupMode>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("selection").required(true).args(["name", "server", "group", "all"])))]
pub struct SelectionArgs {
    pub name: Option<String>,
    #[arg(long)]
    pub server: Option<String>,
    /// Select all forwards in a saved batch group.
    #[arg(long)]
    pub group: Option<String>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Default, Args)]
#[command(group(ArgGroup::new("query_selection").args(["name", "server", "group", "all"])))]
pub struct QueryArgs {
    pub name: Option<String>,
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub group: Option<String>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[command(flatten)]
    pub selection: QueryArgs,
    #[arg(long)]
    pub watch: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub selection: QueryArgs,
    #[arg(long)]
    pub follow: bool,
    /// Show the most recent N matching events before following new events.
    #[arg(long, default_value_t = 100)]
    pub tail: usize,
}

#[derive(Debug, Args)]
pub struct UpArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,
    #[arg(long)]
    pub wait: bool,
    /// Wait up to this duration; implies --wait. --wait alone uses 20s.
    #[arg(long, value_parser = duration)]
    pub timeout: Option<Duration>,
}

pub fn wait_effective(wait: bool, timeout: Option<Duration>) -> Option<Duration> {
    timeout.or_else(|| wait.then_some(Duration::from_secs(20)))
}

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
    /// List groups with member names and server names.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Validate,
    Reload,
    Export,
    /// Restore the applied snapshot from config.toml, back up both files, and keep every rule stopped.
    Recover {
        #[arg(long, required = true)]
        from_candidate: bool,
        /// Explicitly discard unreadable pending stop/delete intent; backups are retained.
        #[arg(long)]
        discard_unreadable_intent: bool,
    },
}
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground.
    Run,
    /// Start the daemon if it is not already running.
    Start,
    /// Stop the daemon without changing saved forward states.
    Stop,
    /// Restart the daemon, preserving configuration and forward running intent.
    Restart,
    /// Show whether the daemon is running.
    Status,
}
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install login startup for this user and hand the current daemon over to it.
    Install {
        /// Compatibility flag; user services are the default and only mode.
        #[arg(long)]
        user: bool,
    },
    /// Remove login startup and stop this profile's daemon and active forwards.
    Uninstall {
        /// Compatibility flag; user services are the default and only mode.
        #[arg(long)]
        user: bool,
    },
    /// Show both the saved definition and the system manager registration.
    Status,
}

fn duration(value: &str) -> Result<Duration, String> {
    let (number, factor) = if let Some(v) = value.strip_suffix("ms") {
        (v, 1)
    } else if let Some(v) = value.strip_suffix('s') {
        (v, 1000)
    } else if let Some(v) = value.strip_suffix('m') {
        (v, 60_000)
    } else {
        (value, 1000)
    };
    let milliseconds = number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|n| *n > 0)
        .ok_or("expected a positive duration such as 500ms, 20s, or 2m")?;
    Ok(Duration::from_millis(milliseconds))
}

#[cfg(test)]
#[path = "args_audit_tests.rs"]
mod audit_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_supports_partial_updates_but_add_still_requires_both_ports() {
        for flags in [
            vec!["--tgt", "8081"],
            vec!["--src", "12346"],
            vec!["--remote"],
            vec!["--local"],
        ] {
            let mut argv = vec!["fwm", "edit", "web"];
            argv.extend(flags);
            assert!(Cli::try_parse_from(argv).is_ok());
        }
        for flags in [vec!["--src", "12346"], vec!["--tgt", "8081"]] {
            let mut argv = vec!["fwm", "add", "--server", "dev", "--local"];
            argv.extend(flags);
            assert!(Cli::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn query_and_management_selectors_are_consistent() {
        for command in ["status", "logs", "up", "down", "retry", "restart", "remove"] {
            for selector in [
                vec!["web"],
                vec!["--server", "dev"],
                vec!["--group", "web"],
                vec!["--all"],
            ] {
                let mut argv = vec!["fwm", command];
                argv.extend(selector);
                assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
            }
            assert!(Cli::try_parse_from(["fwm", command, "web", "--server", "dev"]).is_err());
            assert!(Cli::try_parse_from(["fwm", command, "--group", "web", "--all"]).is_err());
        }
        assert!(Cli::try_parse_from(["fwm", "status"]).is_ok());
        let Command::Logs(logs) = Cli::try_parse_from(["fwm", "logs"]).unwrap().command else {
            unreachable!()
        };
        assert_eq!(logs.tail, 100);
        assert!(
            Cli::try_parse_from(["fwm", "logs", "--server", "dev", "--tail", "0", "--follow"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["fwm", "logs", "--tail", "bad"]).is_err());
        assert!(Cli::try_parse_from(["fwm", "restart", "--group", "web", "--wait"]).is_ok());
        assert!(Cli::try_parse_from(["fwm", "restart"]).is_err());
    }

    #[test]
    fn names_are_optional_and_direction_never_consumes_a_positional_name() {
        for arguments in [
            vec![
                "fwm", "add", "--server", "dev", "--remote", "--src", "12222", "--tgt", "22",
            ],
            vec![
                "fwm",
                "add",
                "--server",
                "dev",
                "--remote",
                "cluster-ssh-mac",
                "--src",
                "12222",
                "--tgt",
                "22",
            ],
            vec![
                "fwm",
                "add",
                "--server",
                "dev",
                "--src",
                "12222",
                "--tgt",
                "22",
                "--remote",
                "cluster-ssh-mac",
            ],
            vec![
                "fwm",
                "add",
                "cluster-ssh-mac",
                "--remote",
                "--server",
                "dev",
                "--src",
                "12222",
                "--tgt",
                "22",
            ],
            vec![
                "fwm",
                "add",
                "--remote",
                "--name",
                "cluster-ssh-mac",
                "--server",
                "dev",
                "--src",
                "12222",
                "--tgt",
                "22",
            ],
            vec![
                "fwm", "add", "--server", "dev", "--remote", "1234", "--port", "7890",
            ],
        ] {
            let Command::Add(args) = Cli::try_parse_from(&arguments).unwrap().command else {
                unreachable!()
            };
            assert_eq!(args.remote.as_deref(), Some(""), "{arguments:?}");
            if arguments.contains(&"cluster-ssh-mac") {
                assert_eq!(
                    args.explicit_name.as_deref().or(args.name.as_deref()),
                    Some("cluster-ssh-mac")
                );
            } else if arguments.contains(&"1234") {
                assert_eq!(args.name.as_deref(), Some("1234"));
            } else {
                assert!(args.name.is_none());
            }
        }
        assert!(
            Cli::try_parse_from([
                "fwm",
                "add",
                "positional",
                "--name",
                "explicit",
                "--server",
                "dev",
                "--local",
                "--port",
                "3000"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["fwm", "add", "--remote", "--port", "7890"]).is_err());
    }

    #[test]
    fn legacy_and_explicit_specs_use_the_same_input_path() {
        for specification in [
            vec!["--local", "3000:localhost:8080"],
            vec!["-L", "3000:localhost:8080"],
            vec!["-L3000:localhost:8080"],
            vec!["--local=3000:localhost:8080"],
            vec!["-L=3000:localhost:8080"],
        ] {
            let mut arguments = vec![
                "fwm",
                "--config-dir",
                "separate-config",
                "add",
                "--server",
                "dev",
            ];
            arguments.extend(specification);
            arguments.push("web");
            let Command::Add(args) = Cli::try_parse_from(&arguments).unwrap().command else {
                unreachable!()
            };
            assert_eq!(args.name.as_deref(), Some("web"));
            assert_eq!(args.local.as_deref(), Some("3000:localhost:8080"));
        }
        let Command::Edit(args) = Cli::try_parse_from([
            "fwm", "edit", "--remote", "web", "--src", "12222", "--tgt", "22",
        ])
        .unwrap()
        .command
        else {
            unreachable!()
        };
        assert_eq!(args.name, "web");
        assert_eq!(args.remote.as_deref(), Some(""));
    }

    #[test]
    fn legacy_remote_ranges_keep_whitespace_and_attached_short_values() {
        for specification in [
            vec!["--remote", "3000-3002, 8080"],
            vec!["-R3000-3002, 8080"],
            vec!["--remote=3000-3002, 8080"],
        ] {
            let mut arguments = vec!["fwm", "add", "--server", "dev"];
            arguments.extend(specification);
            let Command::Add(args) = Cli::try_parse_from(arguments).unwrap().command else {
                unreachable!()
            };
            let tunnels = super::super::parse::tunnels(
                None,
                args.remote.as_deref(),
                None,
                None,
                &args.ports,
                None,
            )
            .unwrap();
            assert_eq!(
                tunnels
                    .iter()
                    .map(|tunnel| tunnel.listen().port())
                    .collect::<Vec<_>>(),
                [3000, 3001, 3002, 8080]
            );
        }
    }

    #[test]
    fn accepts_shorthand_with_a_direction_and_edit_direction_inheritance() {
        for arguments in [
            vec![
                "fwm",
                "add",
                "web",
                "--server",
                "dev",
                "--local",
                "--port",
                "3000-3003,8080",
            ],
            vec![
                "fwm",
                "add",
                "web",
                "--server",
                "dev",
                "--remote",
                "--src",
                "5000-5003",
                "--tgt",
                "3000",
            ],
            vec!["fwm", "add", "web", "--server", "dev", "-L", "3000"],
            vec!["fwm", "edit", "web", "--port", "3000"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_ok(), "{arguments:?}");
        }
        let cli = Cli::try_parse_from([
            "fwm", "add", "web", "--server", "dev", "--local", "--port", "3000",
        ])
        .unwrap();
        let Command::Add(args) = cli.command else {
            panic!("expected add")
        };
        assert_eq!(args.local.as_deref(), Some(""));
        assert_eq!(args.ports.port.as_deref(), Some("3000"));
    }

    #[test]
    fn rejects_missing_direction_or_target_and_conflicting_shorthand_flags() {
        for flags in [
            vec!["--port", "3000"],
            vec!["--local", "--src", "3000"],
            vec!["--remote", "--tgt", "3000"],
            vec![
                "--local", "--port", "3000", "--src", "3001", "--tgt", "3002",
            ],
            vec!["--dynamic", "1080", "--port", "3000"],
            vec!["--dynamic", "1080", "--src", "3000", "--tgt", "3001"],
            vec!["--local", "--remote", "--port", "3000"],
        ] {
            let mut arguments = vec!["fwm", "add", "web", "--server", "dev"];
            arguments.extend(flags);
            assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn remote_dynamic_accepts_listen_specs_and_rejects_conflicting_tunnel_options() {
        for command in ["add", "edit"] {
            let mut base = vec!["fwm", command, "socks"];
            if command == "add" {
                base.extend(["--server", "dev"]);
            }
            for spec in ["7897", "127.0.0.2:7897", "[::1]:7897"] {
                let mut arguments = base.clone();
                arguments.extend(["--remote-dynamic", spec]);
                assert!(Cli::try_parse_from(arguments).is_ok());
            }
            for conflict in [
                vec!["--local=8080"],
                vec!["--remote=8080"],
                vec!["--dynamic", "1080"],
                vec!["--port", "8080"],
                vec!["--src", "8080"],
                vec!["--tgt", "8080"],
            ] {
                let mut arguments = base.clone();
                arguments.extend(["--remote-dynamic", "7897"]);
                arguments.extend(conflict);
                assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
            }
        }
    }
    #[test]
    fn requires_exactly_one_direction_and_selection() {
        assert!(
            Cli::try_parse_from([
                "fwm",
                "add",
                "web",
                "--server",
                "dev",
                "-L",
                "3000:localhost:3000"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "fwm",
                "add",
                "web",
                "--server",
                "dev",
                "-L",
                "3000:localhost:3000",
                "-D",
                "1080"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["fwm", "down"]).is_err());
        assert!(Cli::try_parse_from(["fwm", "down", "web", "--all"]).is_err());
        assert!(
            Cli::try_parse_from([
                "fwm",
                "up",
                "--server",
                "dev",
                "--wait",
                "--timeout",
                "2m",
                "--json"
            ])
            .is_ok()
        );
    }
    #[test]
    fn validates_durations_and_user_service_defaults() {
        assert_eq!(duration("20s").unwrap(), Duration::from_secs(20));
        assert!(duration("0s").is_err());
        assert!(duration("18446744073709551615m").is_err());
        assert!(Cli::try_parse_from(["fwm", "service", "install"]).is_ok());
        assert!(Cli::try_parse_from(["fwm", "service", "install", "--user"]).is_ok());
        assert!(Cli::try_parse_from(["fwm", "service", "uninstall"]).is_ok());
        assert!(Cli::try_parse_from(["fwm", "service", "status"]).is_ok());
    }
}
