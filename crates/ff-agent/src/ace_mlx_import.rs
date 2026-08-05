//! Exact, fail-closed import of Ace's pinned Gemma 4 E4B MLX artifact.
//!
//! The public operation intentionally accepts no filesystem paths.  It uses
//! the operating-system account home, an authority-grade fleet identity, and
//! one compiled repo/revision/manifest.  Hugging Face snapshot symlinks are
//! never followed: their exact `../../blobs/<hex>` targets are parsed, then the
//! blob is opened relative to the already-open blob directory with
//! `O_NOFOLLOW`.  Promotion is same-directory and no-replace.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

pub const CATALOG_ID: &str = "gemma4-e4b-it";
pub const HF_REPO: &str = "mlx-community/gemma-4-e4b-it-4bit";
pub const HF_REVISION: &str = "475b9088d29754a3379866cf5aeb6b41acd313c2";
pub const BASE_HF_REPO: &str = "google/gemma-4-E4B-it";
pub const BASE_SOURCE_REVISION: &str = "fee6332c1abaafb77f6f9624236c63aa2f1d0187";
pub const ARTIFACT_SIZE_BYTES: u64 = 5_179_241_512;
pub const DISK_RESERVE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
const CACHE_REPO_DIR: &str = "models--mlx-community--gemma-4-e4b-it-4bit";
const FINAL_DIR_NAME: &str = "gemma-4-e4b-it-4bit";
pub(crate) const SOURCE_URL: &str = "https://huggingface.co/mlx-community/gemma-4-e4b-it-4bit/tree/475b9088d29754a3379866cf5aeb6b41acd313c2";
const DESCRIPTION: &str =
    "Gemma 4 E4B instruction-tuned multimodal MLX 4-bit artifact pinned for Apple Silicon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestEntry {
    pub name: &'static str,
    pub size_bytes: u64,
    pub sha256: &'static str,
}

