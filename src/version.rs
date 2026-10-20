//! Which build this is (#466): the crate version and the commit `build.rs`
//! stamped. Reported on `/api/v1/health`, `/api/v1/version` and `/metrics`
//! (`stormblock_build_info`), so a caller can tell an engine was upgraded.

/// The crate version (moves only at a tag).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit the binary was built from, 12 hex digits, or `unknown`.
pub const COMMIT: &str = env!("STORMBLOCK_COMMIT");
