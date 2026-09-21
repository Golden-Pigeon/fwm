//! Selectors shared by online commands, offline mutations, and UI adapters.
use crate::model::Config;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "by", content = "value", rename_all = "snake_case")]
pub enum Selection {
    All,
    Server(String),
    Forward(String),
    Group(String),
}

impl Selection {
    pub fn resolve(&self, config: &Config) -> Result<Vec<String>, String> {
        let ids = match self {
            Self::All => config.forwards.iter().map(|rule| rule.id.clone()).collect(),
            Self::Server(server) => config.select_server_forwards(server)?,
            Self::Forward(selector) => config.select_forwards(selector)?,
            Self::Group(group) => config.select_group_forwards(group)?,
        };
        Ok(ids)
    }
}
