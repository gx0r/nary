mod npmrc;
mod registry;

use std::path::PathBuf;

pub use npmrc::NpmrcConfig;
pub use registry::RegistryConfig;

/// Default npm registry
pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Get the global nary directory (~/.nary)
///
/// Uses HOME environment variable on Unix, USERPROFILE on Windows.
pub fn get_global_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".nary"))
}
