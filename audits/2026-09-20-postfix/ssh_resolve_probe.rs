//! Read-only SSH resolver probe. Build against the existing fwm-core rlib.
use fwm_core::{model::ServerProfile, ssh};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mut profile = ServerProfile::new("audit");
    profile.ssh_alias = Some(args[1].clone());
    profile.ssh_config = Some(args[2].clone().into());
    match ssh::resolve(&profile) {
        Ok(resolved) => println!("{resolved:#?}"),
        Err(error) => { eprintln!("{error}"); std::process::exit(2); }
    }
}
