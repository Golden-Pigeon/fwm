#[cfg(test)]
mod batch;
mod dispatch;
mod events;
mod logging;
mod probes;
mod state;

use crate::platform::ipc;
use anyhow::{Context, Result};
use fs2::FileExt;
use fwm_api::{
    codec::{read_frame, write_frame},
    protocol::{ApiError, Request, Response},
};
use fwm_core::paths::Paths;
use std::{fs::OpenOptions, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

pub async fn run(paths: Paths) -> Result<()> {
    let result = run_inner(&paths).await;
    if let Err(error) = &result
        && let Err(log_error) = logging::record_failure(&paths.log_file, error)
    {
        eprintln!(
            "could not write daemon log {}: {log_error}",
            paths.log_file.display()
        );
    }
    result
}

pub(crate) fn error_message(error: &anyhow::Error) -> String {
    logging::error_message(error)
}

async fn run_inner(paths: &Paths) -> Result<()> {
    paths.ensure_dirs()?;
    logging::initialize(paths.log_file.clone());
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(&paths.lock_file)?;
    lock.try_lock_exclusive()
        .context("another daemon already owns this configuration directory")?;
    let listener = ipc::bind(paths)?;
    let mut initial = state::State::new(paths).await?;
    let mut engine_events = initial.engine.subscribe();
    initial.engine.reconcile(&initial.config).await?;
    initial.journal.record(None, "daemon started".into());
    let state = Arc::new(Mutex::new(initial));
    let stop = CancellationToken::new();
    let permits = Arc::new(Semaphore::new(64));
    let mut clients = tokio::task::JoinSet::new();
    let signal = stop.clone();
    let signal_task = tokio::spawn(async move {
        #[cfg(unix)]
        {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            } else {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        signal.cancel();
    });
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            event = engine_events.recv() => match event {
                Ok(event) => state.lock().await.journal.record_engine(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => state.lock().await.journal.record(None, format!("{count} runtime events were coalesced; refresh status")),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            result = listener.accept() => {
                let mut stream = result?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let state = state.clone(); let stop = stop.clone();
                clients.spawn(async move {
                    let _permit = permit;
                    let request = tokio::time::timeout(Duration::from_secs(10), read_frame::<Request, _>(&mut stream)).await;
                    if let Ok(Ok(Some(request))) = request {
                        let response = dispatch::dispatch(state, request, stop).await;
                        if let Ok(Err(error)) = tokio::time::timeout(Duration::from_secs(10), write_frame(&mut stream, &response)).await
                            && error.kind() == std::io::ErrorKind::InvalidData {
                                let failure = Response::failure(response.request_id, ApiError::new("response_too_large", "response exceeds the IPC frame limit"));
                                let _ = tokio::time::timeout(Duration::from_secs(2), write_frame(&mut stream, &failure)).await;
                        }
                    }
                });
            },
            _ = clients.join_next(), if !clients.is_empty() => {},
        }
    }
    // Permit the shutdown request's response to reach its client before ending
    // other RPC handlers; outstanding probes cannot hold shutdown indefinitely.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while clients.join_next().await.is_some() {}
    })
    .await;
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    {
        let mut state = state.lock().await;
        state.engine.shutdown().await;
        state.journal.record(None, "daemon stopped".into());
    }
    signal_task.abort();
    drop(listener);
    drop(lock);
    Ok(())
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[tokio::test]
    async fn direct_daemon_start_records_loading_error_without_stdio_redirection() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(directory.path().to_owned())).unwrap();
        std::fs::write(&paths.config_file, "not valid toml =").unwrap();
        let error = run(paths.clone()).await.unwrap_err();
        let log = std::fs::read_to_string(&paths.log_file).unwrap();
        assert!(log.contains("daemon failed:"));
        assert!(log.contains(&error_message(&error)));
    }
}