pub const MANIFEST: &[ManifestEntry] = &[
    ManifestEntry {
        name: ".gitattributes",
        size_bytes: 1_570,
        sha256: "34448b82c17d60fec9b65b1f093c115ddbaadc04beb1b0140b6bfed2e012a930",
    },
    ManifestEntry {
        name: "README.md",
        size_bytes: 593,
        sha256: "8e9717df49219943b8992eeccecd8050535318d34fffbbaeaac924ed01f27155",
    },
    ManifestEntry {
        name: "chat_template.jinja",
        size_bytes: 17_336,
        sha256: "2f1b4d75d067bae3fe44e676721c7f077d243bc007156cb9c2f8b5836613d082",
    },
    ManifestEntry {
        name: "config.json",
        size_bytes: 6_628,
        sha256: "780ccb3a514a5f1ced161d383f948fc22eca9b84b752ca19494f625bd9bad7a6",
    },
    ManifestEntry {
        name: "generation_config.json",
        size_bytes: 208,
        sha256: "d4226bbe3117d2d253ba4609720ba82c6c4ce4627a9a6ae05387c78983ac03de",
    },
    ManifestEntry {
        name: "model.safetensors",
        size_bytes: 5_146_800_534,
        sha256: "932b8271fc3fe65adcc78b96c10c6268bbfb13e8f67d1358727c0d6ee97e1eff",
    },
    ManifestEntry {
        name: "model.safetensors.index.json",
        size_bytes: 240_961,
        sha256: "f8accac59ee7efe87e0c298c854610b262c3cadd477407503147c71209ff0093",
    },
    ManifestEntry {
        name: "processor_config.json",
        size_bytes: 1_316,
        sha256: "de3e580aebdc98272d4c4547daffe6525fcbae18a83a0e0bcf0d7444d4ee6f37",
    },
    ManifestEntry {
        name: "tokenizer.json",
        size_bytes: 32_169_626,
        sha256: "cc8d3a0ce36466ccc1278bf987df5f71db1719b9ca6b4118264f45cb627bfe0f",
    },
    ManifestEntry {
        name: "tokenizer_config.json",
        size_bytes: 2_740,
        sha256: "080d9e1aff284e2f6043889cd05367966f7c7b80e025fbc0b06745e218158656",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportDisposition {
    Imported,
    AlreadyPresent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub disposition: ImportDisposition,
    pub final_path: PathBuf,
    pub library_id: String,
}

/// Reverify an installed Ace Gemma 4 directory without following a final-path
/// symlink or accepting a non-canonical path.  The exact directory enumerator
/// also opens every manifest member with `O_NOFOLLOW` and rechecks owner, mode,
/// link count, byte length, and SHA-256.
pub(crate) fn verify_installed_ace_gemma4_mlx(path: &Path) -> Result<()> {
    ensure_target_platform()?;
    let expected = os_account_home()?
        .canonicalize()
        .context("resolve OS account home for installed Ace verifier")?
        .join("models")
        .join("mlx")
        .join(FINAL_DIR_NAME);
    if path != expected {
        bail!(
            "installed Ace Gemma 4 path is not the fixed destination: {}",
            path.display()
        );
    }
    verify_installed_manifest(path, MANIFEST)
}

fn verify_installed_manifest(path: &Path, manifest: &[ManifestEntry]) -> Result<()> {
    if !path.is_absolute() {
        bail!("installed Ace Gemma 4 path must be absolute");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolve installed Ace Gemma 4 path {}", path.display()))?;
    if canonical != path {
        bail!(
            "installed Ace Gemma 4 path is not canonical: {}",
            path.display()
        );
    }
    let directory = open_directory(&canonical, "installed Ace Gemma 4 directory")?;
    verify_exact_directory(&directory, manifest, "installed Ace Gemma 4 artifact")
}

/// Import and index the one reviewed Ace artifact.
///
/// `ff model import-ace-gemma4-mlx` runs database migrations before reaching
/// this function.  We still compare the complete catalog row against the
/// compiled authority both before the copy and immediately before indexing.
pub async fn import_ace_gemma4_mlx(pool: &PgPool) -> Result<ImportReport> {
    ensure_target_platform()?;
    let identity = crate::fleet_info::resolve_this_computer_identity_strict(pool)
        .await
        .map_err(|error| anyhow!(error))?;
    ensure_ace_identity(&identity.name)?;
    validate_catalog_authority(pool).await?;

    let home = os_account_home()?;
    let filesystem = tokio::task::spawn_blocking({
        let home = home.clone();
        move || import_under(&home, MANIFEST, None, FailurePoint::Never)
    })
    .await
    .map_err(|error| anyhow!("Ace MLX import worker stopped unexpectedly: {error}"))??;

    // A catalog mutation during the multi-gigabyte copy must not authorize a
    // library row.  The exact artifact remains safe on disk and a later retry
    // can index it after authority is restored.
    validate_catalog_authority(pool).await?;
    let library_id =
        crate::model_library_scanner::index_exact_ace_gemma4_mlx(pool, &filesystem.final_path)
            .await
            .map_err(|error| anyhow!(error))?;

    Ok(ImportReport {
        disposition: filesystem.disposition,
        final_path: filesystem.final_path,
        library_id,
    })
}

fn ensure_target_platform() -> Result<()> {
    if std::env::consts::OS != "macos" || std::env::consts::ARCH != "aarch64" {
        bail!(
            "Ace Gemma 4 MLX import requires aarch64-apple-darwin (this binary is {}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    }
    Ok(())
}

fn ensure_ace_identity(name: &str) -> Result<()> {
    if !name.eq_ignore_ascii_case("ace") {
        bail!(
            "Ace Gemma 4 MLX import is restricted to canonical computer 'ace' (resolved '{name}')"
        );
    }
    Ok(())
}

fn expected_variant() -> serde_json::Value {
    let files = MANIFEST
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "size_bytes": entry.size_bytes,
                "sha256": entry.sha256,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "runtime": "mlx",
        "quant": "4bit",
        "hf_repo": HF_REPO,
        "source_revision": HF_REVISION,
        "base_hf_repo": BASE_HF_REPO,
        "base_source_revision": BASE_SOURCE_REVISION,
        "target_triple": "aarch64-apple-darwin",
        "artifact_size_bytes": ARTIFACT_SIZE_BYTES,
        "files": files,
    })
}

pub(crate) fn expected_catalog_row() -> serde_json::Value {
    json!({
        "id": CATALOG_ID,
        "name": "Gemma 4 E4B Instruct",
        "family": "gemma",
        "parameters": "E4B",
        "tier": 1,
        "description": DESCRIPTION,
        "gated": false,
        "preferred_workloads": ["chat", "vision", "audio", "video", "multimodal"],
        "variants": [expected_variant()],
        "tool_calling": false,
        "display_name": null,
        "tasks": null,
        "modalities": null,
        "benchmarks": null,
        "license": "gemma",
        "lifecycle": "active",
    })
}

async fn validate_catalog_authority(pool: &PgPool) -> Result<()> {
    let rows: Vec<serde_json::Value> = sqlx::query_scalar(
        r#"
        SELECT jsonb_build_object(
                   'id', id,
                   'name', name,
                   'family', family,
                   'parameters', parameters,
                   'tier', tier,
                   'description', description,
                   'gated', gated,
                   'preferred_workloads', preferred_workloads,
                   'variants', variants,
                   'tool_calling', tool_calling,
                   'display_name', display_name,
                   'tasks', tasks,
                   'modalities', modalities,
                   'benchmarks', benchmarks,
                   'license', license,
                   'lifecycle', lifecycle
               )
          FROM fleet_model_catalog
         WHERE id = $1
        "#,
    )
    .bind(CATALOG_ID)
    .fetch_all(pool)
    .await
    .context("read Gemma 4 E4B catalog authority")?;

    match rows.as_slice() {
        [actual] if *actual == expected_catalog_row() => Ok(()),
        [actual] => bail!("Gemma 4 E4B catalog authority drifted: {actual}"),
        [] => bail!("Gemma 4 E4B catalog authority is missing; V294 has not converged"),
        _ => bail!(
            "Gemma 4 E4B catalog authority is duplicated: found {} rows",
            rows.len()
        ),
    }
}

#[derive(Debug)]
struct FilesystemReport {
    disposition: ImportDisposition,
    final_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailurePoint {
    Never,
    #[cfg(test)]
    AfterFirstCopy,
}

fn import_under(
    home: &Path,
    manifest: &[ManifestEntry],
    available_override: Option<u64>,
    failure: FailurePoint,
) -> Result<FilesystemReport> {
    validate_manifest(manifest)?;
    let canonical_home = home
        .canonicalize()
        .with_context(|| format!("resolve OS account home {}", home.display()))?;
    let home_dir = open_directory(&canonical_home, "OS account home")?;
    verify_owned_directory(&home_dir, false, "OS account home")?;

    let final_path = canonical_home
        .join("models")
        .join("mlx")
        .join(FINAL_DIR_NAME);

    // A blocked preflight must not create even empty destination directories.
    // Inspect the deepest existing destination component first; exact finals
    // remain idempotent even when free space has subsequently fallen below the
    // copy threshold.
    let existing_models =
        open_optional_owned_child_directory(&home_dir, "models", "Ace models directory")?;
    let existing_mlx = existing_models
        .as_ref()
        .map(|models| {
            open_optional_owned_child_directory(models, "mlx", "Ace MLX models directory")
        })
        .transpose()?
        .flatten();
    if let Some(mlx_dir) = existing_mlx.as_ref()
        && let Some(final_dir) =
            open_optional_owned_child_directory(mlx_dir, FINAL_DIR_NAME, "Ace MLX final directory")?
    {
        verify_exact_directory(&final_dir, manifest, "existing final artifact")?;
        return Ok(FilesystemReport {
            disposition: ImportDisposition::AlreadyPresent,
            final_path,
        });
    }

    let required = manifest_total(manifest)?
        .checked_add(DISK_RESERVE_BYTES)
        .ok_or_else(|| anyhow!("Ace MLX disk requirement overflow"))?;
    let available_directory = existing_mlx
        .as_ref()
        .or(existing_models.as_ref())
        .unwrap_or(&home_dir);
    let available = available_override.unwrap_or(available_space(available_directory)?);
    if available < required {
        bail!(
            "insufficient disk for Ace MLX import: available={available} required={required} (artifact plus 10 GiB reserve)"
        );
    }

    let models_dir = ensure_owned_child_directory(&home_dir, "models", 0o700)?;
    let mlx_dir = ensure_owned_child_directory(&models_dir, "mlx", 0o700)?;

    let cache = open_existing_chain(
        &home_dir,
        &[".cache", "huggingface", "hub", CACHE_REPO_DIR],
        "Hugging Face cache",
    )?;
    let blobs = open_existing_child_directory(&cache, "blobs", "Hugging Face blob root")?;
    let snapshots =
        open_existing_child_directory(&cache, "snapshots", "Hugging Face snapshots root")?;
    let snapshot = open_existing_child_directory(
        &snapshots,
        HF_REVISION,
        "exact Hugging Face snapshot revision",
    )?;

    let temp_name = format!(".{FINAL_DIR_NAME}.ff-import-{}.tmp", uuid::Uuid::new_v4());
    let temp_dir = create_exclusive_directory(&mlx_dir, &temp_name, 0o700)?;
    let mut guard = TempGuard::new(&mlx_dir, &temp_dir, &temp_name, manifest)?;

    for (index, entry) in manifest.iter().enumerate() {
        copy_one(&snapshot, &blobs, &temp_dir, entry)?;
        #[cfg(test)]
        if failure == FailurePoint::AfterFirstCopy && index == 0 {
            bail!("injected failure after first copied file");
        }
        #[cfg(not(test))]
        let _ = (index, failure);
    }
    verify_exact_directory(&temp_dir, manifest, "completed temporary artifact")?;
    temp_dir
        .sync_all()
        .context("fsync completed Ace MLX temporary directory")?;

    match rename_noreplace(&mlx_dir, &temp_name, FINAL_DIR_NAME) {
        Ok(()) => {
            guard.promoted = true;
            mlx_dir
                .sync_all()
                .context("fsync Ace MLX parent after promotion")?;
            let final_dir = open_existing_child_directory(
                &mlx_dir,
                FINAL_DIR_NAME,
                "promoted Ace MLX artifact",
            )?;
            verify_exact_directory(&final_dir, manifest, "promoted Ace MLX artifact")?;
            Ok(FilesystemReport {
                disposition: ImportDisposition::Imported,
                final_path,
            })
        }
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            let final_dir = open_existing_child_directory(
                &mlx_dir,
                FINAL_DIR_NAME,
                "racing Ace MLX final artifact",
            )?;
            verify_exact_directory(&final_dir, manifest, "racing final artifact")?;
            Ok(FilesystemReport {
                disposition: ImportDisposition::AlreadyPresent,
                final_path,
            })
        }
        Err(error) => Err(error).context("atomically promote Ace MLX artifact without replace"),
    }
}

fn validate_manifest(manifest: &[ManifestEntry]) -> Result<()> {
    if manifest.is_empty() {
        bail!("Ace MLX manifest is empty");
    }
    let mut names = BTreeSet::new();
    for entry in manifest {
        if !is_single_component(entry.name) || !names.insert(entry.name) {
            bail!("unsafe or duplicate manifest name: {}", entry.name);
        }
        if entry.size_bytes == 0
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("invalid manifest authority for {}", entry.name);
        }
    }
    Ok(())
}

fn manifest_total(manifest: &[ManifestEntry]) -> Result<u64> {
    manifest.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size_bytes)
            .ok_or_else(|| anyhow!("manifest byte total overflow"))
    })
}

