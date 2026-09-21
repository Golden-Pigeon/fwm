//! Bounded diagnostic output; lifecycle events use the separate event journal.
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{EnvFilter, fmt::MakeWriter};

#[derive(Clone)]
struct RollingLog {
    state: Arc<Mutex<LogState>>,
}

struct LogState {
    path: PathBuf,
    max_bytes: u64,
}

impl<'a> MakeWriter<'a> for RollingLog {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Write for RollingLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("diagnostic log lock poisoned"))?;
        if std::fs::symlink_metadata(&state.path)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(io::Error::other("refusing symlink diagnostic log"));
        }
        if std::fs::metadata(&state.path)
            .is_ok_and(|metadata| metadata.len() + bytes.len() as u64 > state.max_bytes)
        {
            let backup = state.path.with_extension("log.1");
            if backup.exists() {
                std::fs::remove_file(&backup)?;
            }
            std::fs::rename(&state.path, backup)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&state.path)?;
        // One oversized diagnostic cannot bypass the retention budget.
        file.write_all(&bytes[..bytes.len().min(state.max_bytes as usize)])?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn initialize(path: PathBuf) {
    let writer = RollingLog {
        state: Arc::new(Mutex::new(LogState {
            path,
            max_bytes: 2 * 1024 * 1024,
        })),
    };
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(writer)
        .try_init();
}

pub(super) fn error_message(error: &anyhow::Error) -> String {
    const MAX_ERROR_BYTES: usize = 8 * 1024;
    let message = format!("{error:#}");
    if message.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES / 2;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        let mut start = message.len() - MAX_ERROR_BYTES / 2;
        while !message.is_char_boundary(start) {
            start += 1;
        }
        // TOML puts the reason after its source excerpt; retain both ends.
        return format!(
            "{}\n[error text truncated]\n{}",
            &message[..end],
            &message[start..]
        );
    }
    message
}

pub(super) fn record_failure(path: &std::path::Path, error: &anyhow::Error) -> io::Result<()> {
    let mut writer = RollingLog {
        state: Arc::new(Mutex::new(LogState {
            path: path.to_owned(),
            max_bytes: 2 * 1024 * 1024,
        })),
    };
    writeln!(writer, "daemon failed: {}", error_message(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_log_retains_only_one_bounded_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("diagnostics.log");
        let mut log = RollingLog {
            state: Arc::new(Mutex::new(LogState {
                path: path.clone(),
                max_bytes: 64,
            })),
        };
        for _ in 0..10 {
            log.write_all(&[b'x'; 48]).unwrap();
        }
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 48);
        assert_eq!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len(),
            48
        );
        log.write_all(&[b'x'; 128]).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), 64);
    }

    #[test]
    fn repeated_startup_errors_use_bounded_shared_daemon_log() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("daemon.log");
        let error = anyhow::anyhow!("invalid config: {}\nexpected string", "坏".repeat(100_000));
        let message = error_message(&error);
        assert!(message.len() < 8300);
        assert!(message.starts_with("invalid config:"));
        assert!(message.ends_with("expected string"));
        for _ in 0..300 {
            record_failure(&path, &error).unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() <= 2 * 1024 * 1024);
        assert!(
            std::fs::metadata(path.with_extension("log.1"))
                .unwrap()
                .len()
                <= 2 * 1024 * 1024
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
    }
}
