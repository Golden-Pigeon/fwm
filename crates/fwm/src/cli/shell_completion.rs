//! The upstream completion engine owns parsing, paths and shell quoting.
//! fwm only provides the saved resource candidates for each selector argument.
use super::args::{Cli, CompletionShell};
use anyhow::Result;
use clap::{Command, CommandFactory, ValueHint};
use clap_complete::{
    CompleteEnv,
    engine::{ArgValueCandidates, CompletionCandidate},
    env::{Bash, EnvCompleter, Shells, Zsh},
};
use fwm_core::{model::Config, paths::Paths, store::Store};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
};

const COMPLETION_ENV: &str = "FWM_COMPLETE";

/// Called while main is still single-threaded, before parsing or Tokio startup.
pub(crate) fn complete() {
    let args: Vec<_> = std::env::args_os().collect();
    CompleteEnv::with_factory(|| {
        let words = args
            .iter()
            .position(|arg| arg == "--")
            .map_or(&[][..], |index| &args[index + 1..]);
        let config = saved_config(words);
        add_candidates(Cli::command(), &[], &config)
    })
    .var(COMPLETION_ENV)
    .shells(Shells(&[&Bash, &Zsh]))
    .bin("fwm")
    .completer("fwm")
    .complete();
}

pub(super) fn script(shell: CompletionShell) -> Result<()> {
    let adapter: &dyn EnvCompleter = match shell {
        CompletionShell::Bash => &Bash,
        CompletionShell::Zsh => &Zsh,
    };
    let mut output = io::stdout().lock();
    adapter.write_registration(COMPLETION_ENV, "fwm", "fwm", "fwm", &mut output)?;
    if matches!(shell, CompletionShell::Bash) {
        // Ask Readline to quote paths and IDs. The generated completer remains
        // upstream code; no application code parses or escapes shell input.
        writeln!(
            output,
            "compopt -o fullquote fwm 2>/dev/null || compopt -o filenames fwm 2>/dev/null || complete -o filenames -o nospace -o bashdefault -F _clap_complete_fwm fwm"
        )?;
    }
    Ok(())
}

fn saved_config(words: &[OsString]) -> Config {
    if words.is_empty() {
        return Config::default();
    }
    // Clap accepts incomplete input here; it remains the authority on global
    // options, equals syntax and which tokens are option values.
    let directory = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(super::input::normalize(words.iter().cloned()))
        .ok()
        .and_then(|matches| matches.get_one::<PathBuf>("config_dir").cloned())
        .map(|path| {
            // Decode quoting retained by Bash/Zsh using shlex. Keep existing
            // literal paths (and native Windows paths) intact; never evaluate
            // shell substitutions or rewrite the words passed to the engine.
            if path.exists() || (cfg!(windows) && path.is_absolute()) {
                return path;
            }
            path.to_str()
                .and_then(shlex::split)
                .filter(|parts| parts.len() == 1)
                .and_then(|parts| parts.into_iter().next())
                .map(PathBuf::from)
                .unwrap_or(path)
        });
    Paths::new(directory)
        .ok()
        .and_then(|paths| Store::new(paths).load().ok())
        .map(|loaded| loaded.config)
        .unwrap_or_default()
}

fn add_candidates(mut command: Command, parent: &[&str], config: &Config) -> Command {
    let name = command.get_name().to_owned();
    let mut route = parent.to_vec();
    route.push(&name);
    command = command.mut_args(|arg| {
        if !arg.get_action().takes_values() {
            return arg;
        }
        let id = arg.get_id().as_str();
        let values: Option<Vec<String>> = if id == "server"
            || (id == "name"
                && matches!(
                    route.as_slice(),
                    ["fwm", "server", "edit" | "remove" | "check" | "trust"]
                )) {
            Some(
                config
                    .servers
                    .iter()
                    .flat_map(|server| [server.name.clone(), server.id.clone()])
                    .collect(),
            )
        } else if id == "name"
            && matches!(
                route.as_slice(),
                [
                    "fwm",
                    "edit" | "up" | "down" | "retry" | "restart" | "remove" | "status" | "logs"
                ]
            )
        {
            Some(
                config
                    .forwards
                    .iter()
                    .flat_map(|rule| {
                        [
                            Some(rule.name.clone()),
                            Some(rule.id.clone()),
                            rule.group.clone(),
                        ]
                    })
                    .flatten()
                    .collect(),
            )
        } else if id == "group" {
            Some(
                config
                    .forwards
                    .iter()
                    .filter_map(|rule| rule.group.clone())
                    .collect(),
            )
        } else {
            None
        };
        if let Some(values) = values {
            let values: Vec<_> = values
                .into_iter()
                .filter(|value| !value.chars().any(char::is_control))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            return arg
                .value_hint(ValueHint::Other)
                .add(ArgValueCandidates::new(move || {
                    values
                        .iter()
                        .map(CompletionCandidate::new)
                        .collect::<Vec<_>>()
                }));
        }
        let hint = match id {
            "config_dir" => ValueHint::DirPath,
            "ssh_config" | "identity_files" | "known_hosts" => ValueHint::AnyPath,
            _ => ValueHint::Other,
        };
        arg.value_hint(hint)
    });
    for child in command.get_subcommands_mut() {
        *child = add_candidates(child.clone(), &route, config);
    }
    command
}
