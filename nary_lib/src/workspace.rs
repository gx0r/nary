use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Represents a workspace root configuration
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Path to the workspace root
    pub root_path: PathBuf,
    /// Name from root package.json
    pub name: String,
    /// Version from root package.json
    pub version: String,
    /// Discovered workspace members
    pub members: Vec<WorkspaceMember>,
}

/// A single workspace member (package)
#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    /// Package name from package.json
    pub name: String,
    /// Package version from package.json
    pub version: String,
    /// Path relative to workspace root
    pub path: PathBuf,
    /// Absolute path to the member
    pub abs_path: PathBuf,
}

impl WorkspaceConfig {
    /// Check if a path is a workspace root and load the config
    pub fn detect(root_path: &Path) -> Option<Self> {
        let pkg_json_path = root_path.join("package.json");
        let content = fs::read_to_string(&pkg_json_path).ok()?;
        let pkg: Value = serde_json::from_str(&content).ok()?;

        // Check for workspaces field
        let workspaces = pkg.get("workspaces")?;

        // Get workspace globs
        let globs = parse_workspace_globs(workspaces)?;
        if globs.is_empty() {
            return None;
        }

        let name = pkg
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("(root)")
            .to_string();
        let version = pkg
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();

        // Discover members
        let members = discover_members(root_path, &globs);

        Some(WorkspaceConfig {
            root_path: root_path.to_path_buf(),
            name,
            version,
            members,
        })
    }

    /// Get a member by name
    pub fn get_member(&self, name: &str) -> Option<&WorkspaceMember> {
        self.members.iter().find(|m| m.name == name)
    }

    /// Check if a package name is a workspace member
    pub fn is_member(&self, name: &str) -> bool {
        self.members.iter().any(|m| m.name == name)
    }
}

/// Parse workspace globs from package.json workspaces field
/// Supports both array format and object format:
/// - ["packages/*"]
/// - { "packages": ["packages/*"] }
fn parse_workspace_globs(workspaces: &Value) -> Option<Vec<String>> {
    match workspaces {
        Value::Array(arr) => {
            let globs: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            Some(globs)
        }
        Value::Object(obj) => {
            // Handle { "packages": [...] } format (yarn style)
            if let Some(packages) = obj.get("packages") {
                return parse_workspace_globs(packages);
            }
            None
        }
        _ => None,
    }
}

/// Discover workspace members from glob patterns
fn discover_members(root_path: &Path, patterns: &[String]) -> Vec<WorkspaceMember> {
    let mut members = Vec::new();

    for pattern in patterns {
        let full_pattern = root_path.join(pattern);
        let pattern_str = full_pattern.to_string_lossy();

        // Use glob to expand the pattern
        if let Ok(paths) = glob::glob(&pattern_str) {
            for entry in paths.flatten() {
                // Check if this directory has a package.json
                let pkg_json_path = entry.join("package.json");
                if !pkg_json_path.exists() {
                    continue;
                }

                // Read package.json to get name and version
                if let Ok(content) = fs::read_to_string(&pkg_json_path) {
                    if let Ok(pkg) = serde_json::from_str::<Value>(&content) {
                        let name = pkg
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or_else(|| {
                                entry.file_name().unwrap().to_str().unwrap_or("unknown")
                            })
                            .to_string();

                        let version = pkg
                            .get("version")
                            .and_then(|v| v.as_str())
                            .unwrap_or("0.0.0")
                            .to_string();

                        // Get relative path from root
                        let rel_path = entry
                            .strip_prefix(root_path)
                            .unwrap_or(&entry)
                            .to_path_buf();

                        members.push(WorkspaceMember {
                            name,
                            version,
                            path: rel_path,
                            abs_path: entry,
                        });
                    }
                }
            }
        }
    }

    // Sort by path for consistent ordering
    members.sort_by(|a, b| a.path.cmp(&b.path));
    members
}

/// Check if a dependency version uses workspace protocol
pub fn is_workspace_protocol(version: &str) -> bool {
    version.starts_with("workspace:")
}

