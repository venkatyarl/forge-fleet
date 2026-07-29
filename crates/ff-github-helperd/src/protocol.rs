use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_MANIFEST_ENTRIES: usize = 20_000;
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u16,
    pub nonce: String,
    pub work_item_id: Uuid,
    pub repo_id: Uuid,
    pub base_sha: String,
    pub expected_remote_sha: String,
    pub bundle_digest: String,
    pub operation: Operation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ReadPr { number: u64 },
    PushTaskBranch { manifest: StructuralManifest },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuralManifest {
    pub entries: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub path: String,
    pub mode: FileMode,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    Regular,
    Executable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    PullRequest(PullRequest),
    Pushed {
        ref_name: String,
        commit_sha: String,
    },
    Denied {
        code: DenialCode,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequest {
    pub number: u64,
    pub state: PullRequestState,
    pub head_sha: String,
    pub base_sha: String,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenialCode {
    InvalidRequest,
    UnauthorizedPeer,
    AuthorityMismatch,
    Replay,
    StaleSha,
    ProtectedBase,
    InvalidManifest,
    TransportFailure,
}

pub fn decode_frame(frame: &[u8]) -> Result<Envelope, serde_json::Error> {
    if frame.len() > MAX_FRAME_BYTES {
        return serde_json::from_slice(b"");
    }
    serde_json::from_slice(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_are_rejected() {
        let raw = br#"{"version":1,"nonce":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","work_item_id":"12345678-1234-1234-1234-123456789abc","repo_id":"22345678-1234-1234-1234-123456789abc","base_sha":"1111111111111111111111111111111111111111","expected_remote_sha":"2222222222222222222222222222222222222222","bundle_digest":"3333333333333333333333333333333333333333333333333333333333333333","operation":{"type":"read_pr","number":7},"url":"https://attacker.invalid"}"#;
        assert!(decode_frame(raw).is_err());
    }

    #[test]
    fn oversized_frames_are_rejected() {
        assert!(decode_frame(&vec![b'x'; MAX_FRAME_BYTES + 1]).is_err());
    }
}
