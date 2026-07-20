//! Cache eviction — the artifact-cache janitor.
//!
//! Public home for evicting old artifact versions: [`evaluate_artifact_eviction`]
//! walks a cache root laid out as `<root>/<artifact>/<version>/…` and removes
//! every version beyond the newest [`ArtifactEvictionPolicy::keep_latest`] per
//! artifact. The implementation remains available through
//! [`crate::artifact_cache`] for backwards compatibility.

pub use crate::artifact_cache::{
    ArtifactEvictionPolicy, DEFAULT_KEEP_LATEST, evaluate_artifact_eviction,
    spawn_artifact_eviction_loop,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_janitor_is_available_from_cache_module() {
        let root = tempfile::tempdir().unwrap();
        let policy = ArtifactEvictionPolicy::new(2);
        assert_eq!(policy.keep_latest(), 2);
        assert_eq!(evaluate_artifact_eviction(root.path(), &policy).unwrap(), 0);
    }
}
