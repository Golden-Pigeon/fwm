//! Select an existing server or prepare an SSH alias for the same atomic add.
use anyhow::{Result, bail};
use fwm_core::model::{Config, ServerProfile, validate_name};
use std::path::Path;

pub(super) struct SelectedServer {
    pub profile: ServerProfile,
    pub is_new: bool,
}

pub(super) fn select(
    config: &Config,
    selector: &str,
    ssh_config: Option<&Path>,
) -> Result<SelectedServer> {
    let base = std::env::current_dir()?;
    let existing = config.server(selector);
    if let Some(server) = existing {
        if let Some(path) = ssh_config {
            let existing = server
                .ssh_config
                .clone()
                .unwrap_or_else(|| "~/.ssh/config".into());
            if !fwm_core::ssh::equivalent_paths(&existing, path, &base)? {
                bail!(
                    "server {:?} is already configured with a different SSH config; --ssh-config only configures new aliases (use `fwm server edit NAME --ssh-config PATH` to change it)",
                    server.name,
                );
            }
        }
        return Ok(SelectedServer {
            profile: server.clone(),
            is_new: false,
        });
    }
    validate_name(selector).map_err(anyhow::Error::msg)?;
    let mut profile = ServerProfile::new(selector);
    profile.ssh_alias = Some(selector.into());
    profile.ssh_config = ssh_config.map(Path::to_path_buf);
    fwm_core::ssh::normalize_profile_paths(&mut profile, &base)?;
    Ok(SelectedServer {
        profile,
        is_new: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str) -> ServerProfile {
        let mut profile = ServerProfile::new(name);
        profile.host = Some("example.test".into());
        profile
    }

    #[test]
    fn creates_alias_without_resolving_or_mutating_config() {
        let config = Config::default();
        let selected = select(&config, "example-cluster", Some(Path::new("ssh.conf"))).unwrap();
        assert!(selected.is_new);
        assert_eq!(selected.profile.name, "example-cluster");
        assert_eq!(
            selected.profile.ssh_alias.as_deref(),
            Some("example-cluster")
        );
        assert_eq!(
            selected.profile.ssh_config.as_deref(),
            Some(
                fwm_core::ssh::normalized_path(
                    Path::new("ssh.conf"),
                    &std::env::current_dir().unwrap()
                )
                .unwrap()
                .as_path()
            )
        );
        assert!(config.servers.is_empty());
    }

    #[test]
    fn selects_by_explicit_name_or_id_and_never_overwrites_an_existing_profile() {
        let mut profile = server("dev");
        profile.ssh_config = Some("existing.conf".into());
        let config = Config {
            servers: vec![profile.clone()],
            ..Default::default()
        };
        for selector in ["dev", profile.id.as_str()] {
            let selected = select(&config, selector, None).unwrap();
            assert!(!selected.is_new);
            assert_eq!(selected.profile, profile);
        }
        assert!(select(&config, "dev", Some(Path::new("new.conf"))).is_err());
        assert!(select(&config, "dev", Some(Path::new("existing.conf"))).is_ok());
        assert_eq!(config.servers[0], profile);
    }

    #[test]
    fn validates_new_alias_names() {
        assert!(select(&Config::default(), "bad/name", None).is_err());
    }
}
