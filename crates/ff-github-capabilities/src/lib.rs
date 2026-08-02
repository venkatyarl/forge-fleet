//! Deliberately narrow, in-process GitHub App capabilities.
//!
//! There is no generic request, URL, ref, refspec, delete, force-push, PR
//! mutation, or child-process API in this crate.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, TimeDelta, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

const API_ORIGIN: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "ForgeFleet-GitHub-Capabilities/1";
const MAX_FILES: usize = 512;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;

/// Secret material that is zeroed on drop and cannot be cloned or formatted.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    pub fn new(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.is_empty() {
            return Err(Error::Policy("secret must not be empty"));
        }
        Ok(Self(bytes))
    }

    fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBytes([REDACTED])")
    }
}

/// Credentials for exactly one installed GitHub App.
pub struct AppCredentials {
    app_id: u64,
    installation_id: u64,
    private_key_pem: SecretBytes,
}

impl AppCredentials {
    pub fn new(
        app_id: u64,
        installation_id: u64,
        private_key_pem: SecretBytes,
    ) -> Result<Self, Error> {
        if app_id == 0 || installation_id == 0 {
            return Err(Error::Policy("GitHub App identifiers must be non-zero"));
        }
        Ok(Self {
            app_id,
            installation_id,
            private_key_pem,
        })
    }
}

/// Compile/startup-bound repository identity. It cannot contain URL syntax.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Repository {
    owner: String,
    name: String,
}

impl Repository {
    pub fn new(owner: impl Into<String>, name: impl Into<String>) -> Result<Self, Error> {
        let owner = owner.into();
        let name = name.into();
        if !valid_slug(&owner) || !valid_slug(&name) {
            return Err(Error::Policy("invalid authoritative repository"));
        }
        Ok(Self { owner, name })
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitOid([u8; 20]);

impl GitOid {
    pub fn parse(value: &str) -> Result<Self, Error> {
        if value.len() != 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::InvalidResponse("invalid full Git SHA-1"));
        }
        let mut bytes = [0; 20];
        hex::decode_to_slice(value, &mut bytes)
            .map_err(|_| Error::InvalidResponse("invalid Git SHA-1"))?;
        Ok(Self(bytes))
    }

    pub fn hex(self) -> String {
        hex::encode(self.0)
    }
}

