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
        let name = key
            .rsplit("node_modules/")
            .next()
            .unwrap_or(key)
            .to_string();

        // Skip if missing required fields
        let version = match &entry.version {
            Some(v) => v.clone(),
            None => continue,
        };

        let dep = Dependency {
            name,
            requested: version.clone(), // In lockfile, requested == resolved
            resolved: version,
            is_optional: entry.optional,
            alias: None, // TODO: lockfile v3 doesn't store aliases, need to handle
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
}
