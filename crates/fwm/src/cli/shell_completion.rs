//! Shell-independent completion using clap's command tree and saved state only.
use super::args::{Cli, CompletionShell};
use anyhow::Result;
use clap::{Arg, Command, CommandFactory};
use fwm_core::{model::Config, paths::Paths, store::Store};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

pub(super) fn script(shell: CompletionShell) -> Result<()> {
    let text = match shell {
        CompletionShell::Bash => include_str!("../../../../completions/fwm.bash"),
        CompletionShell::Zsh => include_str!("../../../../completions/_fwm"),
    };
    io::stdout().lock().write_all(text.as_bytes())?;
    Ok(())
}

pub(super) fn write_candidates(words: &[OsString], cursor: usize) -> Result<()> {
    let mut output = io::stdout().lock();
    for candidate in candidates(words, cursor) {
        writeln!(output, "{candidate}")?;
    }
    Ok(())
}

fn candidates(words: &[OsString], cursor: usize) -> Vec<String> {
    if cursor == 0 || cursor > words.len() {
        return vec![];
    }
    let Some(prefix) = words.get(cursor).map_or(Some(""), |word| word.to_str()) else {
        return vec![];
    };
    let mut root = Cli::command();
    root.build();
    let mut command = &root;
    let mut route = Vec::new();
    let mut positional = 1;
    let mut pending: Option<&Arg> = None;
    let mut options = true;
    let mut used = BTreeSet::new();
    let completed = super::input::normalize(words.iter().take(cursor).cloned());
    for word in completed.iter().skip(1) {
        if pending.take().is_some() {
            continue;
        }
        let Some(word) = word.to_str() else {
            return vec![];
        };
        if options && word == "--" {
            options = false;
        } else if options && word.starts_with('-') && word != "-" {
            let Some((arg, attached)) = option(command, word) else {
                return vec![];
            };
            used.insert(arg.get_id().as_str().to_owned());
            if arg.get_action().takes_values() && !attached && !arg.is_require_equals_set() {
                pending = Some(arg);
            }
        } else if options && let Some(next) = command.find_subcommand(word) {
            command = next;
            route.push(command.get_name());
            positional = 1;
            used.clear();
        } else {
            // A positional is consumed even when the supplied name is unknown;
            // completion does not execute or validate the partial command.
            positional += 1;
        }
    }
    let config_dir = selected_directory(words, cursor);
    if let Some(arg) = pending {
        return values(arg, &route, prefix, config_dir.as_deref());
    }
    if options
        && prefix.starts_with("--")
        && let Some((flag, value)) = prefix.split_once('=')
    {
        let Some((arg, _)) = option(command, flag) else {
            return vec![];
        };
        return values(arg, &route, value, config_dir.as_deref())
            .into_iter()
            .map(|value| format!("{flag}={value}"))
            .collect();
    }
    let mut matches = Vec::new();
    if options && (prefix.is_empty() || prefix.starts_with('-')) {
        for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
            if used.contains(arg.get_id().as_str())
                && !matches!(arg.get_action(), clap::ArgAction::Append)
            {
                continue;
            }
            if let Some(longs) = arg.get_long_and_visible_aliases() {
                matches.extend(longs.into_iter().map(|name| format!("--{name}")));
            }
            if let Some(shorts) = arg.get_short_and_visible_aliases() {
                matches.extend(shorts.into_iter().map(|name| format!("-{name}")));
            }
        }
    }
    if !prefix.starts_with('-') || !options {
        if options && positional == 1 {
            for child in command
                .get_subcommands()
                .filter(|child| !child.is_hide_set())
            {
                matches.push(child.get_name().to_owned());
                matches.extend(child.get_visible_aliases().map(str::to_owned));
            }
        }
        if let Some(arg) = command
            .get_positionals()
            .find(|arg| arg.get_index() == Some(positional))
        {
            matches.extend(values(arg, &route, prefix, config_dir.as_deref()));
        }
    }
    retain_prefix(matches, prefix)
}

