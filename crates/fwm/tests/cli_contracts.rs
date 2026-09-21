//! Process-level contracts: documentation and invalid requests have no side
//! effects; successful non-waiting mutations do not claim listener readiness.
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn run(directory: &Path, args: &[&str], json: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fwm"));
    command.arg("--config-dir").arg(directory);
    if json {
        command.arg("--json");
    }
    command.args(args).output().unwrap()
}

#[test]
fn metadata_and_argument_errors_exit_without_starting_tokio() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("never-created");
    let run_without_runtime = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fwm"))
            // Tokio rejects zero workers when constructing its runtime. Set it
            // only in the child so unrelated tests retain their environment.
            .env("TOKIO_WORKER_THREADS", "0")
            .arg("--config-dir")
            .arg(&directory)
            .args(args)
            .output()
            .unwrap()
    };

    for (args, expected) in [
        (vec!["--version"], "fwm "),
        (vec!["-V"], "fwm "),
        (vec!["--json", "--version"], "fwm "),
        (vec!["--help"], "Usage:"),
        (vec!["-h"], "Usage:"),
        (vec!["server", "edit", "--help"], "Usage:"),
        (vec!["--json", "daemon", "run", "--help"], "Usage:"),
    ] {
        let output = run_without_runtime(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(expected),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            !directory.exists(),
            "{args:?} unexpectedly initialized configuration"
        );
    }

    for args in [
        vec!["--json", "not-a-command"],
        vec!["--json", "add", "--local", "--port", "3000"],
        vec!["--json", "status", "web", "--all"],
    ] {
        let output = run_without_runtime(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "invalid_arguments");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("error:") && message.contains("--help"),
            "{args:?}: {message}"
        );
        assert!(
            !directory.exists(),
            "{args:?} unexpectedly initialized configuration"
        );
    }
}

#[test]
fn help_and_version_for_every_command_family_do_not_initialize_configuration() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("never-created");
    let commands = [
        vec![],
        vec!["add"],
        vec!["edit"],
        vec!["group"],
        vec!["group", "list"],
        vec!["status"],
        vec!["up"],
        vec!["down"],
        vec!["restart"],
        vec!["retry"],
        vec!["remove"],
        vec!["logs"],
        vec!["doctor"],
        vec!["server"],
        vec!["server", "add"],
        vec!["server", "edit"],
        vec!["server", "trust"],
        vec!["server", "check"],
        vec!["server", "remove"],
        vec!["server", "list"],
        vec!["config", "validate"],
        vec!["config", "reload"],
        vec!["config", "export"],
        vec!["daemon", "run"],
        vec!["daemon", "start"],
        vec!["daemon", "stop"],
        vec!["daemon", "restart"],
        vec!["daemon", "status"],
        vec!["service", "install"],
        vec!["service", "uninstall"],
    ];
    for mut args in commands {
        args.push("--help");
        let output = run(&directory, &args, true);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
        assert!(
            !directory.exists(),
            "{args:?} unexpectedly initialized configuration"
        );
    }
    let version = run(&directory, &["--version"], true);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("fwm "));
    assert!(!directory.exists());
}

#[test]
fn argument_errors_emit_one_json_failure_with_help_and_no_filesystem_side_effects() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("never-created");
    for args in [
        vec!["add", "--local", "--port", "3000"],
        vec!["add", "--server", "dev", "--local", "--src", "3000"],
        vec![
            "add", "--server", "dev", "--local", "--remote", "--port", "3000",
        ],
        vec!["status", "web", "--all"],
        vec!["restart"],
        vec!["up", "web", "--wait", "--timeout", "18446744073709551615s"],
        vec!["doctor", "--ssh-config", "unused.conf"],
        vec!["server", "edit", "dev", "--unset", "not-a-field"],
    ] {
        let output = run(&directory, &args, true);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["ok"], false);
        assert_eq!(error["error"]["code"], "invalid_arguments");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("error:") && message.contains("--help"),
            "{args:?}: {message}"
        );
        assert!(
            !directory.exists(),
            "{args:?} unexpectedly initialized configuration"
        );
    }
}

#[test]
fn waiting_for_a_disabled_add_fails_before_saving_or_starting_a_daemon() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("never-created");
    let output = run(
        &directory,
        &[
            "add",
            "--server",
            "dev",
            "--local",
            "--port",
            "3000",
            "--disabled",
            "--wait",
        ],
        true,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--wait cannot be combined with --disabled")
    );
    assert!(!directory.exists());
}

#[test]
fn non_waiting_disabled_add_reports_saved_stopped_and_no_readiness_claim() {
    let temporary = tempfile::tempdir().unwrap();
    let output = run(
        temporary.path(),
        &[
            "add",
            "--server",
            "dev",
            "--remote",
            "--port",
            "3000",
            "--disabled",
        ],
        true,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["saved"], true);
    assert_eq!(result["data"]["state"], "stopped");
    assert!(result["data"]["ready"].is_null());
    assert!(result["data"]["runtime"].is_null());
    let status = run(temporary.path(), &["daemon", "status"], true);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["daemon_running"], false);
    let human = run(
        temporary.path(),
        &[
            "add",
            "--server",
            "dev",
            "--local",
            "--port",
            "3001",
            "--disabled",
        ],
        false,
    );
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("stopped"), "{text}");
    assert!(
        !text.contains("ready") && !text.contains("established"),
        "{text}"
    );
}
