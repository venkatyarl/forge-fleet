//! Deterministic, no-follow integrity verification for model artifacts.
//!
//! Callers provide an approved absolute library root and a relative artifact
//! path. On Unix every path component is opened descriptor-relatively with
//! no-follow semantics. Linux uses `openat2(RESOLVE_BENEATH | NO_SYMLINKS |
//! NO_XDEV)` for descendants when available; the fallback opens exactly one
//! component at a time and verifies the device after each open.
//!
//! A userspace scan is not an atomic filesystem snapshot. Before returning,
//! this verifier re-resolves the approved root and target and revalidates every
//! observed file and directory. It fails on every mutation observable before
//! that final pass. Callers must consume a fresh result immediately in the
//! fenced database comparison/CAS operation; they must not cache or replay it.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Domain separator for deterministic directory manifests.
pub const DIRECTORY_DIGEST_DOMAIN: &[u8] = b"ff-dir-v1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelIntegrityLimits {
    /// Maximum observed descendants. Both files and directories count, so a
    /// forest of empty directories cannot bypass the allocation/work bound.
    pub max_files: u64,
    pub max_bytes: u64,
    pub max_depth: u32,
    pub timeout: Duration,
}

impl Default for ModelIntegrityLimits {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_bytes: 2 * 1024 * 1024 * 1024 * 1024,
            max_depth: 32,
            timeout: Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelArtifactKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A point-in-time digest returned only after the final identity pass.
///
/// `entries` includes regular files and directories beneath the target;
/// `files` includes only content-bearing regular files.
pub struct ModelIntegrityDigest {
    pub kind: ModelArtifactKind,
    pub algorithm: &'static str,
    pub sha256: String,
    pub files: u64,
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelIntegrityError {
    #[error("model integrity verification is forbidden on Vinny")]
    VinnyExcluded,
    #[error("approved model root must be an absolute directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("artifact path must be relative and contain only normal components: {0}")]
    InvalidRelativePath(PathBuf),
    #[error("model integrity verification requires descriptor-relative Unix filesystem APIs")]
    UnsupportedPlatform,
    #[error("unsupported filesystem object at {path}: {kind}")]
    UnsupportedType { path: PathBuf, kind: &'static str },
    #[error("artifact crosses a filesystem boundary at {0}")]
    CrossDevice(PathBuf),
    #[error("hard-linked model file is not accepted at {path} (links={links})")]
    HardLink { path: PathBuf, links: u64 },
    #[error("artifact mutated while being verified: {0}")]
    Mutated(PathBuf),
    #[error("model verification {limit} limit exceeded")]
    LimitExceeded { limit: &'static str },
    #[error("model verification timed out")]
    Timeout,
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type ModelIntegrityResult<T> = Result<T, ModelIntegrityError>;

/// Central deny predicate shared by target selection, execution, and storage.
pub fn model_integrity_worker_allowed(worker_name: &str) -> bool {
    !worker_name.trim().eq_ignore_ascii_case("vinny")
}

/// Parse a SHA-256 hex string without accepting prefixes or truncated values.
pub fn parse_sha256_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        out[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(out)
}

/// Compare canonical or legacy-case SHA-256 hex values in constant time.
pub fn constant_time_sha256_eq(left: &str, right: &str) -> bool {
    let Some(left) = parse_sha256_hex(left) else {
        return false;
    };
    let Some(right) = parse_sha256_hex(right) else {
        return false;
    };
    left.ct_eq(&right).into()
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Verify one model artifact beneath an approved root.
///
/// The worker argument is mandatory so the Vinny exclusion is enforced at the
/// execution boundary even if a future selector is implemented incorrectly.
pub fn verify_model_path(
    worker_name: &str,
    approved_root: &Path,
    relative_path: &Path,
    limits: ModelIntegrityLimits,
) -> ModelIntegrityResult<ModelIntegrityDigest> {
    if !model_integrity_worker_allowed(worker_name) {
        return Err(ModelIntegrityError::VinnyExcluded);
    }
    validate_paths(approved_root, relative_path)?;

    #[cfg(unix)]
    {
        unix::verify(approved_root, relative_path, limits)
    }
    #[cfg(not(unix))]
    {
        let _ = limits;
        Err(ModelIntegrityError::UnsupportedPlatform)
    }
}

fn validate_paths(root: &Path, relative: &Path) -> ModelIntegrityResult<()> {
    if !root.is_absolute()
        || root.components().any(|part| {
            matches!(
                part,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(ModelIntegrityError::InvalidRoot(root.to_path_buf()));
    }
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(ModelIntegrityError::InvalidRelativePath(
            relative.to_path_buf(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::fs::{File, Metadata};
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;

    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat, openat, statat};
    #[cfg(target_os = "linux")]
    use rustix::fs::{ResolveFlags, openat2};
    use rustix::io::{Errno, dup};

    const OPEN_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::CLOEXEC)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::NONBLOCK);
    const OPEN_DIR_FLAGS: OFlags = OPEN_FLAGS.union(OFlags::DIRECTORY);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Snapshot {
        dev: u64,
        ino: u64,
        size: u64,
        mtime: i64,
        mtime_nsec: i64,
        ctime: i64,
        ctime_nsec: i64,
    }

    impl Snapshot {
        fn capture(metadata: &Metadata) -> Self {
            Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
                size: metadata.size(),
                mtime: metadata.mtime(),
                mtime_nsec: metadata.mtime_nsec(),
                ctime: metadata.ctime(),
                ctime_nsec: metadata.ctime_nsec(),
            }
        }

        fn capture_stat(metadata: &Stat) -> Self {
            Self {
                dev: unsigned_field(metadata.st_dev),
                ino: unsigned_field(metadata.st_ino),
                size: unsigned_field(metadata.st_size),
                mtime: signed_field(metadata.st_mtime),
                mtime_nsec: signed_field(metadata.st_mtime_nsec),
                ctime: signed_field(metadata.st_ctime),
                ctime_nsec: signed_field(metadata.st_ctime_nsec),
            }
        }
    }

    fn unsigned_field<T: TryInto<u64>>(value: T) -> u64 {
        value.try_into().unwrap_or(u64::MAX)
    }

    fn signed_field<T: TryInto<i64>>(value: T) -> i64 {
        value.try_into().unwrap_or(i64::MAX)
    }

    struct EntryDigest {
        path: Vec<u8>,
        size: u64,
        digest: [u8; 32],
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum EntryKind {
        File,
        Directory,
    }

    struct EntryIdentity {
        path: Vec<u8>,
        kind: EntryKind,
        snapshot: Snapshot,
    }

    struct FinalValidation<'a> {
        approved_root: &'a Path,
        original_root: &'a File,
        root_before: Snapshot,
        relative: &'a Path,
        expected: Snapshot,
        expected_kind: ModelArtifactKind,
        identities: &'a [EntryIdentity],
        display_path: &'a Path,
        started: Instant,
        timeout: Duration,
    }

    struct State {
        limits: ModelIntegrityLimits,
        started: Instant,
        files: u64,
        entries_seen: u64,
        bytes: u64,
        digests: Vec<EntryDigest>,
        identities: Vec<EntryIdentity>,
        root_dev: u64,
    }

    pub(super) fn verify(
        approved_root: &Path,
        relative_path: &Path,
        limits: ModelIntegrityLimits,
    ) -> ModelIntegrityResult<ModelIntegrityDigest> {
        verify_with_final_hook(approved_root, relative_path, limits, || {})
    }

    fn verify_with_final_hook<F>(
        approved_root: &Path,
        relative_path: &Path,
        limits: ModelIntegrityLimits,
        before_final_validation: F,
    ) -> ModelIntegrityResult<ModelIntegrityDigest>
    where
        F: FnOnce(),
    {
        let started = Instant::now();
        let root = open_root(approved_root)?;
        let root_meta = metadata(&root, approved_root)?;
        if !root_meta.is_dir() {
            return Err(ModelIntegrityError::InvalidRoot(
                approved_root.to_path_buf(),
            ));
        }
        let root_dev = root_meta.dev();
        let root_before = Snapshot::capture(&root_meta);
        let (mut target, target_display) = open_relative(&root, relative_path, root_dev)?;
        let target_meta = metadata(&target, &target_display)?;

        let mut state = State {
            limits,
            started,
            files: 0,
            entries_seen: 0,
            bytes: 0,
            digests: Vec::new(),
            identities: Vec::new(),
            root_dev,
        };
        state.check_time()?;

        if target_meta.is_file() {
            let digest = state.hash_file(&mut target, &target_display, &target_meta)?;
            before_final_validation();
            ensure_final_path_identity(FinalValidation {
                approved_root,
                original_root: &root,
                root_before,
                relative: relative_path,
                expected: Snapshot::capture(&target_meta),
                expected_kind: ModelArtifactKind::File,
                identities: &[],
                display_path: &target_display,
                started: state.started,
                timeout: state.limits.timeout,
            })?;
            return Ok(ModelIntegrityDigest {
                kind: ModelArtifactKind::File,
                algorithm: "sha256",
                sha256: hex(&digest),
                files: state.files,
                entries: state.entries_seen,
                bytes: state.bytes,
            });
        }
        if !target_meta.is_dir() {
            return Err(ModelIntegrityError::UnsupportedType {
                path: target_display,
                kind: "non-regular",
            });
        }

        let before = Snapshot::capture(&target_meta);
        state.walk_directory(&target, Vec::new(), 0, &target_display)?;
        ensure_unchanged(before, &target, &target_display)?;
        before_final_validation();
        ensure_final_path_identity(FinalValidation {
            approved_root,
            original_root: &root,
            root_before,
            relative: relative_path,
            expected: before,
            expected_kind: ModelArtifactKind::Directory,
            identities: &state.identities,
            display_path: &target_display,
            started: state.started,
            timeout: state.limits.timeout,
        })?;
        state
            .digests
            .sort_by(|left, right| left.path.cmp(&right.path));

        let mut manifest = Sha256::new();
        manifest.update(DIRECTORY_DIGEST_DOMAIN);
        for entry in &state.digests {
            let record_len = 8_u64
                .checked_add(entry.path.len() as u64)
                .and_then(|n| n.checked_add(1 + 8 + 32))
                .ok_or(ModelIntegrityError::LimitExceeded { limit: "manifest" })?;
            manifest.update(record_len.to_be_bytes());
            manifest.update((entry.path.len() as u64).to_be_bytes());
            manifest.update(&entry.path);
            manifest.update([1_u8]);
            manifest.update(entry.size.to_be_bytes());
            manifest.update(entry.digest);
        }

        Ok(ModelIntegrityDigest {
            kind: ModelArtifactKind::Directory,
            algorithm: "ff-dir-v1+sha256",
            sha256: hex(&manifest.finalize()),
            files: state.files,
            entries: state.entries_seen,
            bytes: state.bytes,
        })
    }

    impl State {
        fn check_time(&self) -> ModelIntegrityResult<()> {
            if self.started.elapsed() > self.limits.timeout {
                Err(ModelIntegrityError::Timeout)
            } else {
                Ok(())
            }
        }

        fn reserve_entry(&mut self) -> ModelIntegrityResult<()> {
            self.entries_seen =
                self.entries_seen
                    .checked_add(1)
                    .ok_or(ModelIntegrityError::LimitExceeded {
                        limit: "entry-count",
                    })?;
            if self.entries_seen > self.limits.max_files {
                return Err(ModelIntegrityError::LimitExceeded {
                    limit: "entry-count",
                });
            }
            Ok(())
        }

        fn reserve_file(&mut self, size: u64) -> ModelIntegrityResult<()> {
            self.reserve_entry()?;
            self.files = self
                .files
                .checked_add(1)
                .ok_or(ModelIntegrityError::LimitExceeded { limit: "files" })?;
            self.bytes =
                self.bytes
                    .checked_add(size)
                    .ok_or(ModelIntegrityError::LimitExceeded {
                        limit: "byte-count",
                    })?;
            if self.bytes > self.limits.max_bytes {
                return Err(ModelIntegrityError::LimitExceeded {
                    limit: "byte-count",
                });
            }
            Ok(())
        }

        fn hash_file(
            &mut self,
            file: &mut File,
            display_path: &Path,
            initial: &Metadata,
        ) -> ModelIntegrityResult<[u8; 32]> {
            if initial.dev() != self.root_dev {
                return Err(ModelIntegrityError::CrossDevice(display_path.to_path_buf()));
            }
            if initial.nlink() != 1 {
                return Err(ModelIntegrityError::HardLink {
                    path: display_path.to_path_buf(),
                    links: initial.nlink(),
                });
            }
            self.reserve_file(initial.size())?;
            let before = Snapshot::capture(initial);
            let mut hasher = Sha256::new();
            let mut read_total = 0_u64;
            let mut buffer = [0_u8; 128 * 1024];
            loop {
                self.check_time()?;
                let count = file
                    .read(&mut buffer)
                    .map_err(|source| ModelIntegrityError::Io {
                        path: display_path.to_path_buf(),
                        source,
                    })?;
                if count == 0 {
                    break;
                }
                read_total = read_total.checked_add(count as u64).ok_or(
                    ModelIntegrityError::LimitExceeded {
                        limit: "byte-count",
                    },
                )?;
                if read_total > initial.size()
                    || self.bytes - initial.size() + read_total > self.limits.max_bytes
                {
                    return Err(ModelIntegrityError::LimitExceeded {
                        limit: "byte-count",
                    });
                }
                hasher.update(&buffer[..count]);
            }
            if read_total != initial.size() {
                return Err(ModelIntegrityError::Mutated(display_path.to_path_buf()));
            }
            ensure_unchanged(before, file, display_path)?;
            Ok(hasher.finalize().into())
        }

        fn walk_directory(
            &mut self,
            directory: &File,
            relative: Vec<u8>,
            depth: u32,
            display_path: &Path,
        ) -> ModelIntegrityResult<()> {
            self.check_time()?;
            if depth > self.limits.max_depth {
                return Err(ModelIntegrityError::LimitExceeded { limit: "depth" });
            }
            let before_meta = metadata(directory, display_path)?;
            if before_meta.dev() != self.root_dev {
                return Err(ModelIntegrityError::CrossDevice(display_path.to_path_buf()));
            }
            let before = Snapshot::capture(&before_meta);
            let mut names = read_names(directory, display_path)?;
            names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));

            for name in names {
                self.check_time()?;
                let child_display = display_path.join(&name);
                let stat = statat(directory.as_fd(), &name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| io_error(&child_display, error))?;
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::Symlink => {
                        return Err(ModelIntegrityError::UnsupportedType {
                            path: child_display,
                            kind: "symlink",
                        });
                    }
                    FileType::RegularFile | FileType::Directory => {}
                    _ => {
                        return Err(ModelIntegrityError::UnsupportedType {
                            path: child_display,
                            kind: "non-regular",
                        });
                    }
                }

                let mut child = open_descendant(directory, &name, OPEN_FLAGS, &child_display)?;
                let child_meta = metadata(&child, &child_display)?;
                ensure_same_snapshot(
                    Snapshot::capture_stat(&stat),
                    Snapshot::capture(&child_meta),
                    &child_display,
                )?;
                if child_meta.dev() != self.root_dev {
                    return Err(ModelIntegrityError::CrossDevice(child_display));
                }
                let mut child_relative = relative.clone();
                if !child_relative.is_empty() {
                    child_relative.push(b'/');
                }
                child_relative.extend_from_slice(name.as_bytes());

                if child_meta.is_file() {
                    let size = child_meta.size();
                    let snapshot = Snapshot::capture(&child_meta);
                    let digest = self.hash_file(&mut child, &child_display, &child_meta)?;
                    self.digests.push(EntryDigest {
                        path: child_relative.clone(),
                        size,
                        digest,
                    });
                    self.identities.push(EntryIdentity {
                        path: child_relative,
                        kind: EntryKind::File,
                        snapshot,
                    });
                } else if child_meta.is_dir() {
                    self.reserve_entry()?;
                    let snapshot = Snapshot::capture(&child_meta);
                    self.walk_directory(
                        &child,
                        child_relative.clone(),
                        depth
                            .checked_add(1)
                            .ok_or(ModelIntegrityError::LimitExceeded { limit: "depth" })?,
                        &child_display,
                    )?;
                    self.identities.push(EntryIdentity {
                        path: child_relative,
                        kind: EntryKind::Directory,
                        snapshot,
                    });
                } else {
                    return Err(ModelIntegrityError::UnsupportedType {
                        path: child_display,
                        kind: "changed type during open",
                    });
                }
            }
            ensure_unchanged(before, directory, display_path)
        }
    }

    fn open_root(path: &Path) -> ModelIntegrityResult<File> {
        let mut current = File::from(
            openat(rustix::fs::CWD, "/", OPEN_DIR_FLAGS, Mode::empty())
                .map_err(|error| io_error(Path::new("/"), error))?,
        );
        let mut display = PathBuf::from("/");
        for component in path.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    display.push(name);
                    current = File::from(
                        openat(current.as_fd(), name, OPEN_DIR_FLAGS, Mode::empty())
                            .map_err(|error| io_error(&display, error))?,
                    );
                }
                _ => return Err(ModelIntegrityError::InvalidRoot(path.to_path_buf())),
            }
        }
        Ok(current)
    }