fn is_single_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

fn copy_one(
    snapshot: &File,
    blobs: &File,
    destination: &File,
    entry: &ManifestEntry,
) -> Result<()> {
    let mut source = open_snapshot_blob(snapshot, blobs, entry)?;
    let source_before = fstat(source.as_raw_fd()).context("inspect opened HF blob")?;
    verify_source_blob_stat(&source_before, entry)?;

    let name = cstring(entry.name)?;
    let destination_fd = unsafe {
        libc::openat(
            destination.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if destination_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("create exclusive temporary file {}", entry.name));
    }
    let mut output = unsafe { File::from_raw_fd(destination_fd) };
    if unsafe { libc::fchmod(output.as_raw_fd(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("set private mode on {}", entry.name));
    }

    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .with_context(|| format!("read HF blob for {}", entry.name))?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| anyhow!("copy byte count overflow for {}", entry.name))?;
        if copied > entry.size_bytes {
            bail!("HF blob {} exceeds its exact size", entry.name);
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .with_context(|| format!("copy {} into exclusive temporary artifact", entry.name))?;
    }
    let source_digest = format!("{:x}", hasher.finalize());
    if copied != entry.size_bytes || source_digest != entry.sha256 {
        bail!(
            "HF blob authority mismatch for {}: size={} sha256={}",
            entry.name,
            copied,
            source_digest
        );
    }
    let source_after = fstat(source.as_raw_fd()).context("reinspect opened HF blob")?;
    if !same_inode_and_shape(&source_before, &source_after) {
        bail!("HF blob changed while copying {}", entry.name);
    }

    output
        .sync_all()
        .with_context(|| format!("fsync temporary {}", entry.name))?;
    verify_destination_file(&mut output, entry)?;
    Ok(())
}

