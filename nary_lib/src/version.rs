//! Version manipulation utilities for semantic versioning operations.

use node_semver::{Identifier, Range, Version};
use serde_json::Value;
use snafu::{ResultExt, Snafu};

/// Errors that can occur during version operations
#[derive(Debug, Snafu)]
pub enum VersionError {
    #[snafu(display("Failed to parse version '{}': {}", version, source))]
    ParseVersion {
        version: String,
        source: node_semver::SemverError,
        backtrace: snafu::Backtrace,
    },
    #[snafu(display(
        "Invalid version bump: {}. Use major, minor, patch, premajor, preminor, prepatch, prerelease, or an explicit version.",
        bump
    ))]
    InvalidBump { bump: String },
}

pub type Result<T> = std::result::Result<T, VersionError>;

/// Bump a version according to the specified bump type.
///
/// Supported bump types:
/// - `major`: Increment major version (1.2.3 -> 2.0.0)
/// - `minor`: Increment minor version (1.2.3 -> 1.3.0)
/// - `patch`: Increment patch version (1.2.3 -> 1.2.4)
/// - `premajor`: Increment major with prerelease (1.2.3 -> 2.0.0-0.0)
/// - `preminor`: Increment minor with prerelease (1.2.3 -> 1.3.0-0.0)
/// - `prepatch`: Increment patch with prerelease (1.2.3 -> 1.2.4-0.0)
/// - `prerelease`: Increment prerelease number (1.2.3-alpha.0 -> 1.2.3-alpha.1)
/// - Any valid semver: Use that version directly
///
/// # Arguments
/// * `current` - The current version string
/// * `bump` - The bump type or explicit version
/// * `preid` - Optional prerelease identifier (e.g., "alpha", "beta")
pub fn bump_version(current: &str, bump: &str, preid: Option<&str>) -> Result<String> {
    let version: Version = current.parse().context(ParseVersionSnafu {
        version: current.to_string(),
    })?;

    let (major, minor, patch) = (version.major, version.minor, version.patch);

    match bump {
        "major" => Ok(format!("{}.0.0", major + 1)),
        "minor" => Ok(format!("{}.{}.0", major, minor + 1)),
        "patch" => Ok(format!("{}.{}.{}", major, minor, patch + 1)),
        "premajor" => {
            let pre = preid.unwrap_or("0");
            Ok(format!("{}.0.0-{}.0", major + 1, pre))
        }
        "preminor" => {
            let pre = preid.unwrap_or("0");
            Ok(format!("{}.{}.0-{}.0", major, minor + 1, pre))
        }
        "prepatch" => {
            let pre = preid.unwrap_or("0");
            Ok(format!("{}.{}.{}-{}.0", major, minor, patch + 1, pre))
        }
        "prerelease" => {
            // Increment prerelease number or add one
            let pre_release = &version.pre_release;
            if pre_release.len() >= 2 {
                let pre_id = &pre_release[0];
                let pre_num: u64 = match &pre_release[1] {
                    Identifier::Numeric(n) => *n,
                    Identifier::AlphaNumeric(s) => s.parse().unwrap_or(0),
                };
                return Ok(format!(
                    "{}.{}.{}-{}.{}",
                    major,
                    minor,
                    patch,
                    pre_id,
                    pre_num + 1
                ));
            }
            let pre = preid.unwrap_or("0");
            Ok(format!("{}.{}.{}-{}.0", major, minor, patch + 1, pre))
        }
        _ => {
            // Assume it's an explicit version - validate it parses
            if bump.parse::<Version>().is_ok() {
                Ok(bump.to_string())
            } else {
                InvalidBumpSnafu {
                    bump: bump.to_string(),
                }
                .fail()
            }
        }
    }
}

/// Check if a version has a prerelease tag
#[inline]
pub fn has_prerelease(version: &Version) -> bool {
    version.is_prerelease()
}

