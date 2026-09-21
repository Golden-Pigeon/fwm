//! Verified, fenced recovery of this manager's dedicated reverse SSH sessions.
mod context;
mod error;
mod lease;

pub use context::CleanupContext;
pub use error::CleanupError;
pub use lease::RemoteLease;
