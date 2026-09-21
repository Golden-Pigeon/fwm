//! Direct production completion/output/configuration imports; memory client only.
use anyhow::Result;
use fwm_api::protocol::{Command, MutationReply, Response, StatusSnapshot};
use fwm_core::{model::{Config, DesiredState, ForwardSpec, RuntimeState, ServerProfile, Tunnel}, paths::Paths};
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::{Mutex, OnceLock}, time::Duration};

#[path = "../../crates/fwm/src/configuration.rs"]
mod configuration;

#[path = "."]
mod cli {
    #[path = "../../crates/fwm/src/cli/output.rs"]
    pub mod output;
    #[path = "../../crates/fwm/src/cli/completion.rs"]
    pub mod completion;
    #[path = "../../crates/fwm/src/cli/input.rs"]
    mod input;
    #[path = "../../crates/fwm/src/cli/args.rs"]
    pub mod args;
}

#[derive(Clone)]
enum Step { Snapshot(StatusSnapshot), Failure, Stall, Delayed(Duration, StatusSnapshot) }
#[derive(Default)]
struct MemoryClient {
    steps: VecDeque<Step>,
    last: Option<StatusSnapshot>,
    calls: usize,
    start_delay: Duration,
    start_error: Option<&'static str>,
    starts: usize,
}
static MEMORY: OnceLock<Mutex<MemoryClient>> = OnceLock::new();
fn memory() -> &'static Mutex<MemoryClient> { MEMORY.get_or_init(Default::default) }

mod client {
    use super::*;
    #[derive(Debug)]
    pub struct ClientError { pub code: String, pub message: String }
    impl std::fmt::Display for ClientError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}: {}", self.code, self.message) }
    }
    impl std::error::Error for ClientError {}
    pub async fn ensure_running(_: &Paths) -> Result<()> {
        let (delay, error) = { let mut state = memory().lock().unwrap(); state.starts += 1; (state.start_delay, state.start_error) };
        tokio::time::sleep(delay).await;
        if let Some(code) = error { return Err(ClientError { code: code.into(), message: "memory fixture start failure".into() }.into()); }
        Ok(())
    }
    pub async fn request(_: &Paths, command: Command) -> Result<Response> {
        assert!(matches!(command, Command::Status));
        let next = {
            let mut state = memory().lock().unwrap();
            state.calls += 1;
            state.steps.pop_front().unwrap_or_else(|| Step::Snapshot(state.last.clone().expect("scripted status exhausted")))
        };
        let snapshot = match next {
            Step::Snapshot(value) => value,
            Step::Delayed(delay, value) => { tokio::time::sleep(delay).await; value },
            Step::Failure => return Err(ClientError { code: "daemon_unavailable".into(), message: "memory fixture daemon restart gap".into() }.into()),
            Step::Stall => std::future::pending().await,
        };
        memory().lock().unwrap().last = Some(snapshot.clone());
        Ok(Response::success("memory".into(), serde_json::to_value(snapshot)?))
    }
    pub fn decode<T: serde::de::DeserializeOwned>(response: Response) -> Result<T> { Ok(serde_json::from_value(response.data)?) }
}

fn config(count: usize) -> Config {
    let mut server = ServerProfile::new("alpha"); server.id = "server-a".into(); server.host = Some("alpha.invalid".into());
    let mut second = ServerProfile::new("beta"); second.id = "server-b".into(); second.host = Some("beta.invalid".into());
    let value = Config {
        revision: 7,
        servers: vec![server, second],
        forwards: (0..count).map(|n| ForwardSpec {
            id: format!("rule-{n}"), name: format!("web-{n}"), group: Some("batch".into()), server_id: "server-a".into(),
            tunnel: Tunnel::Local { listen: format!("127.0.0.1:{}", 42000+n).parse().unwrap(), target: "localhost:8080".parse().unwrap() },
            desired_state: DesiredState::Running, connection_mode: Default::default(), remote_cleanup: Default::default(),
        }).collect(),
        ..Default::default()
    };
    value.validate().unwrap(); value
}

fn snapshot(config: &Config, states: &[RuntimeState]) -> StatusSnapshot {
    config.validate().unwrap();
    assert_eq!(config.forwards.len(), states.len());
    let mut result = cli::output::offline(config.clone());
    result.daemon_instance_id = "memory-daemon".into();
    for (rule, state) in result.forwards.iter_mut().zip(states) {
        rule.state = *state; rule.last_error = None;
    }
    result
}

fn edited(config: &Config, forward: ForwardSpec) -> Config {
    let command = Command::PutForwardsWithServer { forwards: vec![forward], server: None };
    let mut result = configuration::prepare(config, &command).unwrap().config;
    result.revision = config.revision + 1;
    result
}

