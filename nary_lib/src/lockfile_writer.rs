use indexmap::IndexMap;
use snafu::ResultExt;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::deps::{Dependency, ResolvedInfo};
use crate::error::{FileWriteSnafu, JsonSerializeSnafu, Result};
use crate::lockfile::{PackageEntry, PackageLock};

/// Type alias for backward compatibility
pub type PackageLockWrite = PackageLock;
/// Type alias for backward compatibility
pub type PackageEntryWrite = PackageEntry;

/// Build a PackageLock from resolved dependencies
pub fn build_package_lock(
    root_name: &str,
    root_version: &str,
    dependencies: &IndexMap<Dependency, ResolvedInfo>,
) -> PackageLock {
    let mut packages = IndexMap::new();

    // Add root package entry (empty key "")
    packages.insert(
        String::new(),
        PackageEntry {
            version: Some(root_version.to_string()),
            ..Default::default()
        },
    );

    // Build a set of installed package names for filtering dependencies
    let installed_names: std::collections::HashSet<&String> =
        dependencies.iter().map(|(dep, _)| &dep.name).collect();

    // Add all dependencies
    for (dep, info) in dependencies {
        // Use install_path directly as the key (supports nested node_modules)
        let key = info.install_path.clone();
        // Convert dependencies Vec to IndexMap, filtering to only include installed packages
        let mut deps = IndexMap::new();
        for (name, version) in &info.dependencies {
            if installed_names.contains(name) {
                deps.insert(name.clone(), version.clone());
            }
        }
        // For aliased packages, include the actual package name
        let name = if dep.alias.is_some() {
            Some(dep.name.clone())
        } else {
            None
        };

        packages.insert(
            key,
            PackageEntry {
                name,
                version: Some(dep.resolved.clone()),
                resolved: info.tarball_url.clone(),
                integrity: info.integrity.clone(),
                optional: dep.is_optional,
                dev: false,
                dependencies: deps,
            },
        );
    }

    PackageLock {
        name: Some(root_name.to_string()),
        version: Some(root_version.to_string()),
        lockfile_version: 3,
        requires: true,
        packages,
    }
}

