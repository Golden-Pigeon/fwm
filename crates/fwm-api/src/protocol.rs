use fwm_core::model::{
    Config, DesiredState, EngineEvent, ForwardSpec, ForwardStatus, ServerProfile,
};
pub use fwm_core::model::{OperationReport, SkippedForward};
pub use fwm_core::selection::Selection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const API_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub api_version: u32,
    pub request_id: String,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    pub command: Command,
}

impl Request {
    pub fn new(request_id: impl Into<String>, command: Command) -> Self {
        Self {
            api_version: API_VERSION,
            request_id: request_id.into(),
            expected_revision: None,
            command,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Command {
    Ping,
    GetConfig,
    PutServer {
        server: ServerProfile,
    },
    RemoveServer {
        selector: String,
    },
    PutForward {
        forward: ForwardSpec,
    },
    PutForwardWithServer {
        forward: ForwardSpec,
        #[serde(default)]
        server: Option<ServerProfile>,
    },
    PutForwardsWithServer {
        forwards: Vec<ForwardSpec>,
        #[serde(default)]
        server: Option<ServerProfile>,
    },
    /// Create a server, if provided, and all rules in one validated commit;
    /// never overwrite existing profiles or rules.
    CreateForwards {
        forwards: Vec<ForwardSpec>,
        #[serde(default)]
        server: Option<ServerProfile>,
    },
    RemoveForward {
        selector: String,
    },
    RemoveForwards {
        selection: Selection,
    },
    SetDesired {
        selection: Selection,
        state: DesiredState,
    },
    Retry {
        selection: Selection,
    },
    Restart {
        selection: Selection,
    },
    Status,
    /// Configuration and runtime data from the same mutation boundary.
    StatusView {
        #[serde(default)]
        selection: Option<Selection>,
    },
    Events {
        after: u64,
    },
    Reload,
    Validate,
    Doctor {
        server: Option<String>,
    },
    InspectHost {
        server: String,
    },
    InspectHostProfile {
        server: ServerProfile,
    },
    InspectHopProfile {
        server: ServerProfile,
        hop: String,
    },
    TrustHopProfile {
        server: ServerProfile,
        hop: String,
        fingerprint: String,
    },
    TrustHostProfile {
        server: ServerProfile,
        fingerprint: String,
    },
    DoctorProfile {
        server: ServerProfile,
    },
    TrustHost {
        server: String,
        fingerprint: String,
    },
    Shutdown,
}

impl Command {
    pub fn mutates_config(&self) -> bool {
        matches!(
            self,
            Self::PutServer { .. }
                | Self::RemoveServer { .. }
                | Self::PutForward { .. }
                | Self::PutForwardWithServer { .. }
                | Self::PutForwardsWithServer { .. }
                | Self::CreateForwards { .. }
                | Self::RemoveForward { .. }
                | Self::RemoveForwards { .. }
                | Self::SetDesired { .. }
                | Self::Restart { .. }
                | Self::Reload
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub api_version: u32,
    pub request_id: String,
    pub ok: bool,
    pub data: Value,
    pub error: Option<ApiError>,
}

impl Response {
    pub fn success(request_id: String, data: Value) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            ok: true,
            data,
            error: None,
        }
    }
    pub fn failure(request_id: String, error: ApiError) -> Self {
        Self {
            api_version: API_VERSION,
            request_id,
            ok: false,
            data: Value::Null,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub daemon_instance_id: String,
    pub config_revision: u64,
    pub forwards: Vec<ForwardStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusView {
    pub config: Config,
    pub snapshot: StatusSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventsReply {
    pub daemon_instance_id: String,
    pub latest_sequence: u64,
    pub next_sequence: u64,
    pub has_more: bool,
    pub resync_required: bool,
    pub events: Vec<EngineEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MutationReply {
    pub revision: u64,
    pub message: String,
    pub config: Config,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationReport>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_forwards_accepts_existing_clients_without_a_server_field() {
        let command: Command = serde_json::from_value(serde_json::json!({
            "method": "create_forwards",
            "params": { "forwards": [] }
        }))
        .unwrap();
        assert!(matches!(
            command,
            Command::CreateForwards { server: None, .. }
        ));
    }
}
