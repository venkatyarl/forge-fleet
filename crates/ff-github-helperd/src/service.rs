use std::{collections::HashSet, sync::Mutex, time::SystemTime};

use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    git::manifest_digest,
    protocol::{Envelope, Operation, PROTOCOL_VERSION, PullRequest},
};

const NONCE_BYTES: usize = 32;
const SHA1_HEX: usize = 40;
const SHA256_HEX: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    pub uid: u32,
    pub gid: u32,
    pub executable_sha256: String,
}

#[derive(Clone, Debug)]
pub struct Authority {
    pub work_item_id: Uuid,
    pub repo_id: Uuid,
    pub owner: String,
    pub repository: String,
    pub default_branch: String,
    pub base_sha: String,
    pub nonce: String,
    pub nonce_expires_at: SystemTime,
}

pub trait AuthorityStore: Send + Sync {
    fn resolve(&self, work_item_id: Uuid) -> Result<Authority, ServiceError>;
}

pub trait GitHubTransport: Send + Sync {
    fn read_pr(
        &self,
        owner: &str,
        repository: &str,
        number: u64,
    ) -> Result<PullRequest, ServiceError>;

    fn remote_ref_sha(
        &self,
        owner: &str,
        repository: &str,
        ref_name: &str,
    ) -> Result<Option<String>, ServiceError>;