/// Find the maximum version from package metadata that satisfies a semver range.
///
/// # Arguments
/// * `metadata` - Package metadata JSON containing a "versions" object
/// * `range_str` - Semver range string (e.g., "^1.0.0", ">=2.0.0 <3.0.0")
///
/// # Returns
/// The highest version string that satisfies the range, or None if no match found.
pub fn find_max_satisfying_version(metadata: &Value, range_str: &str) -> Option<String> {
    let versions = metadata.get("versions")?.as_object()?;
    let range_str = range_str.trim();

    // For exact versions, just return if it exists
    if versions.contains_key(range_str) {
        if let Ok(version) = range_str.parse::<Version>() {
            if !has_prerelease(&version) || range_str.contains('-') {
                return Some(range_str.to_string());
            }
        }
    }

    // Parse the range using node_semver
    let range: Range = match range_str.parse() {
        Ok(r) => r,
        Err(_) => return None,
    };

    // Collect and filter versions
    let mut valid_versions: Vec<(Version, String)> = versions
        .keys()
        .filter_map(|v| {
            let parsed: Version = v.parse().ok()?;
            // Skip prereleases unless range explicitly includes them
            if has_prerelease(&parsed) && !range_str.contains('-') {
                return None;
            }
            if parsed.satisfies(&range) {
                Some((parsed, v.clone()))
            } else {
                None
            }
        })
        .collect();

    // Sort descending by semver and return highest
    valid_versions.sort_by(|a, b| b.0.cmp(&a.0));
    valid_versions.first().map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_bump_major() {
        assert_eq!(bump_version("1.2.3", "major", None).unwrap(), "2.0.0");
        assert_eq!(bump_version("0.0.1", "major", None).unwrap(), "1.0.0");
    }

    #[test]
    fn test_bump_minor() {
        assert_eq!(bump_version("1.2.3", "minor", None).unwrap(), "1.3.0");
        assert_eq!(bump_version("0.0.1", "minor", None).unwrap(), "0.1.0");
    }

    #[test]
    fn test_bump_patch() {
        assert_eq!(bump_version("1.2.3", "patch", None).unwrap(), "1.2.4");
        assert_eq!(bump_version("0.0.0", "patch", None).unwrap(), "0.0.1");
    }

    #[test]
    fn test_bump_premajor() {
        assert_eq!(
            bump_version("1.2.3", "premajor", None).unwrap(),
            "2.0.0-0.0"
        );
        assert_eq!(
            bump_version("1.2.3", "premajor", Some("alpha")).unwrap(),
            "2.0.0-alpha.0"
        );
    }

    #[test]
    fn test_bump_preminor() {
        assert_eq!(
            bump_version("1.2.3", "preminor", None).unwrap(),
            "1.3.0-0.0"
        );
        assert_eq!(
            bump_version("1.2.3", "preminor", Some("beta")).unwrap(),
            "1.3.0-beta.0"
        );
    }

    #[test]
    fn test_bump_prepatch() {
        assert_eq!(
            bump_version("1.2.3", "prepatch", None).unwrap(),
            "1.2.4-0.0"
        );
        assert_eq!(
            bump_version("1.2.3", "prepatch", Some("rc")).unwrap(),
            "1.2.4-rc.0"
        );
    }

    #[test]
    fn test_bump_prerelease_increment() {
        assert_eq!(
            bump_version("1.2.3-alpha.0", "prerelease", None).unwrap(),
            "1.2.3-alpha.1"
        );
        assert_eq!(
            bump_version("1.2.3-beta.5", "prerelease", None).unwrap(),
            "1.2.3-beta.6"
        );
    }

    #[test]
    fn test_bump_prerelease_new() {
        assert_eq!(
            bump_version("1.2.3", "prerelease", None).unwrap(),
            "1.2.4-0.0"
        );
        assert_eq!(
            bump_version("1.2.3", "prerelease", Some("alpha")).unwrap(),
            "1.2.4-alpha.0"
        );
    }

    #[test]
    fn test_bump_explicit_version() {
        assert_eq!(bump_version("1.2.3", "5.0.0", None).unwrap(), "5.0.0");
        assert_eq!(
            bump_version("1.2.3", "2.0.0-beta.1", None).unwrap(),
            "2.0.0-beta.1"
        );
    }

    #[test]
    fn test_bump_invalid() {
        assert!(bump_version("1.2.3", "invalid", None).is_err());
        assert!(bump_version("1.2.3", "not-a-version", None).is_err());
    }

    #[test]
    fn test_bump_invalid_current() {
        assert!(bump_version("not-a-version", "patch", None).is_err());
    }

    #[test]
    fn test_find_max_satisfying_exact() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.1.0": {},
                "2.0.0": {}
            }
        });
        assert_eq!(
            find_max_satisfying_version(&metadata, "1.1.0"),
            Some("1.1.0".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_caret() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.1.0": {},
                "1.2.0": {},
                "2.0.0": {}
            }
        });
        assert_eq!(
            find_max_satisfying_version(&metadata, "^1.0.0"),
            Some("1.2.0".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_tilde() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.0.5": {},
                "1.1.0": {},
                "1.2.0": {}
            }
        });
        assert_eq!(
            find_max_satisfying_version(&metadata, "~1.0.0"),
            Some("1.0.5".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_range() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "2.0.0": {},
                "2.5.0": {},
                "3.0.0": {}
            }
        });
        assert_eq!(
            find_max_satisfying_version(&metadata, ">=2.0.0 <3.0.0"),
            Some("2.5.0".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_no_match() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "2.0.0": {}
            }
        });
        assert_eq!(find_max_satisfying_version(&metadata, "^3.0.0"), None);
    }

    #[test]
    fn test_find_max_satisfying_excludes_prerelease() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.1.0-alpha.1": {},
                "1.1.0": {}
            }
        });
        // Should skip prerelease unless range includes it
        assert_eq!(
            find_max_satisfying_version(&metadata, "^1.0.0"),
            Some("1.1.0".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_includes_prerelease_when_specified() {
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.1.0-alpha.1": {},
                "1.1.0-alpha.2": {}
            }
        });
        // Range with prerelease should match prereleases
        assert_eq!(
            find_max_satisfying_version(&metadata, "1.1.0-alpha.1"),
            Some("1.1.0-alpha.1".to_string())
        );
    }

    #[test]
    fn test_find_max_satisfying_empty_versions() {
        let metadata = json!({
            "versions": {}
        });
        assert_eq!(find_max_satisfying_version(&metadata, "^1.0.0"), None);
    }

    #[test]
    fn test_find_max_satisfying_invalid_range() {
        let metadata = json!({
            "versions": {
                "1.0.0": {}
            }
        });
        assert_eq!(find_max_satisfying_version(&metadata, "not-a-range"), None);
    }
}
