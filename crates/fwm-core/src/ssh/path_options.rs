//! Stable path bases for saved profiles while retaining SSH home/token syntax.
use std::path::{Path, PathBuf};

use super::SshError;
use crate::model::{Config, ServerProfile};

pub fn normalize_profile_paths(profile: &mut ServerProfile, base: &Path) -> Result<(), SshError> {
    for path in profile
        .identity_files
        .iter_mut()
        .chain(profile.ssh_config.iter_mut())
        .chain(profile.known_hosts.iter_mut())
    {
        *path = normalized_path(path, base)?;
    }
    Ok(())
}

pub fn normalize_config_paths(config: &mut Config, base: &Path) -> Result<(), SshError> {
    for profile in &mut config.servers {
        for path in profile
            .identity_files
            .iter_mut()
            .chain(profile.ssh_config.iter_mut())
            .chain(profile.known_hosts.iter_mut())
        {
            // Persisted absolute paths already have a stable base. Loading,
            // stopping and repairing config must not require their external
            // directories to remain accessible. New CLI inputs still use
            // normalize_profile_paths to establish a canonical path once.
            if !path.is_absolute() {
                *path = normalized_path(path, base)?;
            }
        }
    }
    Ok(())
}

pub fn normalized_path(path: &Path, base: &Path) -> Result<PathBuf, SshError> {
    let text = path.to_string_lossy();
    if text.is_empty() {
        return Err(SshError::Configuration("SSH path must not be empty".into()));
    }
    // Home is a runtime user property. %d is the equivalent SSH home token;
    // %h/%n/%r/%p are expanded only once the server is resolved.
    if text.starts_with('~') || text.starts_with("%d/") || text == "%d" {
        return Ok(path.to_path_buf());
    }
    if text.contains('%') {
        // Expanded tokens may be symlinks or several path components. Resolve
        // the fixed base now, leaving the template's traversal to runtime.
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            crate::paths::absolute_normalized(base, &std::env::current_dir()?)?.join(path)
        });
    }
    // Canonicalize the containing directory, but retain the final filename:
    // SSH configs and identity files are often symlinks intentionally switched
    // by the user. Persisting their current target would freeze that choice.
    if let Some(name) = path.file_name() {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        return match crate::paths::absolute_normalized(parent, base) {
            Ok(parent) => Ok(parent.join(name)),
            // Editing other fields on an existing profile must remain possible
            // while an already-absolute external path is inaccessible.
            Err(_) if path.is_absolute() => Ok(path.to_path_buf()),
            Err(error) => Err(error.into()),
        };
    }
    Ok(crate::paths::absolute_normalized(path, base)?)
}

pub fn equivalent_paths(a: &Path, b: &Path, base: &Path) -> Result<bool, SshError> {
    let expanded = |path: &Path| -> Result<PathBuf, SshError> {
        let text = path.to_string_lossy();
        let path = if text == "~" || text.starts_with("~/") {
            let home = directories::BaseDirs::new()
                .ok_or_else(|| SshError::Configuration("cannot determine home directory".into()))?;
            home.home_dir().join(text.strip_prefix("~/").unwrap_or(""))
        } else {
            path.to_path_buf()
        };
        Ok(crate::paths::absolute_normalized(&path, base)?)
    };
    Ok(expanded(a)? == expanded(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn saved_file_symlinks_follow_later_target_switches() {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let first = base.join("first.conf");
        let second = base.join("second.conf");
        std::fs::write(&first, "Host dev\n HostName 127.0.0.1\n Port 10001\n").unwrap();
        std::fs::write(&second, "Host dev\n HostName 127.0.0.1\n Port 10002\n").unwrap();
        let link = base.join("current.conf");
        std::os::unix::fs::symlink(&first, &link).unwrap();
        let mut profile = ServerProfile::new("dev");
        profile.ssh_alias = Some("dev".into());
        profile.ssh_config = Some("current.conf".into());
        normalize_profile_paths(&mut profile, &base).unwrap();
        assert_eq!(profile.ssh_config.as_ref().unwrap(), &link);
        assert_eq!(super::super::resolve(&profile).unwrap().port, 10001);
        assert!(equivalent_paths(&link, &first, &base).unwrap());
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        assert_eq!(super::super::resolve(&profile).unwrap().port, 10002);
        assert!(!equivalent_paths(&link, &first, &base).unwrap());
    }

    #[test]
    fn ordinary_relative_paths_are_stable_and_token_paths_keep_their_meaning() {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let mut profile = ServerProfile::new("test");
        profile.ssh_config = Some("./ssh.conf".into());
        profile.known_hosts = Some("%d/.ssh/known_hosts".into());
        profile.identity_files = vec!["~/keys/%h".into(), "keys/%r-%p".into()];
        normalize_profile_paths(&mut profile, directory.path()).unwrap();
        assert_eq!(profile.ssh_config.unwrap(), base.join("ssh.conf"));
        assert_eq!(profile.identity_files[0], Path::new("~/keys/%h"));
        assert_eq!(profile.identity_files[1], base.join("keys/%r-%p"));
        assert_eq!(
            normalized_path(Path::new("keys/%h/../id"), directory.path()).unwrap(),
            base.join("keys/%h/../id")
        );
        assert_eq!(
            profile.known_hosts.unwrap(),
            Path::new("%d/.ssh/known_hosts")
        );
        assert!(
            equivalent_paths(
                Path::new("./ssh.conf"),
                &base.join("ssh.conf"),
                directory.path()
            )
            .unwrap()
        );
    }
}