fn open_snapshot_blob(snapshot: &File, blobs: &File, entry: &ManifestEntry) -> Result<File> {
    let name = cstring(entry.name)?;
    let link_stat = fstatat_nofollow(snapshot.as_raw_fd(), &name)
        .with_context(|| format!("inspect HF snapshot entry {}", entry.name))?;
    if link_stat.st_mode & libc::S_IFMT != libc::S_IFLNK || link_stat.st_uid != effective_uid() {
        bail!(
            "HF snapshot entry {} is not an owner-controlled symlink",
            entry.name
        );
    }
    let target = readlinkat(snapshot.as_raw_fd(), &name)
        .with_context(|| format!("read HF snapshot symlink {}", entry.name))?;
    let components = Path::new(&target).components().collect::<Vec<_>>();
    let blob_name = match components.as_slice() {
        [
            Component::ParentDir,
            Component::ParentDir,
            Component::Normal(blobs_component),
            Component::Normal(blob),
        ] if *blobs_component == OsStr::new("blobs") => blob,
        _ => bail!(
            "HF snapshot entry {} does not target ../../blobs/<blob>",
            entry.name
        ),
    };
    let blob_bytes = blob_name.as_bytes();
    if !matches!(blob_bytes.len(), 40 | 64)
        || !blob_bytes
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("HF snapshot entry {} has an unsafe blob name", entry.name);
    }
    let blob_name = CString::new(blob_bytes).expect("validated blob name contains no NUL");
    let fd = unsafe {
        libc::openat(
            blobs.as_raw_fd(),
            blob_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open contained HF blob for {}", entry.name));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn verify_source_blob_stat(stat: &libc::stat, entry: &ManifestEntry) -> Result<()> {
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_uid != effective_uid()
        || stat.st_nlink != 1
        || stat.st_mode & 0o022 != 0
        || stat.st_size < 0
        || stat.st_size as u64 != entry.size_bytes
    {
        bail!(
            "HF blob for {} is not an exact owner-controlled single-link regular file",
            entry.name
        );
    }
    Ok(())
}

fn same_inode_and_shape(before: &libc::stat, after: &libc::stat) -> bool {
    before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_mode == after.st_mode
        && before.st_uid == after.st_uid
        && before.st_nlink == after.st_nlink
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
}

fn verify_destination_file(file: &mut File, entry: &ManifestEntry) -> Result<()> {
    let stat =
        fstat(file.as_raw_fd()).with_context(|| format!("inspect temporary {}", entry.name))?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_uid != effective_uid()
        || stat.st_nlink != 1
        || stat.st_mode & 0o777 != 0o600
        || stat.st_size < 0
        || stat.st_size as u64 != entry.size_bytes
    {
        bail!(
            "temporary {} is not an exact private single-link regular file",
            entry.name
        );
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind temporary {}", entry.name))?;
    let digest = hash_reader(file).with_context(|| format!("rehash temporary {}", entry.name))?;
    if digest != entry.sha256 {
        bail!("temporary file hash mismatch for {}", entry.name);
    }
    Ok(())
}

fn verify_exact_directory(directory: &File, manifest: &[ManifestEntry], label: &str) -> Result<()> {
    verify_owned_directory(directory, true, label)?;
    let expected = manifest
        .iter()
        .map(|entry| entry.name.to_string())
        .collect::<BTreeSet<_>>();
    let before = directory_entries(directory).with_context(|| format!("enumerate {label}"))?;
    if before != expected {
        bail!("{label} is partial, extra, or drifted: found {before:?}");
    }
    for entry in manifest {
        let name = cstring(entry.name)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open exact {label} file {}", entry.name));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        verify_destination_file(&mut file, entry)
            .with_context(|| format!("verify {label} file {}", entry.name))?;
    }
    let after = directory_entries(directory).with_context(|| format!("reenumerate {label}"))?;
    if after != expected {
        bail!("{label} changed during verification: found {after:?}");
    }
    Ok(())
}

fn hash_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn open_directory(path: &Path, label: &str) -> Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("{label} path contains NUL"))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("open {label}"));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_existing_chain(root: &File, components: &[&str], label: &str) -> Result<File> {
    let mut current = root
        .try_clone()
        .with_context(|| format!("clone {label} root"))?;
    for component in components {
        current = open_existing_child_directory(&current, component, label)?;
    }
    Ok(current)
}

