use anyhow::{Context, Result};
use fwm_core::paths::Paths;
use std::fs::OpenOptions;
use std::process::{Command, Stdio};

/// Spawn the current executable without inheriting a terminal or open stdio pipes.
pub fn spawn(paths: &Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.state_dir)?;
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options
        .open(&paths.log_file)
        .context("opening daemon log")?;
    drop(log);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--config-dir")
        .arg(&paths.config_dir)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        // The daemon owns its bounded startup/diagnostic log. An append-only
        // stdio handle would bypass rotation (including after file renames).
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and does not touch Rust state in the child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    command.spawn().context("starting background daemon")?;
    Ok(())
}