fn saved(config: Config) -> Response {
    Response::success("accepted-mutation".into(), serde_json::to_value(MutationReply {
        revision: config.revision, message: "saved; daemon remains stopped.".into(), config, operation: None,
    }).unwrap())
}

async fn run(case: &str) -> Result<()> {
    use RuntimeState::*;
    let count = if case.starts_with("batch") { 2 } else { 1 };
    let original = config(count);
    let ids: Vec<String> = original.forwards.iter().map(|rule| rule.id.clone()).collect();
    let mut latest = original.clone();
    let mut steps = VecDeque::new();
    let timeout = Duration::from_millis(350);
    let mut expect_success = true;
    let mut error_code = None;
    let mut start_first = false;
    let mut wait = Some(timeout);
    match case {
        "port_replaced_during_wait" | "batch_member_port_replaced" => {
            steps.push_back(Step::Snapshot(snapshot(&original, &vec![Starting; count])));
            let mut rule = latest.forwards[count-1].clone();
            rule.tunnel = Tunnel::Local { listen: "127.0.0.1:43000".parse().unwrap(), target: "localhost:9090".parse().unwrap() };
            latest = edited(&latest, rule);
            steps.push_back(Step::Snapshot(snapshot(&latest, &vec![Established; count])));
        }
        "server_replaced_during_wait" => {
            steps.push_back(Step::Snapshot(snapshot(&original, &[Starting])));
            let mut rule = latest.forwards[0].clone(); rule.server_id = "server-b".into();
            latest = edited(&latest, rule);
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "server_profile_replaced_same_display" => {
            steps.push_back(Step::Snapshot(snapshot(&original, &[Starting])));
            let mut server = latest.servers[0].clone(); server.host = Some("replacement.invalid".into());
            latest = configuration::prepare(&latest, &Command::PutServer { server }).unwrap().config;
            latest.revision += 1;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "kind_replaced_during_wait" => {
            let mut rule = latest.forwards[0].clone();
            rule.tunnel = Tunnel::Dynamic { listen: "127.0.0.1:43000".parse().unwrap() };
            latest = edited(&latest, rule);
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "rename_only_control" => {
            let mut rule = latest.forwards[0].clone(); rule.name = "renamed".into();
            latest = edited(&latest, rule);
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "unrelated_revision_control" => {
            let mut server = latest.servers[1].clone(); server.name = "renamed-beta".into();
            latest = configuration::prepare(&latest, &Command::PutServer { server }).unwrap().config;
            latest.revision += 1;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "batch_partial_then_all_ready" => {
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, Starting])));
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, Established])));
        }
        "batch_partial_timeout" => {
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, Starting])));
            expect_success = false; error_code = Some("wait_timeout");
        }
        "batch_attention_fails" => {
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, NeedsAttention])));
            expect_success = false; error_code = Some("needs_attention");
        }
        "batch_added_group_member_not_selected" => {
            let mut extra = latest.forwards[0].clone(); extra.id = "new-member".into(); extra.name = "new-member".into();
            extra.tunnel = Tunnel::Local { listen: "127.0.0.1:45000".parse().unwrap(), target: "localhost:8080".parse().unwrap() };
            latest = configuration::prepare(&latest, &Command::CreateForwards { forwards: vec![extra], server: None }).unwrap().config;
            latest.revision += 1;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, Established, Starting])));
        }
        "selected_deleted_control" => {
            latest = configuration::prepare(&latest, &Command::RemoveForward { selector: "rule-0".into() }).unwrap().config;
            latest.revision += 1;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[])));
            expect_success = false; error_code = Some("wait_timeout");
        }
        "selected_stopped_control" => {
            latest = configuration::prepare(&latest, &Command::SetDesired { selection: fwm_api::protocol::Selection::Forward("rule-0".into()), state: DesiredState::Stopped }).unwrap().config;
            latest.revision += 1;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Stopped])));
            expect_success = false; error_code = Some("wait_timeout");
        }
        "same_name_new_id_control" => {
            latest = configuration::prepare(&latest, &Command::RemoveForward { selector: "rule-0".into() }).unwrap().config;
            let mut rule = original.forwards[0].clone(); rule.id = "replacement-id".into();
            latest = configuration::prepare(&latest, &Command::CreateForwards { forwards: vec![rule], server: None }).unwrap().config;
            latest.revision += 2;
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
            expect_success = false; error_code = Some("wait_timeout");
        }
        "unselected_attention_ignored" => {
            latest = config(2);
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established, NeedsAttention])));
        }
        "restart_instance_control" => {
            steps.push_back(Step::Snapshot(snapshot(&original, &[Starting])));
            let mut restarted = snapshot(&original, &[Established]); restarted.daemon_instance_id = "restarted-daemon".into();
            steps.push_back(Step::Snapshot(restarted));
        }
        "request_stall_deadline" => { steps.push_back(Step::Stall); expect_success = false; error_code = Some("wait_timeout"); }
        "request_slow_success_deadline" => { steps.push_back(Step::Delayed(Duration::from_secs(1), snapshot(&latest, &[Established]))); expect_success = false; error_code = Some("wait_timeout"); }
        "request_failure_preserves_last" => {
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Starting]))); steps.push_back(Step::Failure);
            expect_success = false; error_code = Some("wait_failed");
        }
        "startup_failure_reports_saved" => {
            start_first = true; expect_success = false; error_code = Some("daemon_unavailable");
            memory().lock().unwrap().start_error = Some("daemon_unresponsive");
        }
        "startup_delay_outside_wait_budget" => {
            start_first = true; memory().lock().unwrap().start_delay = Duration::from_secs(2);
            steps.push_back(Step::Snapshot(snapshot(&latest, &[Established])));
        }
        "no_wait_does_not_query_runtime" => { wait = None; }
        _ => panic!("unknown case {case}"),
    }
    memory().lock().unwrap().steps = steps;
    let paths = Paths::new(Some("/private/tmp/fwm-memory-only-not-created".into()))?;
    let began = tokio::time::Instant::now();
    let mut response = saved(original.clone());
    let result = if start_first {
        match cli::completion::start_saved(&paths, response).await {
            Ok(next) => { response = next; cli::completion::mutation(&paths, response, &ids, wait, true).await.map(|_| ()) },
            Err(error) => Err(error),
        }
    } else { cli::completion::mutation(&paths, response, &ids, wait, true).await.map(|_| ()) };
    assert_eq!(result.is_ok(), expect_success);
    if let Err(error) = result {
        let error = error.downcast_ref::<cli::completion::CompletionError>().unwrap();
        assert_eq!(Some(error.code.as_str()), error_code);
        assert_eq!(error.result["data"]["saved"], true);
        assert_eq!(error.result["data"]["revision"], 7);
        println!("{}", error.result);
    }
    println!("{}", json!({"audit_case":case,"elapsed_virtual_ms":began.elapsed().as_millis(),"status_queries":memory().lock().unwrap().calls,"original_config":original,"latest_validated_config":latest}));
    Ok(())
}

