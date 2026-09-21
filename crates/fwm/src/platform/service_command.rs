//! Bounded subprocess execution. Capture files avoid blocked pipe-reader threads
//! when a manager delegates work to a descendant which inherits its handles.
use super::{CommandExecutor, CommandResult, ServiceError};
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_OUTPUT: u64 = 1024 * 1024;
pub(super) struct SystemExecutor;
impl CommandExecutor for SystemExecutor {
    fn execute(&mut self, command: &mut Command) -> Result<CommandResult> {
        execute(command, Duration::from_secs(10))
    }
}

struct Capture(PathBuf);
impl Capture {
    fn new() -> Result<(Self, File)> {
        let path =
            std::env::temp_dir().join(format!("fwm-service-output-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        Ok((Self(path), file))
    }
    fn bytes(&self) -> Result<Vec<u8>> {
        let mut result = vec![];
        File::open(&self.0)?
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut result)?;
        Ok(result)
    }
    fn oversized(&self) -> bool {
        fs::metadata(&self.0).is_ok_and(|metadata| metadata.len() > MAX_OUTPUT)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xff, 0xfe]) {
        let words: Vec<_> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&words)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn execute(command: &mut Command, timeout: Duration) -> Result<CommandResult> {
    let (stdout, out_file) = Capture::new()?;
    let (stderr, err_file) = Capture::new()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    let mut child = command
        .spawn()
        .with_context(|| format!("running {command:?}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let oversized = stdout.oversized() || stderr.oversized();
        if oversized || Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + Duration::from_secs(1);
            while child.try_wait()?.is_none() && Instant::now() < reap_deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let (code, reason) = if oversized {
                ("service_output_limit", "exceeded the 1 MiB output limit")
            } else {
                (
                    "service_timeout",
                    "did not finish within the service command deadline",
                )
            };
            return Err(ServiceError::new(code, format!("service command {reason}; its process was terminated, but the manager operation may still complete. Check service status and daemon status before retrying: {command:?}")).into());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = text(&stdout.bytes()?);
    let stderr = text(&stderr.bytes()?);
    if stdout.len() as u64 > MAX_OUTPUT || stderr.len() as u64 > MAX_OUTPUT {
        return Err(ServiceError::new(
            "service_output_limit",
            "service command output exceeded 1 MiB",
        )
        .into());
    }
    Ok(CommandResult {
        success: status.success(),
        code: status.code(),
        details: format!("{status}: {} {}", stdout.trim(), stderr.trim()),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_child() {
        match std::env::var("FWM_TEST_SERVICE_CHILD").as_deref() {
            Ok("stall") => std::thread::sleep(Duration::from_secs(30)),
            Ok("exit") => std::process::exit(7),
            _ => {}
        }
    }
    fn child(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::service::command::tests::fixture_child",
                "--nocapture",
            ])
            .env("FWM_TEST_SERVICE_CHILD", mode);
        command
    }
    #[test]
    fn subprocess_timeout_is_bounded_and_nonzero_exit_is_retained() {
        let before = Instant::now();
        let error = execute(&mut child("stall"), Duration::from_millis(40))
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<ServiceError>().unwrap().code,
            "service_timeout"
        );
        assert!(before.elapsed() < Duration::from_secs(3));
        let result = execute(&mut child("exit"), Duration::from_secs(3)).unwrap();
        assert!(!result.success);
        assert_eq!(result.code, Some(7));
    }
}
