//! Artifact cache management.
//!
//! This module is the public home for checking the local artifact cache,
//! downloading cache misses from the WAN, and transferring artifacts over the
//! LAN. The implementation remains available through [`crate::artifact_fetch`]
//! for backwards compatibility.

pub use crate::artifact_fetch::{
    ArtifactCacheManager, FetchSource, LanPeer, default_artifact_cache_root, sha256_file,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_is_available_from_cache_module() {
        let root = tempfile::tempdir().unwrap();
        let manager = ArtifactCacheManager::new(root.path().to_path_buf());

        assert_eq!(manager.root(), root.path());
        assert_eq!(
            manager
                .check_cache("ff-agent", "v1", "ff-agent", None)
                .unwrap(),
            None
        );
    }
}