impl TryFrom<String> for GitOid {
    type Error = Error;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<GitOid> for String {
    fn from(value: GitOid) -> Self {
        value.hex()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request rejected by capability policy")]
    Policy(&'static str),
    #[error("manifest rejected: {0}")]
    Manifest(&'static str),
    #[error("GitHub authentication failed")]
    Authentication,
    #[error("GitHub capability request failed")]
    Transport,
    #[error("GitHub returned an invalid or substituted response: {0}")]
    InvalidResponse(&'static str),
    #[error("atomic ref comparison failed")]
    Conflict,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadPr {
    pub number: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrSnapshot {
    pub repository: Repository,
    pub number: u64,
    pub base: GitOid,
    pub head: GitOid,
    pub base_ref: String,
    pub head_ref: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PushTaskBranch {
    pub work_item_id: String,
    pub base: GitOid,
    pub expected_old: Option<GitOid>,
    pub author: CommitAuthor,
    pub manifest: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAuthor {
    pub name: String,
    pub email: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    Regular,
    Executable,
    Delete,
}

impl FileMode {
    fn git(self) -> Option<&'static str> {
        match self {
            Self::Regular => Some("100644"),
            Self::Executable => Some("100755"),
            Self::Delete => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub path: String,
    pub mode: FileMode,
    #[serde(default)]
    pub bytes: Vec<u8>,
    /// Lowercase SHA-256 of `bytes`; deletes use SHA-256 of empty bytes.
    pub digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedPush {
    pub repository: Repository,
    pub base: GitOid,
    pub expected_old: Option<GitOid>,
    pub manifest_digest: String,
    pub ref_name: String,
    pub blobs: BTreeMap<String, GitOid>,
    pub tree: GitOid,
    pub commit: GitOid,
}

struct InstallationToken {
    value: SecretBytes,
    expires_at: DateTime<Utc>,
}

/// A fixed-origin GitHub transport. `reqwest` is configured without proxy
/// discovery and without redirects; rustls is selected in Cargo features.
pub struct GitHubCapabilities {
    repository: Repository,
    credentials: AppCredentials,
    client: Client,
    origin: String,
}

impl GitHubCapabilities {
    pub fn new(repository: Repository, credentials: AppCredentials) -> Result<Self, Error> {
        Self::with_origin(repository, credentials, API_ORIGIN)
    }

    fn with_origin(
        repository: Repository,
        credentials: AppCredentials,
        origin: &str,
    ) -> Result<Self, Error> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .https_only(origin == API_ORIGIN)
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self {
            repository,
            credentials,
            client,
            origin: origin.to_owned(),
        })
    }

    pub async fn read_pr(&self, request: ReadPr) -> Result<PrSnapshot, Error> {
        if request.number == 0 {
            return Err(Error::Policy("PR number must be non-zero"));
        }
        let token = self.installation_token().await?;
        let response: PrResponse = self
            .get_json(
                &format!(
                    "/repos/{}/{}/pulls/{}",
                    self.repository.owner, self.repository.name, request.number
                ),
                &token,
            )
            .await?;
        if response.number != request.number
            || response.base.repo.full_name != self.full_name()
            || response.head.repo.full_name != self.full_name()
        {
            return Err(Error::InvalidResponse("PR repository/number mismatch"));
        }
        Ok(PrSnapshot {
            repository: self.repository.clone(),
            number: response.number,
            base: GitOid::parse(&response.base.sha)?,
            head: GitOid::parse(&response.head.sha)?,
            base_ref: validate_branch(&response.base.reference)?,
            head_ref: validate_branch(&response.head.reference)?,
        })
    }

    pub async fn push_task_branch(&self, request: PushTaskBranch) -> Result<VerifiedPush, Error> {
        let canonical = CanonicalManifest::new(request.manifest)?;
        validate_author(&request.author)?;
        let task = validate_work_item(&request.work_item_id)?;
        let ref_name = format!("refs/heads/ff/task-{task}");
        let token = self.installation_token().await?;

        let base_commit = self.get_commit(request.base, &token).await?;
        if GitOid::parse(&base_commit.sha)? != request.base || base_commit.parents.len() > 2 {
            return Err(Error::InvalidResponse("base commit substitution"));
        }
        let base_tree = GitOid::parse(&base_commit.tree.sha)?;
        let base_snapshot = self.get_tree(base_tree, &token).await?;
        let remote_ref = self.get_ref_optional(&ref_name, &token).await?;
        match (request.expected_old, remote_ref) {
            (Some(expected), Some(actual)) if expected == actual => {}
            (None, None) => {}
            _ => return Err(Error::Conflict),
        }

        let mut blobs = BTreeMap::new();
        for entry in canonical
            .entries
            .values()
            .filter(|e| e.mode != FileMode::Delete)
        {
            let local = git_object_oid(b"blob", &entry.bytes);
            let body = serde_json::json!({
                "content": STANDARD.encode(&entry.bytes),
                "encoding": "base64"
            });
            let created: OidResponse = self
                .post_json(
                    &format!(
                        "/repos/{}/{}/git/blobs",
                        self.repository.owner, self.repository.name
                    ),
                    &body,
                    &token,
                )
                .await?;
            if GitOid::parse(&created.sha)? != local {
                return Err(Error::InvalidResponse("blob SHA substitution"));
            }
            blobs.insert(entry.path.clone(), local);
        }

        let tree_entries: Vec<_> = canonical
            .entries
            .values()
            .map(|entry| {
                serde_json::json!({
                    "path": entry.path,
                    "mode": entry.mode.git(),
                    "type": if entry.mode == FileMode::Delete { serde_json::Value::Null } else { serde_json::Value::String("blob".into()) },
                    "sha": blobs.get(&entry.path).map(|oid| oid.hex())
                })
            })
            .collect();
        let tree: OidResponse = self
            .post_json(
                &format!(
                    "/repos/{}/{}/git/trees",
                    self.repository.owner, self.repository.name
                ),
                &serde_json::json!({"base_tree": base_tree.hex(), "tree": tree_entries}),
                &token,
            )
            .await?;
        let tree_oid = GitOid::parse(&tree.sha)?;
        let local_tree = rebuilt_tree_oid(&base_snapshot, &canonical, &blobs)?;
        if tree_oid != local_tree {
            return Err(Error::InvalidResponse("tree SHA substitution"));
        }

        let message = format!(
            "ForgeFleet task {task}\n\nManifest-SHA256: {}",
            canonical.digest
        );
        let commit_body = serde_json::json!({
            "message": message,
            "tree": tree_oid.hex(),
            "parents": [request.base.hex()],
            "author": {
                "name": request.author.name,
                "email": request.author.email,
                "date": request.author.timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            },
            "committer": {
                "name": request.author.name,
                "email": request.author.email,
                "date": request.author.timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            }
        });
        let commit: OidResponse = self
            .post_json(
                &format!(
                    "/repos/{}/{}/git/commits",
                    self.repository.owner, self.repository.name
                ),
                &commit_body,
                &token,
            )
            .await?;
        let commit_oid = GitOid::parse(&commit.sha)?;
        let local_commit = commit_oid_for(tree_oid, request.base, &request.author, &message);
        if commit_oid != local_commit {
            return Err(Error::InvalidResponse("commit SHA substitution"));
        }
        let verified = self.get_commit(commit_oid, &token).await?;
        if GitOid::parse(&verified.sha)? != commit_oid
            || GitOid::parse(&verified.tree.sha)? != tree_oid
            || verified.parents.len() != 1
            || GitOid::parse(&verified.parents[0].sha)? != request.base
        {
            return Err(Error::InvalidResponse("commit substitution"));
        }

        self.cas_ref(&ref_name, request.expected_old, commit_oid, &token)
            .await?;
        if self.get_ref_optional(&ref_name, &token).await? != Some(commit_oid) {
            return Err(Error::InvalidResponse("ref verification failed"));
        }
        Ok(VerifiedPush {
            repository: self.repository.clone(),
            base: request.base,
            expected_old: request.expected_old,
            manifest_digest: canonical.digest,
            ref_name,
            blobs,
            tree: tree_oid,
            commit: commit_oid,
        })
    }

    async fn installation_token(&self) -> Result<InstallationToken, Error> {
        #[derive(Serialize)]
        struct Claims {
            iat: i64,
            exp: i64,
            iss: String,
        }
        let now = Utc::now();
        let claims = Claims {
            iat: (now - TimeDelta::seconds(30)).timestamp(),
            exp: (now + TimeDelta::minutes(8)).timestamp(),
            iss: self.credentials.app_id.to_string(),
        };
        let key = EncodingKey::from_rsa_pem(self.credentials.private_key_pem.expose())
            .map_err(|_| Error::Authentication)?;
        let mut jwt = jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &key)
            .map_err(|_| Error::Authentication)?;
        let result = self
            .client
            .post(format!(
                "{}/app/installations/{}/access_tokens",
                self.origin, self.credentials.installation_id
            ))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(header::USER_AGENT, USER_AGENT)
            .json(&serde_json::json!({
                "repositories": [self.repository.name],
                "permissions": {"contents": "write", "pull_requests": "read"}
            }))
            .send()
            .await
            .map_err(|_| Error::Authentication)?;
        jwt.zeroize();
        if !result.status().is_success() {
            return Err(Error::Authentication);
        }
        let body = limited_bytes(result).await?;
        let response: TokenResponse =
            serde_json::from_slice(&body).map_err(|_| Error::Authentication)?;
        if response.expires_at <= now
            || response.expires_at > now + TimeDelta::hours(1)
            || response.repositories.len() != 1
            || response.repositories[0].full_name != self.full_name()
            || response.permissions.contents.as_deref() != Some("write")
            || response.permissions.pull_requests.as_deref() != Some("read")
        {
            return Err(Error::Authentication);
        }
        Ok(InstallationToken {
            value: SecretBytes::new(response.token.into_bytes())?,
            expires_at: response.expires_at,
        })
    }

    async fn get_commit(
        &self,
        oid: GitOid,
        token: &InstallationToken,
    ) -> Result<CommitResponse, Error> {
        self.get_json(
            &format!(
                "/repos/{}/{}/git/commits/{}",
                self.repository.owner,
                self.repository.name,
                oid.hex()
            ),
            token,
        )
        .await
    }

    async fn get_tree(
        &self,
        oid: GitOid,
        token: &InstallationToken,
    ) -> Result<TreeResponse, Error> {
        let tree: TreeResponse = self
            .get_json(
                &format!(
                    "/repos/{}/{}/git/trees/{}?recursive=1",
                    self.repository.owner,
                    self.repository.name,
                    oid.hex()
                ),
                token,
            )
            .await?;
        if tree.truncated || GitOid::parse(&tree.sha)? != oid {
            return Err(Error::InvalidResponse("base tree substitution/truncation"));
        }
        Ok(tree)
    }

    async fn get_ref_optional(
        &self,
        ref_name: &str,
        token: &InstallationToken,
    ) -> Result<Option<GitOid>, Error> {
        let suffix = ref_name
            .strip_prefix("refs/")
            .ok_or(Error::Policy("invalid derived ref"))?;
        let response = self
            .request(
                self.client.get(format!(
                    "{}/repos/{}/{}/git/ref/{}",
                    self.origin, self.repository.owner, self.repository.name, suffix
                )),
                token,
            )?
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Error::Transport);
        }
        let value: RefResponse = serde_json::from_slice(&limited_bytes(response).await?)
            .map_err(|_| Error::InvalidResponse("invalid ref response"))?;
        if value.reference != ref_name || value.object.kind != "commit" {
            return Err(Error::InvalidResponse("ref substitution"));
        }
        Ok(Some(GitOid::parse(&value.object.sha)?))
    }

    async fn cas_ref(
        &self,
        ref_name: &str,
        expected: Option<GitOid>,
        commit: GitOid,
        token: &InstallationToken,
    ) -> Result<(), Error> {
        let response = if expected.is_some() {
            let suffix = ref_name.strip_prefix("refs/").ok_or(Error::Policy("ref"))?;
            self.request(
                self.client.patch(format!(
                    "{}/repos/{}/{}/git/refs/{}",
                    self.origin, self.repository.owner, self.repository.name, suffix
                )),
                token,
            )?
            .json(&serde_json::json!({"sha": commit.hex(), "force": false}))
            .send()
            .await
        } else {
            self.request(
                self.client.post(format!(
                    "{}/repos/{}/{}/git/refs",
                    self.origin, self.repository.owner, self.repository.name
                )),
                token,
            )?
            .json(&serde_json::json!({"ref": ref_name, "sha": commit.hex()}))
            .send()
            .await
        }
        .map_err(|_| Error::Transport)?;
        if matches!(
            response.status(),
            StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY
        ) {
            return Err(Error::Conflict);
        }
        if !response.status().is_success() {
            return Err(Error::Transport);
        }
        Ok(())
    }

    fn request(
        &self,
        builder: reqwest::RequestBuilder,
        token: &InstallationToken,
    ) -> Result<reqwest::RequestBuilder, Error> {
        debug_assert!(token.expires_at > Utc::now());
        let mut bearer = b"Bearer ".to_vec();
        bearer.extend_from_slice(token.value.expose());
        let authorization =
            header::HeaderValue::from_bytes(&bearer).map_err(|_| Error::Authentication)?;
        bearer.zeroize();
        Ok(builder
            .header(header::AUTHORIZATION, authorization)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header(header::USER_AGENT, USER_AGENT))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        token: &InstallationToken,
    ) -> Result<T, Error> {
        let response = self
            .request(self.client.get(format!("{}{path}", self.origin)), token)?
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        parse_success(response).await
    }

    async fn post_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: &InstallationToken,
    ) -> Result<T, Error> {
        let response = self
            .request(self.client.post(format!("{}{path}", self.origin)), token)?
            .json(body)
            .send()
            .await
            .map_err(|_| Error::Transport)?;
        parse_success(response).await
    }

    fn full_name(&self) -> String {
        format!("{}/{}", self.repository.owner, self.repository.name)
    }
}

async fn limited_bytes(response: reqwest::Response) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES)
    {
        return Err(Error::InvalidResponse("oversized response"));
    }
    let bytes = response.bytes().await.map_err(|_| Error::Transport)?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(Error::InvalidResponse("oversized response"));
    }
    Ok(bytes.to_vec())
}

async fn parse_success<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<T, Error> {
    if !response.status().is_success() {
        return Err(Error::Transport);
    }
    serde_json::from_slice(&limited_bytes(response).await?)
        .map_err(|_| Error::InvalidResponse("malformed JSON response"))
}

struct CanonicalManifest {
    entries: BTreeMap<String, ManifestEntry>,
    digest: String,
}

impl CanonicalManifest {
    fn new(entries: Vec<ManifestEntry>) -> Result<Self, Error> {
        if entries.is_empty() || entries.len() > MAX_FILES {
            return Err(Error::Manifest("invalid file count"));
        }
        let mut result = BTreeMap::new();
        let mut aliases = BTreeSet::new();
        let mut total = 0usize;
        for entry in entries {
            validate_path(&entry.path)?;
            let alias = entry.path.to_ascii_lowercase();
            if !aliases.insert(alias) || result.contains_key(&entry.path) {
                return Err(Error::Manifest("duplicate or portable path alias"));
            }
            if entry.mode == FileMode::Delete && !entry.bytes.is_empty() {
                return Err(Error::Manifest("delete carries bytes"));
            }
            if entry.bytes.len() > MAX_FILE_BYTES {
                return Err(Error::Manifest("file too large"));
            }
            total = total
                .checked_add(entry.bytes.len())
                .ok_or(Error::Manifest("total size overflow"))?;
            if total > MAX_TOTAL_BYTES {
                return Err(Error::Manifest("manifest too large"));
            }
            let actual = hex::encode(Sha256::digest(&entry.bytes));
            if entry.digest.len() != 64 || entry.digest != actual {
                return Err(Error::Manifest("byte digest mismatch"));
            }
            result.insert(entry.path.clone(), entry);
        }
        let canonical = serde_json::to_vec(
            &result
                .values()
                .map(|e| (&e.path, e.mode, &e.digest, e.bytes.len()))
                .collect::<Vec<_>>(),
        )
        .map_err(|_| Error::Manifest("canonicalization failed"))?;
        Ok(Self {
            entries: result,
            digest: hex::encode(Sha256::digest(canonical)),
        })
    }
}

fn validate_path(path: &str) -> Result<(), Error> {
    if path.is_empty()
        || path.len() > 4096
        || !path.is_ascii()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.bytes().any(|b| b == 0 || b < 0x20 || b == 0x7f)
    {
        return Err(Error::Manifest("non-portable path"));
    }
    for part in path.split('/') {
        let trimmed = part.trim_end_matches([' ', '.']);
        let stem = trimmed.split('.').next().unwrap_or_default();
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.eq_ignore_ascii_case(".git")
            || trimmed != part
            || matches!(
                stem.to_ascii_uppercase().as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
        {
            return Err(Error::Manifest("unsafe path component"));
        }
    }
    Ok(())
}

fn validate_work_item(value: &str) -> Result<&str, Error> {
    if value.is_empty()
        || value.len() > 80
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        return Err(Error::Policy("invalid work item id"));
    }
    Ok(value)
}

fn validate_author(author: &CommitAuthor) -> Result<(), Error> {
    if author.name.is_empty()
        || author.name.len() > 100
        || author.email.is_empty()
        || author.email.len() > 254
        || !author.email.ends_with("@forgefleet.invalid")
        || author.name.bytes().any(|b| b < 0x20 || b == 0x7f)
        || author.email.bytes().any(|b| b <= 0x20 || b == 0x7f)
        || author.timestamp.timestamp_subsec_nanos() != 0
    {
        return Err(Error::Policy("invalid deterministic author policy"));
    }
    Ok(())
}

fn validate_branch(value: &str) -> Result<String, Error> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("@{")
        || value.bytes().any(|b| b <= 0x20 || b == 0x7f)
        || value.contains(['~', '^', ':', '?', '*', '[', '\\'])
    {
        return Err(Error::InvalidResponse("invalid branch name"));
    }
    Ok(value.to_owned())
}

fn git_object_oid(kind: &[u8], body: &[u8]) -> GitOid {
    let mut hash = Sha1::new();
    hash.update(kind);
    hash.update(b" ");
    hash.update(body.len().to_string().as_bytes());
    hash.update([0]);
    hash.update(body);
    GitOid(hash.finalize().into())
}

#[derive(Default)]
struct TreeNode {
    files: BTreeMap<String, (String, GitOid)>,
    dirs: BTreeMap<String, TreeNode>,
}

fn rebuilt_tree_oid(
    base: &TreeResponse,
    manifest: &CanonicalManifest,
    blobs: &BTreeMap<String, GitOid>,
) -> Result<GitOid, Error> {
    let mut root = TreeNode::default();
    for entry in &base.tree {
        if entry.kind == "tree" {
            continue;
        }
        validate_path(&entry.path).map_err(|_| Error::InvalidResponse("unsafe base tree path"))?;
        if !matches!(
            entry.mode.as_str(),
            "100644" | "100755" | "120000" | "160000"
        ) {
            return Err(Error::InvalidResponse("unsupported base tree mode"));
        }
        insert_tree_path(
            &mut root,
            &entry.path,
            Some((entry.mode.clone(), GitOid::parse(&entry.sha)?)),
        )?;
    }
    for entry in manifest.entries.values() {
        let value = match entry.mode.git() {
            Some(mode) => Some((
                mode.to_owned(),
                *blobs
                    .get(&entry.path)
                    .ok_or(Error::InvalidResponse("missing verified blob"))?,
            )),
            None => None,
        };
        insert_tree_path(&mut root, &entry.path, value)?;
    }
    Ok(hash_tree(&root))
}

fn insert_tree_path(
    root: &mut TreeNode,
    path: &str,
    value: Option<(String, GitOid)>,
) -> Result<(), Error> {
    let mut parts = path.split('/').peekable();
    let mut node = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if value.is_some() && node.dirs.contains_key(part) {
                return Err(Error::Manifest("file/directory collision"));
            }
            match value {
                Some(file) => {
                    node.files.insert(part.to_owned(), file);
                }
                None => {
                    node.files.remove(part);
                }
            }
            return Ok(());
        }
        if node.files.contains_key(part) {
            return Err(Error::Manifest("file/directory collision"));
        }
        node = node.dirs.entry(part.to_owned()).or_default();
    }
    Err(Error::Manifest("empty path"))
}

