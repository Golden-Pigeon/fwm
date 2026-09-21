// Read-only configuration resolver; never opens an SSH transport or agent.
use fwm_core::{model::ServerProfile, ssh};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mut profile = ServerProfile::new("audit");
    profile.ssh_alias = Some(args[1].clone());
    if args.get(3).is_some_and(|mode| mode == "host") {
        profile.host = profile.ssh_alias.take();
    }
    profile.ssh_config = Some(args[2].clone().into());
    match ssh::resolved_route(&profile) {
        Ok(route) => println!("{}", serde_json::to_string(&route).unwrap()),
        Err(error) => { println!("{}", serde_json::json!({"error":error.to_string()})); std::process::exit(2); }
    }
}