    fn open_relative(
        root: &File,
        relative: &Path,
        root_dev: u64,
    ) -> ModelIntegrityResult<(File, PathBuf)> {
        let mut current = File::from(dup(root.as_fd()).map_err(|error| io_error(relative, error))?);
        let mut display = PathBuf::new();
        let components: Vec<_> = relative.components().collect();
        for (index, component) in components.iter().enumerate() {
            let Component::Normal(name) = component else {
                return Err(ModelIntegrityError::InvalidRelativePath(
                    relative.to_path_buf(),
                ));
            };
            display.push(name);
            let flags = if index + 1 == components.len() {
                OPEN_FLAGS
            } else {
                OPEN_DIR_FLAGS
            };
            current = open_descendant(&current, name, flags, &display)?;
            if metadata(&current, &display)?.dev() != root_dev {
                return Err(ModelIntegrityError::CrossDevice(display));
            }
        }
        Ok((current, display))
    }

    fn open_descendant(
        parent: &File,
        name: &OsStr,
        flags: OFlags,
        display_path: &Path,
    ) -> ModelIntegrityResult<File> {
        #[cfg(target_os = "linux")]
        {
            let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV;
            match openat2(parent.as_fd(), name, flags, Mode::empty(), resolve) {
                Ok(fd) => return Ok(File::from(fd)),
                Err(Errno::NOSYS) | Err(Errno::INVAL) => {}
                Err(error) => return Err(io_error(display_path, error)),
            }
        }
        openat(parent.as_fd(), name, flags, Mode::empty())
            .map(File::from)
            .map_err(|error| io_error(display_path, error))
    }