/// Parse workspace protocol specifier
/// - "workspace:*" -> use member's exact version
/// - "workspace:^" -> use member's version with ^ prefix
/// - "workspace:~" -> use member's version with ~ prefix
/// - "workspace:^1.0.0" -> validate matches, use with ^ prefix
pub fn resolve_workspace_version(specifier: &str, member_version: &str) -> String {
    let spec = specifier.strip_prefix("workspace:").unwrap_or(specifier);

    match spec {
        "*" => member_version.to_string(),
        "^" => format!("^{}", member_version),
        "~" => format!("~{}", member_version),
        _ => {
            // Specific version like "^1.0.0" - validate it matches
            // For now, just use the specifier's version constraint
            spec.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_workspace_globs_array() {
        let json: Value = serde_json::json!(["packages/*", "apps/*"]);
        let globs = parse_workspace_globs(&json).unwrap();
        assert_eq!(globs, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_parse_workspace_globs_object() {
        let json: Value = serde_json::json!({
            "packages": ["packages/*"]
        });
        let globs = parse_workspace_globs(&json).unwrap();
        assert_eq!(globs, vec!["packages/*"]);
    }

    #[test]
    fn test_is_workspace_protocol() {
        assert!(is_workspace_protocol("workspace:*"));
        assert!(is_workspace_protocol("workspace:^"));
        assert!(is_workspace_protocol("workspace:^1.0.0"));
        assert!(!is_workspace_protocol("^1.0.0"));
        assert!(!is_workspace_protocol("*"));
    }

    #[test]
    fn test_resolve_workspace_version() {
        assert_eq!(resolve_workspace_version("workspace:*", "1.2.3"), "1.2.3");
        assert_eq!(resolve_workspace_version("workspace:^", "1.2.3"), "^1.2.3");
        assert_eq!(resolve_workspace_version("workspace:~", "1.2.3"), "~1.2.3");
        assert_eq!(
            resolve_workspace_version("workspace:^1.0.0", "1.2.3"),
            "^1.0.0"
        );
    }

    #[test]
    fn test_parse_workspace_globs_invalid() {
        // String value (not array or object)
        let json: Value = serde_json::json!("packages/*");
        assert!(parse_workspace_globs(&json).is_none());

        // Null value
        let json: Value = serde_json::json!(null);
        assert!(parse_workspace_globs(&json).is_none());

        // Number value
        let json: Value = serde_json::json!(42);
        assert!(parse_workspace_globs(&json).is_none());

        // Boolean value
        let json: Value = serde_json::json!(true);
        assert!(parse_workspace_globs(&json).is_none());
    }

    #[test]
    fn test_parse_workspace_globs_empty_array() {
        let json: Value = serde_json::json!([]);
        let globs = parse_workspace_globs(&json).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn test_parse_workspace_globs_mixed_types() {
        // Array with mixed types - should only pick up strings
        let json: Value = serde_json::json!(["packages/*", 123, null, "apps/*"]);
        let globs = parse_workspace_globs(&json).unwrap();
        assert_eq!(globs, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_parse_workspace_globs_nested_object() {
        // Object without "packages" key
        let json: Value = serde_json::json!({
            "nohoist": ["**/react"]
        });
        assert!(parse_workspace_globs(&json).is_none());
    }

    #[test]
    fn test_parse_workspace_globs_object_with_empty_packages() {
        let json: Value = serde_json::json!({
            "packages": []
        });
        let globs = parse_workspace_globs(&json).unwrap();
        assert!(globs.is_empty());
    }

    #[test]
    fn test_is_workspace_protocol_edge_cases() {
        // Edge cases
        assert!(is_workspace_protocol("workspace:"));
        assert!(!is_workspace_protocol(""));
        assert!(!is_workspace_protocol("workspac:*")); // typo
        assert!(!is_workspace_protocol("Workspace:*")); // case sensitive
        assert!(!is_workspace_protocol(" workspace:*")); // leading space
    }

    #[test]
    fn test_resolve_workspace_version_edge_cases() {
        // Without workspace: prefix (shouldn't happen but handle gracefully)
        assert_eq!(resolve_workspace_version("*", "1.0.0"), "1.0.0");
        assert_eq!(resolve_workspace_version("^", "1.0.0"), "^1.0.0");
        assert_eq!(resolve_workspace_version("~", "1.0.0"), "~1.0.0");

        // Complex version specifiers
        assert_eq!(
            resolve_workspace_version("workspace:>=1.0.0", "2.0.0"),
            ">=1.0.0"
        );
        assert_eq!(
            resolve_workspace_version("workspace:1.0.0 - 2.0.0", "1.5.0"),
            "1.0.0 - 2.0.0"
        );

        // Prerelease versions
        assert_eq!(
            resolve_workspace_version("workspace:*", "1.0.0-beta.1"),
            "1.0.0-beta.1"
        );
        assert_eq!(
            resolve_workspace_version("workspace:^", "2.0.0-alpha"),
            "^2.0.0-alpha"
        );
    }

    #[test]
    fn test_workspace_config_get_member() {
        let config = WorkspaceConfig {
            root_path: PathBuf::from("/test"),
            name: "test-workspace".to_string(),
            version: "1.0.0".to_string(),
            members: vec![
                WorkspaceMember {
                    name: "@scope/pkg-a".to_string(),
                    version: "1.0.0".to_string(),
                    path: PathBuf::from("packages/pkg-a"),
                    abs_path: PathBuf::from("/test/packages/pkg-a"),
                },
                WorkspaceMember {
                    name: "pkg-b".to_string(),
                    version: "2.0.0".to_string(),
                    path: PathBuf::from("packages/pkg-b"),
                    abs_path: PathBuf::from("/test/packages/pkg-b"),
                },
            ],
        };

        // Find existing member
        let member = config.get_member("@scope/pkg-a");
        assert!(member.is_some());
        assert_eq!(member.unwrap().version, "1.0.0");

        let member = config.get_member("pkg-b");
        assert!(member.is_some());
        assert_eq!(member.unwrap().version, "2.0.0");

        // Non-existent member
        assert!(config.get_member("nonexistent").is_none());
    }

    #[test]
    fn test_workspace_config_is_member() {
        let config = WorkspaceConfig {
            root_path: PathBuf::from("/test"),
            name: "test-workspace".to_string(),
            version: "1.0.0".to_string(),
            members: vec![WorkspaceMember {
                name: "pkg-a".to_string(),
                version: "1.0.0".to_string(),
                path: PathBuf::from("packages/pkg-a"),
                abs_path: PathBuf::from("/test/packages/pkg-a"),
            }],
        };

        assert!(config.is_member("pkg-a"));
        assert!(!config.is_member("pkg-b"));
        assert!(!config.is_member(""));
    }

    #[test]
    fn test_workspace_config_detect_no_package_json() {
        // Test with a path that doesn't exist or has no package.json
        let result = WorkspaceConfig::detect(Path::new("/nonexistent/path"));
        assert!(result.is_none());
    }

    #[test]
    fn test_workspace_member_scoped_package() {
        let member = WorkspaceMember {
            name: "@myorg/shared-utils".to_string(),
            version: "0.1.0".to_string(),
            path: PathBuf::from("packages/shared-utils"),
            abs_path: PathBuf::from("/workspace/packages/shared-utils"),
        };

        assert!(member.name.starts_with('@'));
        assert!(member.name.contains('/'));
    }
}
