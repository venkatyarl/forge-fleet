//! Library target for the ForgeFleet CLI package.
//!
//! The executable remains implemented by `main.rs`; this target exists so
//! package-scoped library verification can treat every workspace package
//! uniformly.

/// Cargo package name for the ForgeFleet CLI.
pub const PACKAGE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::PACKAGE_NAME;

    #[test]
    fn package_name_matches_manifest() {
        assert_eq!(PACKAGE_NAME, "ff-cli");
    }
}