/// Write package-lock.json to disk
pub fn write_package_lock(path: &Path, lock: &PackageLockWrite) -> Result<()> {
    let file = File::create(path).context(FileWriteSnafu {
        path: path.to_path_buf(),
    })?;
    let writer = BufWriter::new(file);
    serde_json::to_writer_pretty(writer, lock).context(JsonSerializeSnafu)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_dep(name: &str, version: &str, optional: bool) -> Dependency {
        Dependency {
            name: name.to_string(),
            requested: format!("^{}", version),
            resolved: version.to_string(),
            is_optional: optional,
            alias: None,
        }
    }

    fn make_info(path: &str, tarball: Option<&str>, integrity: Option<&str>) -> ResolvedInfo {
        ResolvedInfo {
            tarball_url: tarball.map(String::from),
            integrity: integrity.map(String::from),
            dependencies: vec![],
            install_path: path.to_string(),
            deprecated: None,
        }
    }

    fn make_info_with_deps(path: &str, deps: Vec<(String, String)>) -> ResolvedInfo {
        ResolvedInfo {
            tarball_url: Some("https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz".to_string()),
            integrity: Some("sha512-abc".to_string()),
            dependencies: deps,
            install_path: path.to_string(),
            deprecated: None,
        }
    }

    #[test]
    fn test_build_empty_deps() {
        let deps = IndexMap::new();
        let lock = build_package_lock("my-app", "1.0.0", &deps);

        assert_eq!(lock.name, Some("my-app".to_string()));
        assert_eq!(lock.version, Some("1.0.0".to_string()));
        assert_eq!(lock.lockfile_version, 3);
        assert!(lock.requires);
        assert_eq!(lock.packages.len(), 1); // Only root entry
        assert!(lock.packages.contains_key(""));
    }

    #[test]
    fn test_build_single_dep() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("lodash", "4.17.21", false),
            make_info(
                "node_modules/lodash",
                Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"),
                Some("sha512-abc"),
            ),
        );

        let lock = build_package_lock("my-app", "1.0.0", &deps);

        assert_eq!(lock.packages.len(), 2);
        let lodash = lock.packages.get("node_modules/lodash").unwrap();
        assert_eq!(lodash.version, Some("4.17.21".to_string()));
        assert!(lodash.resolved.is_some());
        assert!(lodash.integrity.is_some());
        assert!(!lodash.optional); // false
    }

    #[test]
    fn test_build_multiple_deps_preserves_order() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("zod", "3.0.0", false),
            make_info("node_modules/zod", None, None),
        );
        deps.insert(
            make_dep("axios", "1.0.0", false),
            make_info("node_modules/axios", None, None),
        );
        deps.insert(
            make_dep("react", "18.0.0", false),
            make_info("node_modules/react", None, None),
        );

        let lock = build_package_lock("app", "1.0.0", &deps);

        let keys: Vec<_> = lock.packages.keys().collect();
        // Root entry first, then in insertion order
        assert_eq!(keys[0], "");
        assert_eq!(keys[1], "node_modules/zod");
        assert_eq!(keys[2], "node_modules/axios");
        assert_eq!(keys[3], "node_modules/react");
    }

    #[test]
    fn test_build_nested_paths() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("express", "4.18.0", false),
            make_info("node_modules/express", None, None),
        );
        deps.insert(
            make_dep("qs", "6.11.0", false),
            make_info("node_modules/express/node_modules/qs", None, None),
        );

        let lock = build_package_lock("app", "1.0.0", &deps);

        assert!(lock.packages.contains_key("node_modules/express"));
        assert!(lock
            .packages
            .contains_key("node_modules/express/node_modules/qs"));
    }

    #[test]
    fn test_build_optional_flag() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("fsevents", "2.3.0", true), // optional = true
            make_info("node_modules/fsevents", None, None),
        );
        deps.insert(
            make_dep("lodash", "4.17.0", false), // optional = false
            make_info("node_modules/lodash", None, None),
        );

        let lock = build_package_lock("app", "1.0.0", &deps);

        let fsevents = lock.packages.get("node_modules/fsevents").unwrap();
        assert!(fsevents.optional);

        let lodash = lock.packages.get("node_modules/lodash").unwrap();
        assert!(!lodash.optional);
    }

    #[test]
    fn test_build_filters_uninstalled_deps() {
        let mut deps = IndexMap::new();
        // Package A depends on B (installed) and C (not installed)
        deps.insert(
            make_dep("pkg-a", "1.0.0", false),
            make_info_with_deps(
                "node_modules/pkg-a",
                vec![
                    ("pkg-b".to_string(), "^2.0.0".to_string()),
                    ("pkg-c".to_string(), "^3.0.0".to_string()), // not in deps
                ],
            ),
        );
        deps.insert(
            make_dep("pkg-b", "2.0.0", false),
            make_info("node_modules/pkg-b", None, None),
        );
        // pkg-c is NOT in deps (not installed)

        let lock = build_package_lock("app", "1.0.0", &deps);

        let pkg_a = lock.packages.get("node_modules/pkg-a").unwrap();
        assert!(pkg_a.dependencies.contains_key("pkg-b"));
        assert!(!pkg_a.dependencies.contains_key("pkg-c")); // filtered out
    }

    #[test]
    fn test_build_scoped_packages() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("@types/node", "18.0.0", false),
            make_info("node_modules/@types/node", None, None),
        );
        deps.insert(
            make_dep("@babel/core", "7.20.0", false),
            make_info("node_modules/@babel/core", None, None),
        );

        let lock = build_package_lock("app", "1.0.0", &deps);

        assert!(lock.packages.contains_key("node_modules/@types/node"));
        assert!(lock.packages.contains_key("node_modules/@babel/core"));
    }

    #[test]
    fn test_build_with_integrity() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("lodash", "4.17.21", false),
            make_info(
                "node_modules/lodash",
                Some("https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz"),
                Some("sha512-v2kDEe57lecTulaDIuNTPy3Ry4gLGJ6Z1O3vE1krgXZNrsQ+LFTGHVxVjcXPs17LhbZVGedAJv8XZ1tvj5FvSg=="),
            ),
        );

        let lock = build_package_lock("app", "1.0.0", &deps);

        let lodash = lock.packages.get("node_modules/lodash").unwrap();
        assert!(lodash.integrity.as_ref().unwrap().starts_with("sha512-"));
    }

    #[test]
    fn test_write_creates_file() {
        let mut deps = IndexMap::new();
        deps.insert(
            make_dep("test-pkg", "1.0.0", false),
            make_info("node_modules/test-pkg", None, None),
        );

        let lock = build_package_lock("test-app", "0.1.0", &deps);

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("package-lock.json");

        write_package_lock(&path, &lock).unwrap();

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["name"], "test-app");
        assert_eq!(parsed["lockfileVersion"], 3);
    }

    #[test]
    fn test_write_pretty_printed() {
        let deps = IndexMap::new();
        let lock = build_package_lock("app", "1.0.0", &deps);

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("package-lock.json");

        write_package_lock(&path, &lock).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // Pretty-printed JSON should have newlines
        assert!(content.contains('\n'));
        // Should have indentation
        assert!(content.contains("  "));
    }
}
