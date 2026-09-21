//! Read-only group discovery, backed by saved configuration in every daemon state.
use super::{args::GroupCommand, get_config, output};
use anyhow::Result;
use fwm_core::{model::DesiredState, paths::Paths};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
struct Member {
    id: String,
    name: String,
    server: String,
    desired_state: DesiredState,
}

#[derive(Serialize)]
struct Group {
    name: String,
    members: Vec<Member>,
    servers: BTreeSet<String>,
}

pub async fn run(paths: &Paths, command: GroupCommand, json_output: bool) -> Result<()> {
    match command {
        GroupCommand::List => list(paths, json_output).await,
    }
}

async fn list(paths: &Paths, json_output: bool) -> Result<()> {
    let config = get_config(paths, false).await?;
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for rule in &config.forwards {
        let Some(name) = &rule.group else {
            continue;
        };
        let server = config
            .server(&rule.server_id)
            .map_or(rule.server_id.as_str(), |server| server.name.as_str());
        let group = groups.entry(name.clone()).or_insert_with(|| Group {
            name: name.clone(),
            members: vec![],
            servers: BTreeSet::new(),
        });
        group.servers.insert(server.to_owned());
        group.members.push(Member {
            id: rule.id.clone(),
            name: rule.name.clone(),
            server: server.to_owned(),
            desired_state: rule.desired_state,
        });
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    for group in &mut groups {
        group.members.sort_by(|a, b| a.name.cmp(&b.name));
    }
    if json_output {
        return output::json_value(
            &serde_json::json!({"revision":config.revision,"groups":groups}),
        );
    }
    if groups.is_empty() {
        println!(
            "No groups configured. Use `fwm add --server SERVER --local --port PORT --group GROUP` or `fwm edit NAME --group GROUP`."
        );
        return Ok(());
    }
    println!("{:<24} {:<8} SERVERS", "GROUP", "MEMBERS");
    for group in groups {
        println!(
            "{:<24} {:<8} {}",
            group.name,
            group.members.len(),
            group.servers.into_iter().collect::<Vec<_>>().join(", ")
        );
        println!(
            "  {}",
            group
                .members
                .into_iter()
                .map(|member| member.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
