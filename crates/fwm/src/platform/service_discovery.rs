//! Recognize definitions created by FWM without executing their command text.
use super::{ServiceError, identifier_path};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use anyhow::Result;
use fwm_core::paths::{Paths, absolute_normalized};
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(super) fn query(
    executor: &mut dyn super::CommandExecutor,
    command: &mut std::process::Command,
    absent: &[&str],
) -> Result<Option<super::CommandResult>> {
    match executor.execute(command) {
        Ok(result) if result.success => Ok(Some(result)),
        // In a macOS sandbox without an accessible launchd domain, `list` can
        // return exactly exit 1 with no diagnostic output. This is absence of
        // the discovery surface, not evidence that an owned service exists.
        Ok(result)
            if command.get_program() == std::ffi::OsStr::new("launchctl")
                && command.get_args().next() == Some(std::ffi::OsStr::new("list"))
                && result.code == Some(1)
                && result.stdout.trim().is_empty()
                && result.stderr.trim().is_empty() =>
        {
            Ok(None)
        }
        Ok(result)
            if absent
                .iter()
                .any(|reason| result.stderr.contains(reason) || result.stdout.contains(reason)) =>
        {
            Ok(None)
        }
        Ok(result) => Err(ServiceError::new(
            "service_query_failed",
            format!(
                "could not discover existing login services: {} ({command:?})",
                result.details
            ),
        )
        .into()),
        Err(error) if super::manager_unavailable(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn same_profile(candidate: &Path, paths: &Paths) -> bool {
    candidate.is_absolute()
        && absolute_normalized(candidate, &paths.config_dir)
            .is_ok_and(|path| path == paths.config_dir)
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(super) fn identity_conflict(name: &str) -> anyhow::Error {
    ServiceError::new("service_identity_conflict", format!("service {name:?} does not identify this configuration; refusing to operate on another or malformed service")).into()
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(super) fn validate_profile_file(
    path: &Path,
    paths: &Paths,
    extract: fn(&str) -> Option<PathBuf>,
) -> Result<()> {
    if let Some(bytes) = super::definition::read_optional(path)?
        && !std::str::from_utf8(&bytes)
            .ok()
            .and_then(extract)
            .is_some_and(|profile| same_profile(&profile, paths))
    {
        return Err(identity_conflict(&path.to_string_lossy()));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(super) fn file_candidates(
    directory: &Path,
    prefix: &str,
    extension: &str,
    paths: &Paths,
    extract: fn(&str) -> Option<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut found = vec![];
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(prefix) || !name.ends_with(extension) {
            continue;
        }
        if !entry.file_type()?.is_file() || entry.metadata()?.len() > 256 * 1024 {
            continue;
        }
        if let Ok(text) = fs::read_to_string(entry.path())
            && extract(&text).is_some_and(|path| same_profile(&path, paths))
        {
            found.push(entry.path());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

#[cfg(any(target_os = "macos", windows, test))]
pub(super) fn xml_unescape(text: &str) -> Option<String> {
    let mut result = String::new();
    let mut remainder = text;
    while let Some(index) = remainder.find('&') {
        result.push_str(&remainder[..index]);
        remainder = &remainder[index + 1..];
        let end = remainder.find(';')?;
        let entity = &remainder[..end];
        match entity {
            "amp" => result.push('&'),
            "lt" => result.push('<'),
            "gt" => result.push('>'),
            "quot" => result.push('"'),
            "apos" => result.push('\''),
            _ => {
                let value = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    entity.strip_prefix('#')?.parse().ok()?
                };
                result.push(char::from_u32(value)?);
            }
        }
        remainder = &remainder[end + 1..];
    }
    result.push_str(remainder);
    Some(result)
}

#[cfg(any(target_os = "macos", windows, test))]
pub(super) fn xml_value(text: &str, tag: &str) -> Option<String> {
    let start = format!("<{tag}>");
    let end = format!("</{tag}>");
    xml_unescape(text.split_once(&start)?.1.split_once(&end)?.0)
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn plist_profile(text: &str) -> Option<PathBuf> {
    let tail = text.split_once("<string>--config-dir</string>")?.1;
    Some(xml_value(tail, "string")?.into())
}

/// Decode one quoted argument, including Windows backslashes before quotes.
#[cfg(any(target_os = "linux", windows, test))]
pub(super) fn quoted(value: &str) -> Option<String> {
    let mut chars = value.trim_start().strip_prefix('"')?.chars();
    let mut output = String::new();
    let mut slashes = 0;
    for character in chars.by_ref() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        if character == '"' {
            output.extend(std::iter::repeat_n('\\', slashes / 2));
            if slashes % 2 == 0 {
                return Some(output);
            }
            output.push('"');
        } else {
            output.extend(std::iter::repeat_n('\\', slashes));
            output.push(character);
        }
        slashes = 0;
    }
    None
}

#[cfg(any(windows, test))]
pub(super) fn task_profile(text: &str) -> Option<PathBuf> {
    let arguments = xml_value(text, "Arguments")?;
    Some(quoted(arguments.strip_prefix("--config-dir ")?)?.into())
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn unit_profile(text: &str) -> Option<PathBuf> {
    let command = text
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart="))?;
    let argument = command.split_once(" --config-dir ")?.1;
    Some(
        quoted(argument)?
            .replace("%%", "%")
            .replace("$$", "$")
            .replace("\\\\", "\\")
            .into(),
    )
}

#[cfg(any(target_os = "macos", test))]
pub(super) fn launch_profile(text: &str) -> Option<PathBuf> {
    fn scalar(line: &str) -> &str {
        let line = line.trim().trim_end_matches(',');
        let line = line
            .split_once('=')
            .filter(|(key, _)| key.trim().chars().all(|c| c.is_ascii_digit()))
            .map_or(line, |(_, value)| value.trim());
        line.trim_matches('"')
    }
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if scalar(line) == "--config-dir" {
            return lines.next().map(scalar).map(PathBuf::from);
        }
    }
    None
}

pub(super) fn legacy_ids(paths: &Paths) -> Vec<String> {
    let mut ids = vec![identifier_path(&paths.config_dir)];
    ids.extend(
        paths
            .legacy_config_dirs
            .iter()
            .map(|path| identifier_path(path)),
    );
    ids.sort();
    ids.dedup();
    ids
}

pub(super) fn conflict(names: &[String]) -> anyhow::Error {
    ServiceError::new("service_legacy_conflict", format!("multiple login services refer to this configuration: {}. Run service uninstall to stop and remove all matching registrations, then service install to create one canonical service", names.join(", "))).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extracts_escaped_paths_without_running_command_text() {
        assert_eq!(
            plist_profile("<string>--config-dir</string><string>/a&amp;b/&quot;c</string>"),
            Some(PathBuf::from("/a&b/\"c"))
        );
        assert_eq!(
            task_profile(
                "<Arguments>--config-dir &quot;C:\\My Config\\\\&quot; daemon run</Arguments>"
            ),
            Some(PathBuf::from("C:\\My Config\\"))
        );
        assert_eq!(
            unit_profile("ExecStart=\"/bin/fwm\" --config-dir \"/a %% $$\" daemon run\n"),
            Some(PathBuf::from("/a % $"))
        );
        assert!(xml_unescape("bad &unknown;").is_none());
    }

    #[test]
    fn empty_launchctl_list_failure_is_unavailable_but_other_failures_remain_errors() {
        struct Failure(&'static str);
        impl super::super::CommandExecutor for Failure {
            fn execute(
                &mut self,
                _: &mut std::process::Command,
            ) -> Result<super::super::CommandResult> {
                Ok(super::super::CommandResult {
                    success: false,
                    code: Some(1),
                    stderr: self.0.into(),
                    ..Default::default()
                })
            }
        }
        assert!(
            query(
                &mut Failure(""),
                std::process::Command::new("launchctl").arg("list"),
                &[]
            )
            .unwrap()
            .is_none()
        );
        assert!(
            query(
                &mut Failure("permission denied"),
                std::process::Command::new("launchctl").arg("list"),
                &[]
            )
            .is_err()
        );
        assert!(
            query(
                &mut Failure(""),
                std::process::Command::new("launchctl").args(["print", "gui/501/service"]),
                &[]
            )
            .is_err()
        );
        assert!(
            query(
                &mut Failure(""),
                std::process::Command::new("systemctl").args(["--user", "list-units"]),
                &[]
            )
            .is_err()
        );
    }
}
