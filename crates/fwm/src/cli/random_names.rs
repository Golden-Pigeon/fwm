//! Short names sampled without replacement from the unused built-in vocabulary.
use anyhow::{Context, Result};
use fwm_core::model::Config;
use rand::seq::SliceRandom;
use std::collections::HashSet;

const WORDS: &str = include_str!("name_words.txt");

pub(super) struct RandomNames {
    available: Vec<&'static str>,
}

impl RandomNames {
    pub(super) fn new(config: &Config, reserved_group: Option<&str>) -> Self {
        let used: HashSet<_> = config
            .forwards
            .iter()
            .flat_map(|rule| {
                [
                    Some(rule.id.as_str()),
                    Some(rule.name.as_str()),
                    rule.group.as_deref(),
                ]
                .into_iter()
                .flatten()
            })
            .chain(reserved_group)
            .collect();
        let mut available: Vec<_> = WORDS.lines().filter(|word| !used.contains(word)).collect();
        available.shuffle(&mut rand::rng());
        Self { available }
    }

    pub(super) fn take(&mut self) -> Result<String> {
        self.available
            .pop()
            .map(str::to_owned)
            .context("no unused automatic names remain; choose an explicit --name")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fwm_core::model::{
        ConnectionMode, DesiredState, ForwardSpec, RemoteCleanup, ServerProfile, Tunnel,
    };

    #[test]
    fn every_word_is_short_usable_and_selected_once_before_exhaustion() {
        let vocabulary: HashSet<_> = WORDS.lines().collect();
        assert_eq!(vocabulary.len(), 2048);
        assert_eq!(WORDS.lines().count(), vocabulary.len());
        assert!(vocabulary.iter().all(|word| (3..=8).contains(&word.len())
            && word.bytes().all(|byte| byte.is_ascii_lowercase())));
        let mut names = RandomNames::new(&Config::default(), None);
        let mut selected = HashSet::new();
        for _ in 0..vocabulary.len() {
            let name = names.take().unwrap();
            assert!(vocabulary.contains(name.as_str()));
            assert!(selected.insert(name));
        }
        assert!(names.take().unwrap_err().to_string().contains("--name"));
    }

    #[test]
    fn near_capacity_config_reserves_names_ids_groups_and_new_explicit_group() {
        let words: Vec<_> = WORDS.lines().collect();
        let mut server = ServerProfile::new("server");
        server.host = Some("127.0.0.1".into());
        let mut config = Config {
            servers: vec![server.clone()],
            ..Default::default()
        };
        for (index, chunk) in words[..1533].chunks_exact(3).enumerate() {
            config.forwards.push(ForwardSpec {
                id: chunk[0].into(),
                name: chunk[1].into(),
                group: Some(chunk[2].into()),
                server_id: server.id.clone(),
                desired_state: DesiredState::Stopped,
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
                tunnel: Tunnel::Dynamic {
                    listen: ([127, 0, 0, 1], 10000 + index as u16).into(),
                },
            });
        }
        config.validate().unwrap();
        let mut names = RandomNames::new(&config, Some(words[1533]));
        let mut remaining = HashSet::new();
        while let Ok(name) = names.take() {
            assert!(!words[..1534].contains(&name.as_str()));
            assert!(remaining.insert(name));
        }
        assert_eq!(remaining.len(), 2048 - 1534);
    }
}