fn option<'a>(command: &'a Command, word: &str) -> Option<(&'a Arg, bool)> {
    if let Some(long) = word.strip_prefix("--") {
        let (name, value) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        command
            .get_arguments()
            .find(|arg| {
                arg.get_long_and_visible_aliases()
                    .is_some_and(|names| names.contains(&name))
            })
            .map(|arg| (arg, value))
    } else {
        let mut short = word.strip_prefix('-')?.chars();
        let name = short.next()?;
        command
            .get_arguments()
            .find(|arg| {
                arg.get_short_and_visible_aliases()
                    .is_some_and(|names| names.contains(&name))
            })
            .map(|arg| (arg, short.next().is_some()))
    }
}

fn selected_directory(words: &[OsString], cursor: usize) -> Option<PathBuf> {
    let mut result = None;
    let mut index = 1;
    while index < words.len() {
        let word = &words[index];
        if word == "--" {
            break;
        }
        if word == "--config-dir" {
            if index + 1 != cursor
                && let Some(path) = words.get(index + 1).filter(|path| !path.is_empty())
            {
                result = Some(path.into());
            }
            index += 2;
            continue;
        }
        if index != cursor
            && let Some(value) = word
                .to_str()
                .and_then(|word| word.strip_prefix("--config-dir="))
            && !value.is_empty()
        {
            result = Some(value.into());
        }
        index += 1;
    }
    result
}

fn values(arg: &Arg, route: &[&str], prefix: &str, directory: Option<&Path>) -> Vec<String> {
    let id = arg.get_id().as_str();
    let possible = arg.get_possible_values();
    if !possible.is_empty() {
        let (head, prefix) = if arg.get_value_delimiter() == Some(',') {
            prefix
                .rsplit_once(',')
                .map_or(("", prefix), |(head, tail)| {
                    (&prefix[..head.len() + 1], tail)
                })
        } else {
            ("", prefix)
        };
        return retain_prefix(
            possible
                .into_iter()
                .filter(|value| !value.is_hide_set())
                .map(|value| value.get_name().to_owned())
                .collect(),
            prefix,
        )
        .into_iter()
        .map(|value| format!("{head}{value}"))
        .collect();
    }
    if ["config_dir", "ssh_config", "identity_files", "known_hosts"].contains(&id) {
        return paths(prefix, id == "config_dir");
    }
    let servers = id == "server"
        || (id == "name" && matches!(route, ["server", "edit" | "remove" | "check" | "trust"]));
    let rules = id == "name"
        && matches!(
            route,
            ["edit" | "up" | "down" | "retry" | "restart" | "remove" | "status" | "logs"]
        );
    let groups = id == "group";
    if !servers && !rules && !groups {
        return vec![];
    }
    let Some(config) = saved_config(directory) else {
        return vec![];
    };
    let mut matches = Vec::new();
    if servers {
        for server in config.servers {
            matches.push(server.name);
            matches.push(server.id);
        }
    } else {
        for rule in config.forwards {
            if rules {
                matches.push(rule.name);
                matches.push(rule.id);
            }
            if let Some(group) = rule.group {
                matches.push(group);
            }
        }
    }
    retain_prefix(matches, prefix)
}

fn saved_config(directory: Option<&Path>) -> Option<Config> {
    let paths = Paths::new(directory.map(Path::to_path_buf)).ok()?;
    Store::new(paths).load().ok().map(|loaded| loaded.config)
}

fn retain_prefix(values: Vec<String>, prefix: &str) -> Vec<String> {
    values
        .into_iter()
        .filter(|value| value.starts_with(prefix) && !value.chars().any(char::is_control))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn paths(prefix: &str, directories_only: bool) -> Vec<String> {
    let (head, tail) = prefix
        .rsplit_once(std::path::is_separator)
        .map_or(("", prefix), |(head, tail)| {
            (&prefix[..head.len() + 1], tail)
        });
    let directory = if let Some(path) = head.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(path))
    } else {
        Some(PathBuf::from(if head.is_empty() { "." } else { head }))
    };
    let Some(entries) = directory.and_then(|path| std::fs::read_dir(path).ok()) else {
        return vec![];
    };
    let matches = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(tail) || (name.starts_with('.') && !tail.starts_with('.')) {
                return None;
            }
            let directory = entry.path().is_dir();
            if directories_only && !directory {
                return None;
            }
            Some(format!("{head}{name}{}", if directory { "/" } else { "" }))
        })
        .collect();
    retain_prefix(matches, prefix)
}
