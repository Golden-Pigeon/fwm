//! Normalize legacy forwarding values before clap resolves positional names.
use std::ffi::OsString;

pub(super) fn normalize<I, T>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let Some(start) = forward_command_start(&arguments) else {
        return arguments;
    };
    let end = arguments[start..]
        .iter()
        .position(|argument| argument == "--")
        .map_or(arguments.len(), |offset| start + offset);
    // With explicit ports, every positional token is a name, even a numeric one.
    let explicit_ports = arguments[start..end].iter().any(|argument| {
        argument.to_str().is_some_and(|argument| {
            ["--port", "--src", "--tgt"]
                .iter()
                .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
        })
    });
    let mut normalized = arguments[..start].to_vec();
    let editing = arguments[start - 1] == "edit";
    let mut edit_name_seen = false;
    let mut index = start;
    while index < end {
        let argument = &arguments[index];
        let flag = argument.to_str().unwrap_or("");
        if editing && !flag.starts_with('-') {
            edit_name_seen = true;
        }
        if ["--local", "--remote", "-L", "-R"].contains(&flag)
            && !explicit_ports
            && let Some(value) = arguments.get(index + 1).and_then(|value| value.to_str())
            && index + 1 < end
            && looks_like_spec(value)
            // In edit, a numeric token can be the required rule name. A full
            // colon mapping remains unambiguous; scalar values use =/attached
            // spelling or --port, in any option order.
            && (!editing || value.contains(':') || edit_name_seen || later_edit_name(&arguments, index + 2, end))
        {
            normalized.push(format!("{flag}={value}").into());
            index += 2;
            continue;
        }
        // clap's require_equals also applies to short options. Preserve the
        // OpenSSH-style attached spelling (-L3000:host:3000).
        if let Some(value) = flag.strip_prefix("-L").or_else(|| flag.strip_prefix("-R"))
            && !value.is_empty()
            && !value.starts_with('=')
            && looks_like_spec(value)
        {
            normalized.push(format!("{}={value}", &flag[..2]).into());
        } else {
            normalized.push(argument.clone());
        }
        index += 1;
        // A value which happens to look like a flag belongs to this option.
        if takes_value(flag) && index < end {
            normalized.push(arguments[index].clone());
            index += 1;
        }
    }
    normalized.extend_from_slice(&arguments[end..]);
    normalized
}

/// A separated scalar SPEC is still safe when an independent required NAME
/// follows it. Skip option values so `--rename 456` cannot steal numeric NAME.
fn later_edit_name(arguments: &[OsString], mut index: usize, end: usize) -> bool {
    while index < end {
        let argument = arguments[index].to_str().unwrap_or("");
        if !argument.starts_with('-') {
            return true;
        }
        index += if takes_value(argument) { 2 } else { 1 };
    }
    end < arguments.len() && end + 1 < arguments.len()
}

fn forward_command_start(arguments: &[OsString]) -> Option<usize> {
    let mut index = 1;
    while let Some(argument) = arguments.get(index).and_then(|value| value.to_str()) {
        match argument {
            "add" | "edit" => return Some(index + 1),
            "--config-dir" => index += 2,
            "--json" => index += 1,
            value if value.starts_with("--config-dir=") => index += 1,
            _ => return None,
        }
    }
    None
}

fn takes_value(argument: &str) -> bool {
    [
        "--server",
        "--name",
        "--group",
        "--ssh-config",
        "--config-dir",
        "--port",
        "--src",
        "--tgt",
        "--timeout",
        "--connection-mode",
        "--remote-cleanup",
        "--rename",
        "--dynamic",
        "--remote-dynamic",
        "-D",
    ]
    .contains(&argument)
}

fn looks_like_spec(value: &str) -> bool {
    !value.starts_with('-')
        && (value.contains(':')
            || (!value.is_empty()
                && value.bytes().all(|byte| {
                    byte.is_ascii_digit() || byte.is_ascii_whitespace() || b",-".contains(&byte)
                })))
}