fn hash_tree(node: &TreeNode) -> GitOid {
    let mut entries: Vec<(Vec<u8>, &str, GitOid)> = node
        .files
        .iter()
        .map(|(name, (mode, oid))| (name.as_bytes().to_vec(), mode.as_str(), *oid))
        .collect();
    for (name, child) in &node.dirs {
        if tree_is_empty(child) {
            continue;
        }
        let mut sort_name = name.as_bytes().to_vec();
        sort_name.push(b'/');
        entries.push((sort_name, "40000", hash_tree(child)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (sort_name, mode, oid) in entries {
        body.extend_from_slice(mode.as_bytes());
        body.push(b' ');
        body.extend_from_slice(sort_name.strip_suffix(b"/").unwrap_or(&sort_name));
        body.push(0);
        body.extend_from_slice(&oid.0);
    }
    git_object_oid(b"tree", &body)
}

fn tree_is_empty(node: &TreeNode) -> bool {
    node.files.is_empty() && node.dirs.values().all(tree_is_empty)
}

fn commit_oid_for(tree: GitOid, parent: GitOid, author: &CommitAuthor, message: &str) -> GitOid {
    let timestamp = author.timestamp.timestamp();
    let identity = format!("{} <{}> {timestamp} +0000", author.name, author.email);
    let body = format!(
        "tree {}\nparent {}\nauthor {identity}\ncommitter {identity}\n\n{message}",
        tree.hex(),
        parent.hex()
    );
    git_object_oid(b"commit", body.as_bytes())
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    repositories: Vec<RepoResponse>,
    #[serde(default)]
    permissions: PermissionResponse,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionResponse {
    contents: Option<String>,
    pull_requests: Option<String>,
}

#[derive(Deserialize)]
struct RepoResponse {
    full_name: String,
}

#[derive(Deserialize)]
struct PrResponse {
    number: u64,
    base: PrSide,
    head: PrSide,
}

#[derive(Deserialize)]
struct PrSide {
    sha: String,
    #[serde(rename = "ref")]
    reference: String,
    repo: RepoResponse,
}

#[derive(Deserialize)]
struct OidResponse {
    sha: String,
}

#[derive(Deserialize)]
struct TreeResponse {
    sha: String,
    #[serde(default)]
    truncated: bool,
    tree: Vec<TreeEntryResponse>,
}

#[derive(Deserialize)]
struct TreeEntryResponse {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
    tree: OidResponse,
    parents: Vec<OidResponse>,
}

#[derive(Deserialize)]
struct RefResponse {
    #[serde(rename = "ref")]
    reference: String,
    object: RefObject,
}

#[derive(Deserialize)]
struct RefObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn hashes_real_git_blob() {
        assert_eq!(
            git_object_oid(b"blob", b"hello\n").hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn manifest_is_sorted_bounded_and_rejects_aliases_and_git() {
        let good = ManifestEntry {
            path: "src/main.rs".into(),
            mode: FileMode::Regular,
            bytes: b"fn main() {}\n".to_vec(),
            digest: digest(b"fn main() {}\n"),
        };
        assert!(CanonicalManifest::new(vec![good]).is_ok());
        for path in [
            "../x",
            "a/../../x",
            ".git/config",
            "a/.GIT/x",
            "x\\y",
            "con.txt",
            "x.",
            "/x",
            "x//y",
        ] {
            let entry = ManifestEntry {
                path: path.into(),
                mode: FileMode::Regular,
                bytes: vec![],
                digest: digest(&[]),
            };
            assert!(CanonicalManifest::new(vec![entry]).is_err(), "{path}");
        }
    }

    #[test]
    fn opaque_secrets_never_debug_value() {
        let secret = SecretBytes::new(b"LEAK-SENTINEL".to_vec()).unwrap();
        assert_eq!(format!("{secret:?}"), "SecretBytes([REDACTED])");
    }

    #[test]
    fn rejects_truncated_and_synthetic_sha() {
        assert!(GitOid::parse("deadbeef").is_err());
        assert!(GitOid::parse(&"z".repeat(40)).is_err());
    }
}
