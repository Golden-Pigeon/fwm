use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

pub struct Stream {
    child: Child,
    lines: Receiver<(bool, String)>,
    pub output: Vec<String>,
    pub errors: Vec<String>,
    value_cursor: usize,
    warning_cursor: usize,
}

impl Stream {
    pub fn start(directory: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fwm"))
            .arg("--config-dir")
            .arg(directory)
            .arg("--json")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (sender, lines) = mpsc::channel();
        let out = child.stdout.take().unwrap();
        let error = child.stderr.take().unwrap();
        let stdout_sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                if stdout_sender.send((false, line.unwrap())).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(error).lines() {
                if sender.send((true, line.unwrap())).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            output: vec![],
            errors: vec![],
            value_cursor: 0,
            warning_cursor: 0,
        }
    }

    fn line(&mut self, deadline: Instant) -> Option<(bool, String)> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (stderr, text) = self.lines.recv_timeout(remaining).ok()?;
        if stderr {
            self.errors.push(text.clone());
        } else {
            self.output.push(text.clone());
        }
        Some((stderr, text))
    }

    pub fn value_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            // stdout and stderr have independent reader threads. Waiting for a
            // warning must not discard a value that arrives before that warning.
            while self.value_cursor < self.output.len() {
                let text = &self.output[self.value_cursor];
                self.value_cursor += 1;
                let value: Value = serde_json::from_str(text).expect("stream stdout must be JSONL");
                if predicate(&value) {
                    return value;
                }
            }
            if self.line(deadline).is_none() {
                break;
            }
        }
        panic!(
            "stream did not produce expected value; errors={:?}; recent output={:?}",
            self.errors,
            self.output.iter().rev().take(4).collect::<Vec<_>>()
        );
    }

    pub fn warning_until(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            while self.warning_cursor < self.errors.len() {
                let text = &self.errors[self.warning_cursor];
                self.warning_cursor += 1;
                if text.contains(needle) {
                    return;
                }
            }
            if self.line(deadline).is_none() {
                break;
            }
        }
        panic!("missing warning {needle:?}: {:?}", self.errors);
    }

    pub fn finish(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                while self.line(deadline).is_some() {}
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not exit: {:?}",
                self.errors
            );
            self.line(deadline.min(Instant::now() + Duration::from_millis(20)));
        }
    }

    pub fn interrupt(&mut self) {
        // Only signal the child created by this fixture.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) },
            0
        );
        let status = self.finish(Duration::from_secs(5));
        assert!(
            status.success(),
            "Ctrl-C was not graceful: {status:?}, {:?}",
            self.errors
        );
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn cli(directory: &Path, args: &[&str]) -> Value {
    let mut process = Stream::start(directory, args);
    let status = process.finish(Duration::from_secs(15));
    assert!(status.success(), "{args:?}: {:?}", process.errors);
    serde_json::from_str(&process.output.join("\n")).unwrap()
}