    /// Uploads already reconstructed Git objects and updates exactly `ref_name`.
    /// The implementation owns the in-memory installation token; it must not
    /// delegate authentication to Git or another child process.
    fn push_exact(
        &self,
        owner: &str,
        repository: &str,
        ref_name: &str,
        expected_old_sha: &str,
        new_sha: &str,
    ) -> Result<(), ServiceError>;
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ServiceError {
    #[error("invalid request")]
    InvalidRequest,
    #[error("scheduler peer identity mismatch")]
    UnauthorizedPeer,
    #[error("request does not match authoritative work-item binding")]
    AuthorityMismatch,
    #[error("nonce is expired, unknown, or already consumed")]
    Replay,
    #[error("remote ref no longer has the expected object id")]
    StaleSha,
    #[error("operation targets a protected base")]
    ProtectedBase,
    #[error("invalid structural manifest")]
    InvalidManifest,
    #[error("capability backend unavailable")]
    BackendUnavailable,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ServiceResult {
    PullRequest(PullRequest),
    Pushed {
        ref_name: String,
        commit_sha: String,
    },
}

pub struct CapabilityService<A, G> {
    allowed_peer: PeerIdentity,
    authority: A,
    github: G,
    used_nonces: Mutex<HashSet<[u8; NONCE_BYTES]>>,
}

impl<A: AuthorityStore, G: GitHubTransport> CapabilityService<A, G> {
    pub fn new(allowed_peer: PeerIdentity, authority: A, github: G) -> Self {
        Self {
            allowed_peer,
            authority,
            github,
            used_nonces: Mutex::new(HashSet::new()),
        }
    }

    pub fn execute(
        &self,
        peer: &PeerIdentity,
        now: SystemTime,
        request: Envelope,
    ) -> Result<ServiceResult, ServiceError> {
        if peer != &self.allowed_peer {
            return Err(ServiceError::UnauthorizedPeer);
        }
        validate_envelope(&request)?;
        let authority = self.authority.resolve(request.work_item_id)?;
        validate_binding(&request, &authority, now)?;
        self.consume_nonce(&request.nonce)?;

        match request.operation {
            Operation::ReadPr { number } => {
                if number == 0 {
                    return Err(ServiceError::InvalidRequest);
                }
                self.github
                    .read_pr(&authority.owner, &authority.repository, number)
                    .map(ServiceResult::PullRequest)
            }
            Operation::PushTaskBranch { manifest } => {
                let digest =
                    manifest_digest(&manifest).map_err(|_| ServiceError::InvalidManifest)?;
                if !constant_time_eq(digest.as_bytes(), request.bundle_digest.as_bytes()) {
                    return Err(ServiceError::AuthorityMismatch);
                }
                let ref_name = task_ref(request.work_item_id);
                if ref_name == format!("refs/heads/{}", authority.default_branch) {
                    return Err(ServiceError::ProtectedBase);
                }
                let remote = self.github.remote_ref_sha(
                    &authority.owner,
                    &authority.repository,
                    &ref_name,
                )?;
                let actual = remote.unwrap_or_else(zero_sha);
                if actual != request.expected_remote_sha {
                    return Err(ServiceError::StaleSha);
                }
                // The content-address is deterministic and is the only candidate
                // identifier exposed to the in-process authenticated transport.
                let commit_sha = synthetic_commit_sha(&authority.base_sha, &digest);
                self.github.push_exact(
                    &authority.owner,
                    &authority.repository,
                    &ref_name,
                    &request.expected_remote_sha,
                    &commit_sha,
                )?;
                Ok(ServiceResult::Pushed {
                    ref_name,
                    commit_sha,
                })
            }
        }
    }

    fn consume_nonce(&self, nonce: &str) -> Result<(), ServiceError> {
        let raw = hex::decode(nonce).map_err(|_| ServiceError::InvalidRequest)?;
        let key: [u8; NONCE_BYTES] = raw.try_into().map_err(|_| ServiceError::InvalidRequest)?;
        let mut used = self
            .used_nonces
            .lock()
            .map_err(|_| ServiceError::BackendUnavailable)?;
        if !used.insert(key) {
            return Err(ServiceError::Replay);
        }
        Ok(())
    }
}

fn validate_envelope(request: &Envelope) -> Result<(), ServiceError> {
    if request.version != PROTOCOL_VERSION
        || !lower_hex(&request.nonce, SHA256_HEX)
        || !lower_hex(&request.base_sha, SHA1_HEX)
        || !lower_hex(&request.expected_remote_sha, SHA1_HEX)
        || !lower_hex(&request.bundle_digest, SHA256_HEX)
    {
        return Err(ServiceError::InvalidRequest);
    }
    Ok(())
}

fn validate_binding(
    request: &Envelope,
    authority: &Authority,
    now: SystemTime,
) -> Result<(), ServiceError> {
    if request.work_item_id != authority.work_item_id
        || request.repo_id != authority.repo_id
        || request.base_sha != authority.base_sha
        || !constant_time_eq(request.nonce.as_bytes(), authority.nonce.as_bytes())
    {
        return Err(ServiceError::AuthorityMismatch);
    }
    if now > authority.nonce_expires_at {
        return Err(ServiceError::Replay);
    }
    if !valid_github_component(&authority.owner)
        || !valid_github_component(&authority.repository)
        || !valid_branch(&authority.default_branch)
    {
        return Err(ServiceError::AuthorityMismatch);
    }
    Ok(())
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_github_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

fn valid_branch(value: &str) -> bool {
    valid_github_component(value)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

pub fn task_ref(work_item_id: Uuid) -> String {
    format!("refs/heads/ff/task-{work_item_id}")
}

fn zero_sha() -> String {
    "0".repeat(SHA1_HEX)
}

fn synthetic_commit_sha(base_sha: &str, digest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"ff-github-helperd-v1\0");
    hasher.update(base_sha);
    hasher.update(b"\0");
    hasher.update(digest);
    hex::encode(hasher.finalize())[..SHA1_HEX].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FileMode, ManifestEntry, StructuralManifest};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Clone)]
    struct FakeAuthority(Authority);
    impl AuthorityStore for FakeAuthority {
        fn resolve(&self, _: Uuid) -> Result<Authority, ServiceError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeGitHub {
        remote: Option<String>,
        pushes: Mutex<Vec<(String, String, String)>>,
    }
    impl GitHubTransport for FakeGitHub {
        fn read_pr(&self, _: &str, _: &str, number: u64) -> Result<PullRequest, ServiceError> {
            Ok(PullRequest {
                number,
                state: crate::protocol::PullRequestState::Open,
                head_sha: "4".repeat(40),
                base_sha: "1".repeat(40),
                title: "safe".into(),
            })
        }
        fn remote_ref_sha(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Option<String>, ServiceError> {
            Ok(self.remote.clone())
        }
        fn push_exact(
            &self,
            _: &str,
            _: &str,
            ref_name: &str,
            old: &str,
            new: &str,
        ) -> Result<(), ServiceError> {
            self.pushes
                .lock()
                .unwrap()
                .push((ref_name.into(), old.into(), new.into()));
            Ok(())
        }
    }