fn open_existing_child_directory(parent: &File, name: &str, label: &str) -> Result<File> {
    let name_c = cstring(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("open {label} component {name}"));
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    verify_owned_directory(&directory, false, label)?;
    Ok(directory)
}

fn open_optional_owned_child_directory(
    parent: &File,
    name: &str,
    label: &str,
) -> Result<Option<File>> {
    match open_existing_child_directory(parent, name, label) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if io_error_code(&error) == Some(libc::ENOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

fn ensure_owned_child_directory(parent: &File, name: &str, mode: libc::mode_t) -> Result<File> {
    let name_c = cstring(name)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), mode) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error).with_context(|| format!("create directory {name}"));
        }
    }
    open_existing_child_directory(parent, name, "Ace model destination")
}

fn create_exclusive_directory(parent: &File, name: &str, mode: libc::mode_t) -> Result<File> {
    let name_c = cstring(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), mode) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("create exclusive temporary directory {name}"));
    }
    let directory = open_existing_child_directory(parent, name, "Ace MLX temporary directory")?;
    if unsafe { libc::fchmod(directory.as_raw_fd(), mode) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("set private mode on Ace MLX temporary directory");
    }
    verify_owned_directory(&directory, true, "Ace MLX temporary directory")?;
    Ok(directory)
}

