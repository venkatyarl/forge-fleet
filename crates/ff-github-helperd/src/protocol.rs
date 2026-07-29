use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Issue {
        version: u16,
        work_item_id: Uuid,
        repo_id: Uuid,
        operation: Operation,
        request_digest: String,
    },
    Execute {
        version: u16,
        nonce: String,
        work_item_id: Uuid,
        repo_id: Uuid,
        operation: Operation,
        request_digest: String,
    },
    Ready {
        version: u16,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ReadPr { number: u64 },
    PushTaskBranch { manifest_digest: String },
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::ReadPr { .. } => "read_pr",
            Self::PushTaskBranch { .. } => "push_task_branch",
        }
    }

    pub fn bound_value(&self) -> String {
        match self {
            Self::ReadPr { number } => number.to_string(),
            Self::PushTaskBranch { manifest_digest } => manifest_digest.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Ready,
    Issued { nonce: String, expires_at: String },
    Denied { code: DenialCode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    InvalidRequest,
    UnauthorizedPeer,
    AuthorityMismatch,
    Replay,
    Expired,
    BackendUnavailable,
    DeadlineExceeded,
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn validate(request: &Request) -> bool {
    match request {
        Request::Ready { version } => *version == PROTOCOL_VERSION,
        Request::Issue {
            version,
            operation,
            request_digest,
            ..
        }
        | Request::Execute {
            version,
            operation,
            request_digest,
            ..
        } => {
            *version == PROTOCOL_VERSION
                && valid_digest(request_digest)
                && match operation {
                    Operation::ReadPr { number } => *number > 0,
                    Operation::PushTaskBranch { manifest_digest } => valid_digest(manifest_digest),
                }
        }
    }
}
