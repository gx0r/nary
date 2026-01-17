use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::deps::{Dependency, ResolvedInfo};

/// Helper function for skip_serializing_if on bool fields
fn is_false(b: &bool) -> bool {
    !*b
}

/// npm package-lock.json format (v2/v3)
/// Used for both reading and writing lockfiles
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageLock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    pub lockfile_version: u32,

    #[serde(default, skip_serializing_if = "is_false")]
    pub requires: bool,

    #[serde(default)]
    pub packages: IndexMap<String, PackageEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageEntry {
    /// The actual package name (used for aliased packages where path differs from name)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,

    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,

    #[serde(default, skip_serializing_if = "is_false")]
    pub dev: bool,

    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub dependencies: IndexMap<String, String>,
}

/// Read and parse package-lock.json if it exists
pub fn read_package_lock(path: &Path) -> Option<PackageLock> {
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).ok()
}

/// Convert package-lock.json packages to our Dependency format
/// Returns dependencies in installation order (we just use insertion order)
pub fn deps_from_lockfile(lock: &PackageLock) -> IndexMap<Dependency, ResolvedInfo> {
    let mut result = IndexMap::new();

    for (key, entry) in &lock.packages {
        // Skip the root entry (empty key "")
        if key.is_empty() {
            continue;
        }

        // Extract package name from key, handling nested paths:
        // "node_modules/lodash" -> "lodash"
        // "node_modules/express/node_modules/lodash" -> "lodash"
        // "node_modules/@scope/pkg" -> "@scope/pkg"
        let path_name = key
            .rsplit("node_modules/")
            .next()
            .unwrap_or(key)
            .to_string();

        // Skip if missing required fields
        let version = match &entry.version {
            Some(v) => v.clone(),
            None => continue,
        };

        // Handle aliased packages: if entry.name differs from path-derived name,
        // the path is the alias and entry.name is the actual package name
        let (name, alias) = match &entry.name {
            Some(actual_name) if actual_name != &path_name => {
                (actual_name.clone(), Some(path_name))
            }
            _ => (path_name, None),
        };

        let dep = Dependency {
            name,
            requested: version.clone(), // In lockfile, requested == resolved
            resolved: version,
            is_optional: entry.optional,
            alias,
            install_path: Some(key.clone()), // Include install path in identity for deduplication
        };

        // Convert dependencies to Vec<(String, String)>
        let deps: Vec<(String, String)> = entry
            .dependencies
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let info = ResolvedInfo {
            tarball_url: entry.resolved.clone(),
            integrity: entry.integrity.clone(),
            dependencies: deps,
            install_path: key.clone(), // Preserve full install path including nested paths
            deprecated: None,          // Not tracked in lockfile
            maturity_fallback: None,   // Not tracked in lockfile
        };
        result.insert(dep, info);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(version: &str) -> PackageEntry {
        PackageEntry {
            name: None,
            version: Some(version.to_string()),
            resolved: Some(format!(
                "https://registry.npmjs.org/pkg/-/pkg-{}.tgz",
                version
            )),
            integrity: Some("sha512-abc123".to_string()),
            optional: false,
            dev: false,
            dependencies: IndexMap::new(),
        }
    }

    fn make_optional_entry(version: &str) -> PackageEntry {
        PackageEntry {
            name: None,
            version: Some(version.to_string()),
            resolved: None,
            integrity: None,
            optional: true,
            dev: false,
            dependencies: IndexMap::new(),
        }
    }

    #[test]
    fn test_deps_from_lockfile_empty() {
        // Lockfile with only root entry
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_deps_from_lockfile_simple_flat() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert("node_modules/lodash".to_string(), make_entry("4.17.21"));
        packages.insert("node_modules/express".to_string(), make_entry("4.18.2"));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert_eq!(deps.len(), 2);

        let names: Vec<&str> = deps.keys().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"lodash"));
        assert!(names.contains(&"express"));

        // Check install paths are preserved
        let lodash = deps.iter().find(|(d, _)| d.name == "lodash").unwrap();
        assert_eq!(lodash.1.install_path, "node_modules/lodash");
    }

    #[test]
    fn test_deps_from_lockfile_nested_deps() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert("node_modules/express".to_string(), make_entry("4.18.2"));
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            make_entry("6.11.0"),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.12.0"));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert_eq!(deps.len(), 3);

        // Both qs versions should be present with different install paths
        let qs_entries: Vec<_> = deps.iter().filter(|(d, _)| d.name == "qs").collect();
        assert_eq!(qs_entries.len(), 2);

        let paths: Vec<&str> = qs_entries
            .iter()
            .map(|(_, i)| i.install_path.as_str())
            .collect();
        assert!(paths.contains(&"node_modules/qs"));
        assert!(paths.contains(&"node_modules/express/node_modules/qs"));
    }

    #[test]
    fn test_deps_from_lockfile_same_version_multiple_paths() {
        // Test case: same package@version installed at multiple paths
        // This happens when different parent packages need conflicting peer deps
        // e.g., tinyglobby and vite both need fdir@6.5.0 but at different paths
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert("node_modules/tinyglobby".to_string(), make_entry("0.2.15"));
        packages.insert("node_modules/vite".to_string(), make_entry("6.0.0"));
        // Same version of fdir at two different nested paths
        packages.insert(
            "node_modules/tinyglobby/node_modules/fdir".to_string(),
            make_entry("6.5.0"),
        );
        packages.insert(
            "node_modules/vite/node_modules/fdir".to_string(),
            make_entry("6.5.0"),
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);

        // Should have all 4 packages: tinyglobby, vite, and both fdir instances
        assert_eq!(deps.len(), 4);

        // Both fdir entries should be preserved despite same name+version
        let fdir_entries: Vec<_> = deps.iter().filter(|(d, _)| d.name == "fdir").collect();
        assert_eq!(
            fdir_entries.len(),
            2,
            "Both fdir instances should be preserved"
        );

        // Verify both install paths are present
        let fdir_paths: Vec<&str> = fdir_entries
            .iter()
            .map(|(_, i)| i.install_path.as_str())
            .collect();
        assert!(
            fdir_paths.contains(&"node_modules/tinyglobby/node_modules/fdir"),
            "tinyglobby's fdir should be present"
        );
        assert!(
            fdir_paths.contains(&"node_modules/vite/node_modules/fdir"),
            "vite's fdir should be present"
        );

        // Verify install_path is part of Dependency identity
        let fdir1 = deps
            .iter()
            .find(|(d, _)| {
                d.name == "fdir"
                    && d.install_path
                        .as_ref()
                        .map_or(false, |p| p.contains("tinyglobby"))
            })
            .expect("Should find tinyglobby's fdir");
        let fdir2 = deps
            .iter()
            .find(|(d, _)| {
                d.name == "fdir"
                    && d.install_path
                        .as_ref()
                        .map_or(false, |p| p.contains("vite"))
            })
            .expect("Should find vite's fdir");

        // They should be considered different dependencies due to install_path
        assert_ne!(
            fdir1.0, fdir2.0,
            "fdir at different paths should be different Dependencies"
        );
    }

    #[test]
    fn test_deps_from_lockfile_scoped_packages() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert(
            "node_modules/@types/node".to_string(),
            make_entry("20.10.0"),
        );
        packages.insert("node_modules/@babel/core".to_string(), make_entry("7.23.0"));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert_eq!(deps.len(), 2);

        let names: Vec<&str> = deps.keys().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"@types/node"));
        assert!(names.contains(&"@babel/core"));
    }

    #[test]
    fn test_deps_from_lockfile_optional() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert("node_modules/lodash".to_string(), make_entry("4.17.21"));
        packages.insert(
            "node_modules/fsevents".to_string(),
            make_optional_entry("2.3.3"),
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert_eq!(deps.len(), 2);

        let fsevents = deps.iter().find(|(d, _)| d.name == "fsevents").unwrap();
        assert!(fsevents.0.is_optional);

        let lodash = deps.iter().find(|(d, _)| d.name == "lodash").unwrap();
        assert!(!lodash.0.is_optional);
    }

    #[test]
    fn test_deps_from_lockfile_skips_missing_version() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());
        packages.insert("node_modules/lodash".to_string(), make_entry("4.17.21"));
        packages.insert(
            "node_modules/broken".to_string(),
            PackageEntry {
                version: None, // Missing version
                ..Default::default()
            },
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps.keys().next().unwrap().name, "lodash");
    }

    #[test]
    fn test_deps_from_lockfile_with_dependencies() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), PackageEntry::default());

        let mut express_deps = IndexMap::new();
        express_deps.insert("body-parser".to_string(), "^1.20.0".to_string());
        express_deps.insert("cookie".to_string(), "^0.5.0".to_string());

        packages.insert(
            "node_modules/express".to_string(),
            PackageEntry {
                name: None,
                version: Some("4.18.2".to_string()),
                resolved: Some(
                    "https://registry.npmjs.org/express/-/express-4.18.2.tgz".to_string(),
                ),
                integrity: Some("sha512-abc".to_string()),
                optional: false,
                dev: false,
                dependencies: express_deps,
            },
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let deps = deps_from_lockfile(&lock);
        let express = deps.iter().find(|(d, _)| d.name == "express").unwrap();

        assert_eq!(express.1.dependencies.len(), 2);
        let dep_names: Vec<&str> = express
            .1
            .dependencies
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert!(dep_names.contains(&"body-parser"));
        assert!(dep_names.contains(&"cookie"));
    }

    #[test]
    fn test_lockfile_round_trip_maturity_fallback_none() {
        use crate::lockfile_writer::{build_package_lock, write_package_lock};
        use tempfile::TempDir;

        // Create dependencies with maturity_fallback set (simulating a fallback scenario)
        let mut deps = IndexMap::new();
        deps.insert(
            Dependency {
                name: "lodash".to_string(),
                requested: "^4.17.0".to_string(),
                resolved: "4.17.21".to_string(),
                is_optional: false,
                alias: None,
                install_path: None,
            },
            ResolvedInfo {
                tarball_url: Some(
                    "https://registry.npmjs.org/lodash/-/lodash-4.17.21.tgz".to_string(),
                ),
                integrity: Some("sha512-v2kDEe57abc".to_string()),
                dependencies: vec![],
                install_path: "node_modules/lodash".to_string(),
                deprecated: None,
                // This would be set if we fell back from a newer version
                maturity_fallback: Some(crate::maturity::MaturityFallbackInfo {
                    skipped_version: "4.18.0".to_string(),
                    skipped_published_at: chrono::Utc::now(),
                    skipped_age_minutes: 60,
                    required_age_minutes: 4320,
                }),
            },
        );

        // Build and write the lockfile
        let lock = build_package_lock("test-app", "1.0.0", &deps);

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("package-lock.json");
        write_package_lock(&path, &lock).unwrap();

        // Read the lockfile back
        let read_lock = read_package_lock(&path).unwrap();

        // Convert back to deps
        let read_deps = deps_from_lockfile(&read_lock);

        // Verify maturity_fallback is None (not persisted in lockfile)
        let lodash = read_deps.iter().find(|(d, _)| d.name == "lodash").unwrap();
        assert!(
            lodash.1.maturity_fallback.is_none(),
            "maturity_fallback should be None when read from lockfile"
        );

        // Verify other fields are preserved correctly
        assert_eq!(lodash.0.resolved, "4.17.21");
        assert!(lodash.1.tarball_url.is_some());
        assert!(lodash.1.integrity.is_some());
    }
}
