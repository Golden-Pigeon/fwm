mod cli;
mod client;
mod configuration;
mod daemon;
mod offline;
mod platform;
mod ssh_actions;
#[cfg(test)]
mod test_support;

fn main() -> std::process::ExitCode {
    cli::shell_completion::complete();
    // Help, version and argument errors exit before creating runtime threads.
    let raw: Vec<_> = std::env::args_os().collect();
    let json_requested = raw
        .iter()
        .take_while(|argument| argument.as_os_str() != "--")
        .any(|argument| argument.as_os_str() == "--json");
    let args = match cli::args::Cli::try_parse_from(raw) {
        Ok(args) => args,
        Err(error) if json_requested && error.use_stderr() => {
            eprintln!(
                "{}",
                serde_json::json!({"ok":false,"error":{"code":"invalid_arguments","message":error.to_string()}})
            );
            return std::process::ExitCode::from(2);
        }
        Err(error) => error.exit(),
    };
    if matches!(args.command, cli::args::Command::Licenses) {
        let notices = include_str!("../THIRD_PARTY_NOTICES.txt");
        if args.json {
            println!("{}", serde_json::json!({"licenses": notices}));
        } else {
            print!("{notices}");
        }
        return std::process::ExitCode::SUCCESS;
    }
    execute(args)
}

#[tokio::main]
async fn execute(args: cli::args::Cli) -> std::process::ExitCode {
    let json_output = args.json;
    let daemon_run = matches!(
        &args.command,
        cli::args::Command::Daemon {
            command: cli::args::DaemonCommand::Run
        }
    );
    match cli::run(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let (code, exit) = cli::error_code(&error);
            let message = if daemon_run {
                daemon::error_message(&error)
            } else {
                format!("{error:#}")
            };
            if json_output {
                let result = cli::json_error(&error);
                eprintln!(
                    "{}",
                    result.unwrap_or_else(|| serde_json::json!({"ok": false, "error": {"code": code, "message": message}}))
                );
            } else {
                eprintln!("error: {message}");
            }
            std::process::ExitCode::from(exit)
        }
    }
}