    fn fixture() -> (PeerIdentity, Authority, Envelope) {
        let work_item_id = Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
        let repo_id = Uuid::parse_str("22345678-1234-1234-1234-123456789abc").unwrap();
        let nonce = "a".repeat(64);
        let manifest = StructuralManifest {
            entries: vec![ManifestEntry {
                path: "src/lib.rs".into(),
                mode: FileMode::Regular,
                bytes: b"pub fn safe() {}".to_vec(),
            }],
        };
        let digest = manifest_digest(&manifest).unwrap();
        let peer = PeerIdentity {
            uid: 991,
            gid: 991,
            executable_sha256: "b".repeat(64),
        };
        let authority = Authority {
            work_item_id,
            repo_id,
            owner: "forgefleet".into(),
            repository: "project".into(),
            default_branch: "develop".into(),
            base_sha: "1".repeat(40),
            nonce: nonce.clone(),
            nonce_expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(100),
        };
        let request = Envelope {
            version: PROTOCOL_VERSION,
            nonce,
            work_item_id,
            repo_id,
            base_sha: "1".repeat(40),
            expected_remote_sha: "0".repeat(40),
            bundle_digest: digest,
            operation: Operation::PushTaskBranch { manifest },
        };
        (peer, authority, request)
    }

    #[test]
    fn allows_only_derived_task_ref_and_exact_lease() {
        let (peer, authority, request) = fixture();
        let service = CapabilityService::new(
            peer.clone(),
            FakeAuthority(authority),
            FakeGitHub::default(),
        );
        let result = service
            .execute(
                &peer,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1),
                request,
            )
            .unwrap();
        let ServiceResult::Pushed { ref_name, .. } = result else {
            panic!("expected push")
        };
        assert_eq!(
            ref_name,
            "refs/heads/ff/task-12345678-1234-1234-1234-123456789abc"
        );
    }

    #[test]
    fn rejects_peer_substitution_and_replay() {
        let (peer, authority, request) = fixture();
        let service = CapabilityService::new(
            peer.clone(),
            FakeAuthority(authority),
            FakeGitHub::default(),
        );
        let mut impostor = peer.clone();
        impostor.uid += 1;
        assert_eq!(
            service.execute(&impostor, SystemTime::UNIX_EPOCH, request.clone()),
            Err(ServiceError::UnauthorizedPeer)
        );
        service
            .execute(&peer, SystemTime::UNIX_EPOCH, request.clone())
            .unwrap();
        assert_eq!(
            service.execute(&peer, SystemTime::UNIX_EPOCH, request),
            Err(ServiceError::Replay)
        );
    }

    #[test]
    fn every_authority_binding_is_enforced() {
        let (peer, authority, request) = fixture();
        let mutations: Vec<Box<dyn Fn(&mut Envelope)>> = vec![
            Box::new(|r| r.work_item_id = Uuid::new_v4()),
            Box::new(|r| r.repo_id = Uuid::new_v4()),
            Box::new(|r| r.base_sha = "2".repeat(40)),
            Box::new(|r| r.nonce = "c".repeat(64)),
        ];
        for mutate in mutations {
            let service = CapabilityService::new(
                peer.clone(),
                FakeAuthority(authority.clone()),
                FakeGitHub::default(),
            );
            let mut changed = request.clone();
            mutate(&mut changed);
            assert!(
                service
                    .execute(&peer, SystemTime::UNIX_EPOCH, changed)
                    .is_err()
            );
        }
    }

    #[test]
    fn stale_remote_sha_and_digest_substitution_are_denied() {
        let (peer, authority, request) = fixture();
        let service = CapabilityService::new(
            peer.clone(),
            FakeAuthority(authority.clone()),
            FakeGitHub {
                remote: Some("9".repeat(40)),
                ..Default::default()
            },
        );
        assert_eq!(
            service.execute(&peer, SystemTime::UNIX_EPOCH, request.clone()),
            Err(ServiceError::StaleSha)
        );

        let service = CapabilityService::new(
            peer.clone(),
            FakeAuthority(authority),
            FakeGitHub::default(),
        );
        let mut substituted = request;
        substituted.bundle_digest = "f".repeat(64);
        assert_eq!(
            service.execute(&peer, SystemTime::UNIX_EPOCH, substituted),
            Err(ServiceError::AuthorityMismatch)
        );
    }

    #[test]
    fn hostile_authority_is_denied() {
        for base in ["../main", "refs/heads/main"] {
            let (peer, mut authority, request) = fixture();
            authority.default_branch = base.into();
            let service = CapabilityService::new(
                peer.clone(),
                FakeAuthority(authority),
                FakeGitHub::default(),
            );
            assert_eq!(
                service.execute(&peer, SystemTime::UNIX_EPOCH, request),
                Err(ServiceError::AuthorityMismatch)
            );
        }
    }
}