fn parser_contracts() {
    let cases: Vec<(&str, Vec<&str>, Option<u64>, bool)> = vec![
        ("no_wait", vec![], None, false),
        ("wait_default", vec!["--wait"], Some(20_000), false),
        ("timeout_implies_wait", vec!["--timeout", "500ms"], Some(500), false),
        ("timeout_overrides_default", vec!["--wait", "--timeout", "2m"], Some(120_000), false),
        ("bare_duration_means_seconds", vec!["--timeout", "2"], Some(2_000), false),
        ("seconds", vec!["--timeout", "3s"], Some(3_000), false),
        ("zero_rejected", vec!["--timeout", "0ms"], None, true),
        ("negative_rejected", vec!["--timeout", "-1s"], None, true),
        ("fraction_rejected", vec!["--timeout", "2.5s"], None, true),
        ("empty_rejected", vec!["--timeout", ""], None, true),
        ("missing_value_rejected", vec!["--timeout"], None, true),
        ("unknown_unit_rejected", vec!["--timeout", "2h"], None, true),
    ];
    let mut results = vec![];
    for (name, flags, expected, reject) in cases {
        let mut argv = vec!["fwm", "up", "--all"]; argv.extend(flags);
        let parsed = cli::args::Cli::try_parse_from(argv.clone());
        assert_eq!(parsed.is_err(), reject, "{name}");
        let milliseconds = match parsed {
            Ok(cli::args::Cli { command: cli::args::Command::Up(args), .. }) => cli::args::wait_effective(args.wait, args.timeout).map(|duration| duration.as_millis() as u64),
            Err(_) => None,
            _ => panic!("wrong command"),
        };
        assert_eq!(milliseconds, expected, "{name}");
        results.push(json!({"case":name,"argv":argv,"rejected":reject,"effective_wait_ms":milliseconds,"passed":true}));
    }
    println!("{}", json!({"parser_cases":results}));
}

fn main() -> Result<()> {
    let case = std::env::args().nth(1).unwrap();
    if case == "parser_contracts" { parser_contracts(); return Ok(()); }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async { tokio::time::pause(); run(&case).await })
}