fn verify_owned_directory(directory: &File, private: bool, label: &str) -> Result<()> {
    let stat = fstat(directory.as_raw_fd()).with_context(|| format!("inspect {label}"))?;
    let mode = stat.st_mode & 0o777;
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_uid != effective_uid()
        || mode & 0o022 != 0
        || (private && mode != 0o700)
    {
        bail!(
            "{label} is not an owner-controlled directory (owner={}, expected={}, mode={mode:o})",
            stat.st_uid,
            effective_uid()
        );
    }
    Ok(())
}

fn available_space(directory: &File) -> Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("inspect Ace model filesystem space");
    }
    let stat = unsafe { stat.assume_init() };
    let available = (stat.f_bavail as u128)
        .checked_mul(stat.f_frsize as u128)
        .ok_or_else(|| anyhow!("available disk byte count overflow"))?;
    u64::try_from(available).map_err(|_| anyhow!("available disk byte count exceeds u64"))
}

fn fstat(fd: RawFd) -> std::io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

fn fstatat_nofollow(fd: RawFd, name: &CStr) -> std::io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

fn readlinkat(fd: RawFd, name: &CStr) -> std::io::Result<OsString> {
    let mut buffer = vec![0_u8; 4096];
    let read =
        unsafe { libc::readlinkat(fd, name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if read < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if read as usize == buffer.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "HF snapshot symlink target is too long",
        ));
    }
    buffer.truncate(read as usize);
    Ok(OsString::from_vec(buffer))
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn directory_entries(directory: &File) -> std::io::Result<BTreeSet<String>> {
    if unsafe { libc::lseek(directory.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    let stream = DirectoryStream(stream);
    let mut entries = BTreeSet::new();
    loop {
        set_errno(0);
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = errno();
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        let name = std::str::from_utf8(bytes).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "artifact directory contains a non-UTF-8 name",
            )
        })?;
        entries.insert(name.to_string());
    }
    Ok(entries)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(parent: &File, from: &str, to: &str) -> std::io::Result<()> {
    let from = cstring_io(from)?;
    let to = cstring_io(to)?;
    if unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_noreplace(parent: &File, from: &str, to: &str) -> std::io::Result<()> {
    let from = cstring_io(from)?;
    let to = cstring_io(to)?;
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_parent: &File, _from: &str, _to: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace directory promotion is supported only on Linux and macOS",
    ))
}

struct TempGuard {
    parent: File,
    directory: File,
    name: CString,
    file_names: Vec<CString>,
    promoted: bool,
}

