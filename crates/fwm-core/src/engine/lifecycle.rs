use super::*;
use crate::model::{OperationReport, SkippedForward};

#[derive(Clone, Copy)]
enum Operation {
    Retry,
    Restart,
}

impl Engine {
    /// Retry unhealthy rules without disrupting healthy siblings.
    pub async fn retry(&mut self, ids: &[String]) -> Result<OperationReport> {
        self.request_generation(ids, Operation::Retry)
    }

    /// Rebuild selected running listeners. Shared SSH connections remain intact.
    /// The caller must persist running intent before restarting a stopped rule.
    pub async fn restart(&mut self, ids: &[String]) -> Result<OperationReport> {
        self.request_generation(ids, Operation::Restart)
    }

    /// Explicit server restart rebuilds its SSH sessions and re-resolves aliases.
    /// Other servers and their streams retain their original sessions.
    pub async fn reconnect_server(
        &mut self,
        config: &Config,
        selector: &str,
    ) -> Result<OperationReport> {
        let server = config.server(selector).context("unknown server")?;
        self.ssh_fingerprints.remove(&server.id);
        let ids = config
            .select_server_forwards(&server.id)
            .map_err(anyhow::Error::msg)?;
        let report = self.request_generation(&ids, Operation::Restart)?;
        let keys: Vec<_> = self
            .groups
            .iter()
            .filter(|(_, group)| {
                group
                    .desired
                    .borrow()
                    .iter()
                    .any(|rule| rule.spec.server_id == server.id)
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut retired = Vec::new();
        for key in keys {
            if let Some(group) = self.groups.remove(&key) {
                group.cancel.cancel();
                retired.push(group.task);
            }
        }
        // Closing old listeners before replacements avoids a spurious port
        // conflict, especially for remote forwards with verified recovery.
        join_cancelled(retired).await;
        self.reconcile(config).await?;
        Ok(report)
    }

    /// Only an explicit configuration reload refreshes otherwise unchanged
    /// aliases; ordinary edits must not interrupt unrelated SSH sessions.
    pub async fn refresh_ssh_config(&mut self, config: &Config) -> Result<()> {
        self.ssh_fingerprints.clear();
        self.reconcile(config).await
    }

    fn request_generation(
        &mut self,
        ids: &[String],
        operation: Operation,
    ) -> Result<OperationReport> {
        let mut report = OperationReport {
            affected: vec![],
            skipped: vec![],
        };
        let mut seen = HashSet::new();
        let statuses = self.statuses.lock().unwrap();
        // Validate the entire selection before changing a generation.
        for id in ids {
            if !statuses.contains_key(id) {
                return Err(anyhow!("unknown forward {id}"));
            }
        }
        for id in ids {
            if !seen.insert(id) {
                continue;
            }
            let entry = &statuses[id];
            let reason = if entry.spec.desired_state != DesiredState::Running {
                Some("stopped")
            } else if matches!(operation, Operation::Retry)
                && self
                    .groups
                    .get(&entry.connection_key)
                    .is_some_and(|group| !group.task.is_finished())
            {
                match entry.status.state {
                    RuntimeState::Established => Some("already_established"),
                    RuntimeState::Starting => Some("already_starting"),
                    RuntimeState::Stopping => Some("stopping"),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(reason) = reason {
                report.skipped.push(SkippedForward {
                    id: id.clone(),
                    name: entry.spec.name.clone(),
                    reason: reason.into(),
                });
            } else {
                report.affected.push(id.clone());
            }
        }
        drop(statuses);
        self.generation
            .checked_add(report.affected.len() as u64)
            .context("runtime generation exhausted")?;
        let selected: HashSet<_> = report.affected.iter().collect();
        for group in self.groups.values() {
            let mut rules = group.desired.borrow().clone();
            let mut changed = false;
            for rule in &mut rules {
                if !selected.contains(&rule.spec.id) {
                    continue;
                }
                let mut statuses = self.statuses.lock().unwrap();
                let entry = statuses.get_mut(&rule.spec.id).unwrap();
                self.generation += 1;
                entry.generation = self.generation;
                entry.status.active_connections = 0;
                entry.status.retry_count = 0;
                entry.status.state = RuntimeState::Starting;
                entry.status.last_error = None;
                entry.status.next_retry_unix_ms = None;
                rule.generation = self.generation;
                changed = true;
            }
            if changed {
                group.desired.send_replace(rules);
            }
        }
        self.revive_finished_groups();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ForwardSpec, RemoteCleanup, Tunnel};

    async fn fixture() -> Engine {
        let mut server = ServerProfile::new("local-fixture");
        server.host = Some("127.0.0.1".into());
        let forwards = (0..3)
            .map(|index| ForwardSpec {
                id: format!("rule-{index}"),
                name: format!("rule-{index}"),
                group: None,
                server_id: server.id.clone(),
                tunnel: Tunnel::Local {
                    listen: ([127, 0, 0, 1], 32000 + index).into(),
                    target: "localhost:22".parse().unwrap(),
                },
                desired_state: if index == 2 {
                    DesiredState::Stopped
                } else {
                    DesiredState::Running
                },
                connection_mode: ConnectionMode::Shared,
                remote_cleanup: RemoteCleanup::Off,
            })
            .collect();
        let mut engine = Engine::new();
        engine
            .reconcile(&Config {
                servers: vec![server],
                forwards,
                ..Config::default()
            })
            .await
            .unwrap();
        // Tasks have not been polled yet; exercise the control transaction alone.
        {
            let mut statuses = engine.statuses.lock().unwrap();
            statuses.get_mut("rule-0").unwrap().status.state = RuntimeState::Established;
            statuses.get_mut("rule-1").unwrap().status.state = RuntimeState::Backoff;
        }
        engine
    }

    #[tokio::test]
    async fn retry_reports_skips_and_only_bumps_unhealthy_running_rules() {
        let mut engine = fixture().await;
        let healthy = engine.statuses.lock().unwrap()["rule-0"].generation;
        let report = engine
            .retry(&["rule-0".into(), "rule-1".into(), "rule-2".into()])
            .await
            .unwrap();
        assert_eq!(report.affected, ["rule-1"]);
        assert_eq!(
            report
                .skipped
                .iter()
                .map(|value| value.reason.as_str())
                .collect::<Vec<_>>(),
            ["already_established", "stopped"]
        );
        assert_eq!(
            engine.statuses.lock().unwrap()["rule-0"].generation,
            healthy
        );
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn restart_rebuilds_healthy_selection_without_replacing_shared_connection_or_siblings() {
        let mut engine = fixture().await;
        let (selected, sibling) = {
            let statuses = engine.statuses.lock().unwrap();
            (statuses["rule-0"].generation, statuses["rule-1"].generation)
        };
        let key = engine.groups.keys().next().unwrap().clone();
        let cancel = engine.groups[&key].cancel.clone();
        let report = engine.restart(&["rule-0".into()]).await.unwrap();
        assert_eq!(report.affected, ["rule-0"]);
        assert!(report.skipped.is_empty());
        assert!(engine.groups.contains_key(&key));
        assert!(!cancel.is_cancelled());
        {
            let statuses = engine.statuses.lock().unwrap();
            assert!(statuses["rule-0"].generation > selected);
            assert_eq!(statuses["rule-1"].generation, sibling);
        }
        engine.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_selection_does_not_partially_restart() {
        let mut engine = fixture().await;
        let generation = engine.generation;
        assert!(
            engine
                .restart(&["rule-0".into(), "missing".into()])
                .await
                .is_err()
        );
        assert_eq!(engine.generation, generation);
        engine.shutdown().await;
    }
}
