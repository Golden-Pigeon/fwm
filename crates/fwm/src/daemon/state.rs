use super::events::EventJournal;
use anyhow::Result;
use fwm_api::protocol::{ApiError, MutationReply, Selection};
use fwm_core::{
    cleanup::CleanupContext, engine::Engine, model::Config, paths::Paths, store::Store,
};
use std::collections::VecDeque;

pub struct State {
    pub instance: String,
    pub config: Config,
    pub store: Store,
    pub engine: Engine,
    pub journal: EventJournal,
    pub completed_requests: VecDeque<(String, String, serde_json::Value)>,
}

impl State {
    pub async fn new(paths: &Paths) -> Result<Self> {
        let store = Store::new(paths.clone());
        let loaded = store.load()?;
        store.initialize(&loaded.config)?;
        let instance = uuid::Uuid::new_v4().to_string();
        let mut journal = EventJournal::new(instance.clone(), paths.state_dir.join("events.jsonl"));
        journal.set_config(&loaded.config);
        if let Some(warning) = loaded.warning {
            journal.record(None, warning);
        }
        Ok(Self {
            instance,
            config: loaded.config,
            store,
            engine: Engine::with_cleanup(CleanupContext::open(paths)?),
            journal,
            completed_requests: VecDeque::new(),
        })
    }

    pub async fn commit(
        &mut self,
        config: Config,
        message: &str,
    ) -> Result<MutationReply, ApiError> {
        if config == self.config {
            return Ok(MutationReply {
                revision: config.revision,
                message: "No changes.".into(),
                config,
                operation: None,
            });
        }
        if self
            .store
            .has_pending_edits()
            .map_err(|e| ApiError::new("storage_error", e.to_string()))?
        {
            return Err(ApiError::new(
                "config_pending_edits",
                "config.toml has unapplied edits; validate and reload it before changing rules",
            ));
        }
        self.commit_reloaded(config, message).await
    }

    pub async fn commit_reloaded(
        &mut self,
        config: Config,
        message: &str,
    ) -> Result<MutationReply, ApiError> {
        let config = self.prepare_commit(config)?;
        let warning = self
            .store
            .commit_reload(&config)
            .map_err(|e| ApiError::new("storage_error", format!("{e:#}")))?;
        self.finish_commit(config, message, warning, None).await
    }

    pub async fn commit_control(
        &mut self,
        config: Config,
        selected: &[String],
        message: &str,
    ) -> Result<MutationReply, ApiError> {
        let config = self.prepare_commit(config)?;
        let warning = self
            .store
            .commit_control_for(&config, selected)
            .map_err(|e| ApiError::new("storage_error", format!("{e:#}")))?;
        self.finish_commit(config, message, warning, Some(selected))
            .await
    }

    fn prepare_commit(&self, mut config: Config) -> Result<Config, ApiError> {
        config.revision = self.config.revision.checked_add(1).ok_or_else(|| {
            ApiError::new("revision_exhausted", "configuration revision exhausted")
        })?;
        config
            .validate()
            .map_err(|e| ApiError::new("invalid_config", e))?;
        Ok(config)
    }

    async fn finish_commit(
        &mut self,
        config: Config,
        message: &str,
        warning: Option<String>,
        selected: Option<&[String]>,
    ) -> Result<MutationReply, ApiError> {
        let changed = selected.map(<[String]>::to_vec).unwrap_or_else(|| {
            self.config
                .forwards
                .iter()
                .chain(config.forwards.iter())
                .filter(|forward| self.config.forward(&forward.id) != config.forward(&forward.id))
                .map(|forward| forward.id.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        });
        let changed_servers: std::collections::BTreeSet<_> = self
            .config
            .servers
            .iter()
            .chain(config.servers.iter())
            .filter(|server| self.config.server(&server.id) != config.server(&server.id))
            .map(|server| server.id.clone())
            .collect();
        self.journal.set_config(&self.config);
        self.config = config;
        self.journal.set_config(&self.config);
        // Persistence precedes runtime convergence. A runtime failure must not
        // make a caller believe its durable configuration vanished.
        let mut details = message.to_string();
        if let Some(warning) = warning {
            details.push_str(&format!("; {warning}"));
        }
        if let Err(error) = self.engine.reconcile(&self.config).await {
            details.push_str(&format!(
                "; saved, runtime reconciliation needs attention: {error:#}"
            ));
        }
        if changed.is_empty() && changed_servers.is_empty() {
            self.journal.record(None, details.clone());
        } else {
            for id in changed {
                self.journal.record(Some(id), details.clone());
            }
        }
        for id in changed_servers {
            self.journal.record_server(id, details.clone());
        }
        Ok(MutationReply {
            revision: self.config.revision,
            message: details,
            config: self.config.clone(),
            operation: None,
        })
    }

    pub fn select(&self, selection: &Selection) -> Result<Vec<String>, ApiError> {
        selection
            .resolve(&self.config)
            .map_err(|message| ApiError::new("not_found", message))
    }
}