impl TempGuard {
    fn new(
        parent: &File,
        directory: &File,
        name: &str,
        manifest: &[ManifestEntry],
    ) -> Result<Self> {
        Ok(Self {
            parent: parent
                .try_clone()
                .context("clone Ace MLX parent for rollback")?,
            directory: directory
                .try_clone()
                .context("clone Ace MLX temporary directory for rollback")?,
            name: cstring(name)?,
            file_names: manifest
                .iter()
                .map(|entry| cstring(entry.name))
                .collect::<Result<Vec<_>>>()?,
            promoted: false,
        })
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.promoted {
            return;
        }
        for name in &self.file_names {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
            }
        }
        unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            );
            libc::fsync(self.parent.as_raw_fd());
        }
    }
}

fn cstring(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| anyhow!("path component contains NUL"))
}

fn cstring_io(value: &str) -> std::io::Result<CString> {
    CString::new(value).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })
}

fn effective_uid() -> libc::uid_t {
    unsafe { libc::geteuid() }
}

#[cfg(target_os = "linux")]
fn errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(target_os = "linux")]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value }
}

#[cfg(target_os = "macos")]
fn errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(target_os = "macos")]
fn set_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn errno() -> libc::c_int {
    0
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn set_errno(_value: libc::c_int) {}

fn io_error_code(error: &anyhow::Error) -> Option<i32> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .and_then(std::io::Error::raw_os_error)
}

