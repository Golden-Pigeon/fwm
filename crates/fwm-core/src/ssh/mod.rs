//! Native SSH transport, configuration, authentication, and explicit host trust.
//!
//! Forwarding policy lives in the engine; this module never accepts an unknown
//! host key during an authenticated connection.
mod auth;
mod config;
mod connection;
mod error;
mod path_options;
#[cfg(test)]
mod tests;
mod trust;

pub use auth::agent_socket;
pub use config::{ResolvedServer, resolve};
pub use connection::{
    check_connection, connect, inspect_hop_key, inspect_host_key, resolved_route,
};
pub use error::SshError;
pub use path_options::{
    equivalent_paths, normalize_config_paths, normalize_profile_paths, normalized_path,
};
pub use trust::{HostKeyInfo, trust_host_key, verify_host_key};