    fn read_names(directory: &File, display_path: &Path) -> ModelIntegrityResult<Vec<OsString>> {
        let mut stream =
            Dir::read_from(directory.as_fd()).map_err(|error| io_error(display_path, error))?;
        let mut names = Vec::new();
        for entry in &mut stream {
            let entry = entry.map_err(|error| io_error(display_path, error))?;
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(OsString::from_vec(bytes.to_vec()));
        }
        Ok(names)
    }

    fn metadata(file: &File, path: &Path) -> ModelIntegrityResult<Metadata> {
        file.metadata().map_err(|source| ModelIntegrityError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    fn ensure_unchanged(before: Snapshot, file: &File, path: &Path) -> ModelIntegrityResult<()> {
        let after = Snapshot::capture(&metadata(file, path)?);
        ensure_same_snapshot(before, after, path)
    }

    fn ensure_same_snapshot(
        before: Snapshot,
        after: Snapshot,
        path: &Path,
    ) -> ModelIntegrityResult<()> {
        if before == after {
            Ok(())
        } else {
            Err(ModelIntegrityError::Mutated(path.to_path_buf()))
        }
    }

    fn ensure_final_path_identity(check: FinalValidation<'_>) -> ModelIntegrityResult<()> {
        let FinalValidation {
            approved_root,
            original_root,
            root_before,
            relative,
            expected,
            expected_kind,
            identities,
            display_path,
            started,
            timeout,
        } = check;
        check_final_time(started, timeout)?;
        ensure_unchanged(root_before, original_root, approved_root)?;

        // Re-resolve the absolute approved root instead of trusting the
        // original descriptor: that descriptor remains valid after a rename
        // and could otherwise authenticate a detached directory.
        let current_root = open_root(approved_root)?;
        let current_root_meta = metadata(&current_root, approved_root)?;
        ensure_same_snapshot(
            root_before,
            Snapshot::capture(&current_root_meta),
            approved_root,
        )?;
        let (reopened, _) = open_relative(&current_root, relative, current_root_meta.dev())?;
        let reopened_meta = metadata(&reopened, display_path)?;
        ensure_artifact_kind(&reopened_meta, expected_kind, display_path)?;
        ensure_same_snapshot(expected, Snapshot::capture(&reopened_meta), display_path)?;

        if expected_kind == ModelArtifactKind::Directory {
            let target_dev = reopened_meta.dev();
            for identity in identities {
                check_final_time(started, timeout)?;
                let relative_entry = PathBuf::from(OsString::from_vec(identity.path.clone()));
                let (entry, _) = open_relative(&reopened, &relative_entry, target_dev)?;
                let entry_meta = metadata(&entry, &relative_entry)?;
                ensure_entry_kind(&entry_meta, identity.kind, &relative_entry)?;
                ensure_same_snapshot(
                    identity.snapshot,
                    Snapshot::capture(&entry_meta),
                    &relative_entry,
                )?;
            }
        }
        check_final_time(started, timeout)
    }

    fn check_final_time(started: Instant, timeout: Duration) -> ModelIntegrityResult<()> {
        if started.elapsed() > timeout {
            Err(ModelIntegrityError::Timeout)
        } else {
            Ok(())
        }
    }

    fn ensure_artifact_kind(
        metadata: &Metadata,
        expected: ModelArtifactKind,
        path: &Path,
    ) -> ModelIntegrityResult<()> {
        let matches = match expected {
            ModelArtifactKind::File => metadata.is_file(),
            ModelArtifactKind::Directory => metadata.is_dir(),
        };
        if matches {
            Ok(())
        } else {
            Err(ModelIntegrityError::Mutated(path.to_path_buf()))
        }
    }

    fn ensure_entry_kind(
        metadata: &Metadata,
        expected: EntryKind,
        path: &Path,
    ) -> ModelIntegrityResult<()> {
        let matches = match expected {
            EntryKind::File => metadata.is_file(),
            EntryKind::Directory => metadata.is_dir(),
        };
        if matches {
            Ok(())
        } else {
            Err(ModelIntegrityError::Mutated(path.to_path_buf()))
        }
    }

    fn io_error(path: &Path, error: Errno) -> ModelIntegrityError {
        ModelIntegrityError::Io {
            path: path.to_path_buf(),
            source: error.into(),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(bytes.len() * 2);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in bytes {
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
        output
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::symlink;

        fn verify(root: &Path, relative: &str) -> ModelIntegrityResult<ModelIntegrityDigest> {
            super::verify(root, Path::new(relative), ModelIntegrityLimits::default())
        }

        #[test]
        fn file_sha256_matches_known_vector() {
            let temp = tempfile::tempdir().unwrap();
            fs::write(temp.path().join("model.gguf"), b"abc").unwrap();
            let result = verify(temp.path(), "model.gguf").unwrap();
            assert_eq!(result.kind, ModelArtifactKind::File);
            assert_eq!(
                result.sha256,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
            assert_eq!((result.files, result.entries, result.bytes), (1, 1, 3));
        }

        #[test]
        fn directory_digest_is_order_independent() {
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            for root in [first.path(), second.path()] {
                fs::create_dir(root.join("weights")).unwrap();
            }
            fs::write(first.path().join("z.json"), b"z").unwrap();
            fs::write(first.path().join("weights/a.bin"), b"aaa").unwrap();
            fs::write(second.path().join("weights/a.bin"), b"aaa").unwrap();
            fs::write(second.path().join("z.json"), b"z").unwrap();

            let first = verify(first.path(), "").unwrap();
            let second = verify(second.path(), "").unwrap();
            assert_eq!(first.sha256, second.sha256);
            assert_eq!(
                first.sha256,
                "eb17cffaba4acb535c81f39ef469d52723f72db00b2f8d50e1c95cef63cd795c"
            );
            assert_eq!(first.algorithm, "ff-dir-v1+sha256");
            assert_eq!((first.files, first.entries, first.bytes), (2, 3, 4));
        }

        #[test]
        fn rejects_symlink_traversal_and_hardlinks() {
            let temp = tempfile::tempdir().unwrap();
            fs::write(temp.path().join("real"), b"model").unwrap();
            symlink("real", temp.path().join("link")).unwrap();
            assert!(matches!(
                verify(temp.path(), ""),
                Err(ModelIntegrityError::UnsupportedType {
                    kind: "symlink",
                    ..
                })
            ));
            assert!(matches!(
                verify_model_path(
                    "adele",
                    temp.path(),
                    Path::new("../escape"),
                    ModelIntegrityLimits::default()
                ),
                Err(ModelIntegrityError::InvalidRelativePath(_))
            ));

            fs::remove_file(temp.path().join("link")).unwrap();
            fs::hard_link(temp.path().join("real"), temp.path().join("copy")).unwrap();
            assert!(matches!(
                verify(temp.path(), "real"),
                Err(ModelIntegrityError::HardLink { .. })
            ));
        }

        #[test]
        fn enforces_file_byte_depth_and_time_limits() {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir_all(temp.path().join("a/b")).unwrap();
            fs::write(temp.path().join("a/b/model"), b"abcd").unwrap();
            for (limits, expected) in [
                (
                    ModelIntegrityLimits {
                        max_files: 0,
                        ..Default::default()
                    },
                    "entry-count",
                ),
                (
                    ModelIntegrityLimits {
                        max_bytes: 3,
                        ..Default::default()
                    },
                    "byte-count",
                ),
                (
                    ModelIntegrityLimits {
                        max_depth: 0,
                        ..Default::default()
                    },
                    "depth",
                ),
            ] {
                let error = super::verify(temp.path(), Path::new(""), limits).unwrap_err();
                assert!(
                    matches!(error, ModelIntegrityError::LimitExceeded { limit } if limit == expected)
                );
            }
            let error = super::verify(
                temp.path(),
                Path::new(""),
                ModelIntegrityLimits {
                    timeout: Duration::ZERO,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(matches!(error, ModelIntegrityError::Timeout));
        }

        #[test]
        fn empty_directories_count_against_the_entry_limit() {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("a")).unwrap();
            fs::create_dir(temp.path().join("b")).unwrap();
            let error = super::verify(
                temp.path(),
                Path::new(""),
                ModelIntegrityLimits {
                    max_files: 1,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(matches!(
                error,
                ModelIntegrityError::LimitExceeded {
                    limit: "entry-count"
                }
            ));
        }

        #[test]
        fn final_tree_pass_rejects_an_already_hashed_nested_child_mutation() {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("nested")).unwrap();
            fs::write(temp.path().join("nested/model.bin"), b"before").unwrap();

            let error = verify_with_final_hook(
                temp.path(),
                Path::new(""),
                ModelIntegrityLimits::default(),
                || {
                    // In-place child mutation does not change the target/root
                    // directory identity, so only the retained tree pass can
                    // reject the now-stale manifest.
                    fs::write(temp.path().join("nested/model.bin"), b"mutated-longer").unwrap();
                },
            )
            .unwrap_err();

            assert!(matches!(
                error,
                ModelIntegrityError::Mutated(path)
                    if path == Path::new("nested/model.bin")
            ));
        }

        #[test]
        fn snapshot_detects_high_resolution_mutation() {
            let before = Snapshot {
                dev: 1,
                ino: 2,
                size: 3,
                mtime: 4,
                mtime_nsec: 5,
                ctime: 6,
                ctime_nsec: 7,
            };
            assert!(matches!(
                ensure_same_snapshot(
                    before,
                    Snapshot {
                    ctime_nsec: 8,
                    ..before
                    },
                    Path::new("model"),
                ),
                Err(ModelIntegrityError::Mutated(path)) if path == Path::new("model")
            ));
        }

        #[test]
        fn rejects_renamed_and_replaced_absolute_root() {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("models");
            let detached = parent.path().join("detached-models");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("model.gguf"), b"same bytes").unwrap();

            let error = verify_with_final_hook(
                &root,
                Path::new("model.gguf"),
                ModelIntegrityLimits::default(),
                || {
                    fs::rename(&root, &detached).unwrap();
                    fs::create_dir(&root).unwrap();
                    fs::write(root.join("model.gguf"), b"same bytes").unwrap();
                },
            )
            .unwrap_err();

            assert!(
                matches!(error, ModelIntegrityError::Mutated(ref path) if path == &root),
                "unexpected error: {error}"
            );
        }

        #[test]
        fn absolute_root_reopen_rejects_a_still_valid_detached_descriptor() {
            let parent = tempfile::tempdir().unwrap();
            let root_path = parent.path().join("models");
            let detached = parent.path().join("detached-models");
            fs::create_dir(&root_path).unwrap();
            fs::write(root_path.join("model.gguf"), b"same bytes").unwrap();

            let original_root = open_root(&root_path).unwrap();
            let (original_target, display) = open_relative(
                &original_root,
                Path::new("model.gguf"),
                metadata(&original_root, &root_path).unwrap().dev(),
            )
            .unwrap();
            let target_snapshot = Snapshot::capture(&metadata(&original_target, &display).unwrap());

            fs::rename(&root_path, &detached).unwrap();
            fs::create_dir(&root_path).unwrap();
            fs::write(root_path.join("model.gguf"), b"same bytes").unwrap();

            // Capture after the rename so the detached descriptor itself is
            // unchanged across validation. Only reopening the absolute root
            // can distinguish it from the replacement at the approved path.
            let detached_snapshot =
                Snapshot::capture(&metadata(&original_root, &root_path).unwrap());
            let error = ensure_final_path_identity(FinalValidation {
                approved_root: &root_path,
                original_root: &original_root,
                root_before: detached_snapshot,
                relative: Path::new("model.gguf"),
                expected: target_snapshot,
                expected_kind: ModelArtifactKind::File,
                identities: &[],
                display_path: &display,
                started: Instant::now(),
                timeout: ModelIntegrityLimits::default().timeout,
            })
            .unwrap_err();

            assert!(
                matches!(error, ModelIntegrityError::Mutated(ref path) if path == &root_path),
                "unexpected error: {error}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_parser_and_constant_time_compare_are_strict() {
        let lower = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let upper = lower.to_ascii_uppercase();
        assert_eq!(parse_sha256_hex(lower).unwrap().len(), 32);
        assert!(constant_time_sha256_eq(lower, &upper));
        assert!(!constant_time_sha256_eq(lower, &"0".repeat(64)));
        assert!(!constant_time_sha256_eq(lower, "sha256:bad"));
    }

    #[test]
    fn vinny_is_excluded_case_insensitively() {
        for name in ["vinny", "VINNY", " Vinny "] {
            assert!(!model_integrity_worker_allowed(name));
        }
        assert!(model_integrity_worker_allowed("adele"));
    }
}