fn os_account_home() -> Result<PathBuf> {
    let uid = effective_uid();
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let buffer_len = if suggested > 0 {
        usize::try_from(suggested)
            .unwrap_or(16 * 1024)
            .clamp(1024, 1024 * 1024)
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; buffer_len];
    let mut passwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut result = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc)).context("resolve OS account home");
    }
    if result.is_null() {
        bail!("no passwd entry for effective uid {uid}");
    }
    let passwd = unsafe { passwd.assume_init() };
    if passwd.pw_dir.is_null() {
        bail!("passwd entry for effective uid {uid} has no home directory");
    }
    let bytes = unsafe { CStr::from_ptr(passwd.pw_dir) }.to_bytes();
    if bytes.is_empty() {
        bail!("passwd entry for effective uid {uid} has an empty home directory");
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    const ONE_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const TWO_SHA: &str = "486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7";
    const TINY: &[ManifestEntry] = &[
        ManifestEntry {
            name: "config.json",
            size_bytes: 5,
            sha256: ONE_SHA,
        },
        ManifestEntry {
            name: "model.safetensors",
            size_bytes: 5,
            sha256: TWO_SHA,
        },
    ];

    fn setup_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let repo = home
            .path()
            .join(".cache/huggingface/hub")
            .join(CACHE_REPO_DIR);
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::create_dir_all(repo.join("snapshots").join(HF_REVISION)).unwrap();
        for directory in [
            home.path().join(".cache"),
            home.path().join(".cache/huggingface"),
            home.path().join(".cache/huggingface/hub"),
            repo.clone(),
            repo.join("blobs"),
            repo.join("snapshots"),
            repo.join("snapshots").join(HF_REVISION),
        ] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for (entry, contents, blob_name) in [
            (
                &TINY[0],
                b"hello".as_slice(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                &TINY[1],
                b"world".as_slice(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ] {
            std::fs::write(repo.join("blobs").join(blob_name), contents).unwrap();
            std::fs::set_permissions(
                repo.join("blobs").join(blob_name),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            symlink(
                Path::new("../../blobs").join(blob_name),
                repo.join("snapshots").join(HF_REVISION).join(entry.name),
            )
            .unwrap();
        }
        home
    }

    fn ample_space() -> u64 {
        manifest_total(TINY).unwrap() + DISK_RESERVE_BYTES
    }

    #[test]
    fn production_manifest_is_exact_and_bounded() {
        validate_manifest(MANIFEST).unwrap();
        assert_eq!(MANIFEST.len(), 10);
        assert_eq!(manifest_total(MANIFEST).unwrap(), ARTIFACT_SIZE_BYTES);
        assert_eq!(expected_variant()["files"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn imports_private_regular_files_and_is_exactly_idempotent() {
        let home = setup_home();
        let first = import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
            .expect("first import");
        assert_eq!(first.disposition, ImportDisposition::Imported);
        let second = import_under(home.path(), TINY, Some(0), FailurePoint::Never)
            .expect("exact final bypasses source and disk copy");
        assert_eq!(second.disposition, ImportDisposition::AlreadyPresent);
        for entry in TINY {
            let metadata = std::fs::metadata(first.final_path.join(entry.name)).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 1);
        }
    }

    #[test]
    fn final_extra_or_content_drift_fails_closed() {
        let home = setup_home();
        let report = import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
            .expect("first import");
        std::fs::write(report.final_path.join("extra"), b"no").unwrap();
        assert!(
            import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
                .unwrap_err()
                .to_string()
                .contains("partial, extra, or drifted")
        );
        std::fs::remove_file(report.final_path.join("extra")).unwrap();
        std::fs::write(report.final_path.join("config.json"), b"HELLO").unwrap();
        assert!(import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never).is_err());
    }

    #[test]
    fn drift_after_import_is_rejected_by_descriptor_safe_verifier() {
        let home = setup_home();
        let report = import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
            .expect("first import");
        verify_installed_manifest(&report.final_path, TINY).unwrap();

        std::fs::write(report.final_path.join("config.json"), b"HELLO").unwrap();
        assert!(verify_installed_manifest(&report.final_path, TINY).is_err());
    }

    #[test]
    fn final_symlink_is_rejected_without_following_it() {
        let home = setup_home();
        let report = import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
            .expect("first import");
        let outside = home.path().join("outside");
        std::fs::write(&outside, b"hello").unwrap();
        std::fs::remove_file(report.final_path.join("config.json")).unwrap();
        symlink(&outside, report.final_path.join("config.json")).unwrap();

        assert!(import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"hello");
    }

    #[test]
    fn source_symlink_escape_and_hardlinked_blob_are_rejected() {
        let home = setup_home();
        let snapshot = home
            .path()
            .join(".cache/huggingface/hub")
            .join(CACHE_REPO_DIR)
            .join("snapshots")
            .join(HF_REVISION);
        std::fs::remove_file(snapshot.join("config.json")).unwrap();
        symlink("../../../outside", snapshot.join("config.json")).unwrap();
        assert!(import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never).is_err());

        std::fs::remove_file(snapshot.join("config.json")).unwrap();
        symlink(
            "../../blobs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            snapshot.join("config.json"),
        )
        .unwrap();
        let blob = home
            .path()
            .join(".cache/huggingface/hub")
            .join(CACHE_REPO_DIR)
            .join("blobs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        std::fs::hard_link(&blob, home.path().join("extra-hardlink")).unwrap();
        assert!(import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never).is_err());
    }

    #[test]
    fn disk_floor_is_artifact_plus_ten_gibibytes() {
        let home = setup_home();
        let required = ample_space();
        let error = import_under(home.path(), TINY, Some(required - 1), FailurePoint::Never)
            .unwrap_err()
            .to_string();
        assert!(error.contains("insufficient disk"));
        assert!(
            !home.path().join("models").exists(),
            "blocked preflight must not create destination directories"
        );
        import_under(home.path(), TINY, Some(required), FailurePoint::Never)
            .expect("exact boundary accepted");
    }

    #[test]
    fn interrupted_copy_rolls_back_temp_and_retry_succeeds() {
        let home = setup_home();
        assert!(
            import_under(
                home.path(),
                TINY,
                Some(ample_space()),
                FailurePoint::AfterFirstCopy,
            )
            .is_err()
        );
        let mlx = home.path().join("models/mlx");
        assert_eq!(std::fs::read_dir(&mlx).unwrap().count(), 0);
        import_under(home.path(), TINY, Some(ample_space()), FailurePoint::Never)
            .expect("retry succeeds");
    }

    #[test]
    fn no_replace_promotion_preserves_existing_destination() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("from")).unwrap();
        std::fs::create_dir(root.path().join("to")).unwrap();
        let directory = open_directory(root.path(), "rename test root").unwrap();
        let error = rename_noreplace(&directory, "from", "to").unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EEXIST));
        assert!(root.path().join("from").is_dir());
        assert!(root.path().join("to").is_dir());
    }

    #[test]
    fn os_home_is_absolute_and_owned_by_effective_user() {
        let home = os_account_home().unwrap();
        assert!(home.is_absolute());
        assert_eq!(std::fs::metadata(home).unwrap().uid(), effective_uid());
    }

    #[test]
    fn only_canonical_ace_identity_is_accepted() {
        assert!(ensure_ace_identity("ace").is_ok());
        assert!(ensure_ace_identity("ACE").is_ok());
        for name in ["adele", "vinny", "ace-worker", ""] {
            assert!(ensure_ace_identity(name).is_err(), "accepted {name:?}");
        }
    }
}
