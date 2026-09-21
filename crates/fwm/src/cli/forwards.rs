use super::{
    args::{EditArgs, SelectionArgs, UpArgs},
    cleanup, completion, get_config, output, parse, server_selection,
};
use crate::{client, offline};
use anyhow::{Result, bail};
use fwm_api::protocol::{Command, OperationReport, Selection};
#[cfg(test)]
use fwm_core::model::Config;
use fwm_core::{
    model::{DesiredState, ForwardSpec},
    paths::Paths,
};

pub fn selection(args: SelectionArgs) -> Selection {
    if let Some(name) = args.name {
        Selection::Forward(name)
    } else if let Some(server) = args.server {
        Selection::Server(server)
    } else if let Some(group) = args.group {
        Selection::Group(group)
    } else {
        Selection::All
    }
}

pub async fn edit(paths: &Paths, args: EditArgs, json_output: bool) -> Result<()> {
    let config = get_config(paths, false).await?;
    let ids = config
        .select_forwards(&args.name)
        .map_err(anyhow::Error::msg)?;
    let selected_server = args
        .server
        .as_deref()
        .map(|server| server_selection::select(&config, server, args.ssh_config.as_deref()))
        .transpose()?;
    let group_edit = config.forward(&args.name).is_none();
    if group_edit && args.rename.is_some() && (args.group.is_some() || args.ungroup) {
        bail!(
            "renaming a group cannot be combined with --group or --ungroup; use one operation at a time"
        );
    }
    if group_edit
        && let Some(name) = &args.rename
        && name != &args.name
        && config
            .forwards
            .iter()
            .any(|rule| rule.group.as_ref() == Some(name))
    {
        bail!(
            "group {name:?} already exists; use edit GROUP --group {name} to explicitly move its members"
        );
    }
    let mut forwards = Vec::with_capacity(ids.len());
    for id in &ids {
        let existing = config.forward(id).expect("selected rule exists");
        let mut forward = edit_fields(existing, &args)?;
        if let Some(selected) = &selected_server {
            forward.server_id = selected.profile.id.clone();
        }
        if group_edit && let Some(name) = &args.rename {
            forward.name = existing.name.clone();
            forward.group = Some(name.clone());
        }
        forwards.push(forward);
    }
    let server = selected_server.and_then(|selected| selected.is_new.then_some(selected.profile));
    let mut candidate = config.clone();
    candidate.servers.extend(server.iter().cloned());
    for forward in &forwards {
        *candidate
            .forwards
            .iter_mut()
            .find(|rule| rule.id == forward.id)
            .expect("selected rule exists") = forward.clone();
    }
    candidate.validate().map_err(anyhow::Error::msg)?;
    let response = offline::mutate(
        paths,
        Command::PutForwardsWithServer { forwards, server },
        Some(config.revision),
    )
    .await?;
    let reply = completion::mutation(paths, response, &ids, None, json_output).await?;
    if !json_output {
        for id in ids {
            if let Some(forward) = reply.config.forward(&id) {
                cleanup::describe(forward);
            }
        }
    }
    Ok(())
}

fn edit_fields(existing: &ForwardSpec, args: &EditArgs) -> Result<ForwardSpec> {
    let mut forward = existing.clone();
    let tunnels = parse::tunnels(
        args.local.as_deref(),
        args.remote.as_deref(),
        args.dynamic.as_deref(),
        &args.ports,
        Some(&forward.tunnel),
    )?;
    if tunnels.len() > 1 {
        bail!("edit does not create new listeners; use add for new port ranges");
    }
    if let Some(tunnel) = tunnels.into_iter().next() {
        forward.tunnel = tunnel;
    }
    if let Some(name) = &args.rename {
        forward.name = name.clone();
    }
    if let Some(group) = &args.group {
        forward.group = Some(group.clone());
    } else if args.ungroup {
        forward.group = None;
    }
    (forward.connection_mode, forward.remote_cleanup) = cleanup::effective(
        &forward.tunnel,
        Some(existing),
        args.connection_mode,
        args.remote_cleanup,
    )?;
    Ok(forward)
}

#[cfg(test)]
fn edited(config: &Config, args: &EditArgs) -> Result<ForwardSpec> {
    let existing = config
        .forward(&args.name)
        .ok_or_else(|| anyhow::anyhow!("unknown forward {:?}", args.name))?;
    let mut forward = edit_fields(existing, args)?;
    if let Some(server) = &args.server {
        forward.server_id = server_selection::select(config, server, args.ssh_config.as_deref())?
            .profile
            .id;
    }
    Ok(forward)
}

pub async fn up(paths: &Paths, args: UpArgs, json_output: bool) -> Result<()> {
    activate(paths, args, json_output, false).await
}
pub async fn restart(paths: &Paths, args: UpArgs, json_output: bool) -> Result<()> {
    activate(paths, args, json_output, true).await
}

async fn activate(paths: &Paths, args: UpArgs, json_output: bool, restart: bool) -> Result<()> {
    let selection = selection(args.selection);
    let config = get_config(paths, false).await?;
    let ids = selection.resolve(&config).map_err(anyhow::Error::msg)?;
    if ids.is_empty() {
        bail!("no forwards match this selection");
    }
    let command = if restart {
        Command::Restart { selection }
    } else {
        Command::SetDesired {
            selection,
            state: DesiredState::Running,
        }
    };
    // Persist only after selection and conflict validation. A failed command
    // must not start the daemon or awaken unrelated saved running rules.
    let response = offline::mutate(paths, command, Some(config.revision)).await?;
    let response = completion::start_saved(paths, response).await?;
    completion::mutation(
        paths,
        response,
        &ids,
        super::args::wait_effective(args.wait, args.timeout),
        json_output,
    )
    .await?;
    Ok(())
}

pub async fn down(paths: &Paths, args: SelectionArgs, json_output: bool) -> Result<()> {
    let response = offline::control(
        paths,
        Command::SetDesired {
            selection: selection(args),
            state: DesiredState::Stopped,
        },
    )
    .await?;
    output::mutation(response, json_output)?;
    Ok(())
}

pub async fn remove(paths: &Paths, args: SelectionArgs, json_output: bool) -> Result<()> {
    let response = offline::control(
        paths,
        Command::RemoveForwards {
            selection: selection(args),
        },
    )
    .await?;
    output::mutation(response, json_output)?;
    Ok(())
}

pub async fn retry(paths: &Paths, args: SelectionArgs, json_output: bool) -> Result<()> {
    if !client::running(paths).await {
        bail!("daemon is stopped; use daemon start or up to enable forwarding");
    }
    let response = client::request(
        paths,
        Command::Retry {
            selection: selection(args),
        },
    )
    .await?;
    if json_output {
        return output::response(response, true);
    }
    let report: OperationReport = client::decode(response)?;
    let config = get_config(paths, true).await?;
    println!(
        "Retried {} forward(s); skipped {}.",
        report.affected.len(),
        report.skipped.len()
    );
    for id in report.affected {
        println!(
            "  retry: {}",
            config
                .forward(&id)
                .map_or(id.as_str(), |rule| rule.name.as_str())
        );
    }
    for skipped in report.skipped {
        println!("  skipped {}: {}", skipped.name, skipped.reason);
    }
    Ok(())
}

#[cfg(test)]
fn retain_selected(snapshot: &mut fwm_api::protocol::StatusSnapshot, ids: &[String]) {
    snapshot.forwards.retain(|rule| ids.contains(&rule.id));
}

#[cfg(test)]
#[path = "forwards_tests.rs"]
mod tests;
