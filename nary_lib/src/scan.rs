//! Node modules scanning utilities.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;

/// Recursively scan a node_modules directory to find all installed packages.
///
/// Handles:
/// - Regular packages (e.g., "lodash")
/// - Scoped packages (e.g., "@babel/core")
/// - Nested node_modules directories
/// - Skips symlinks (workspace members)
///
/// # Arguments
/// * `dir` - The node_modules directory to scan
/// * `prefix` - Path prefix for nested packages (empty string for top-level)
/// * `found` - Set to collect discovered package paths
///
/// # Returns
/// Package paths like "lodash", "@scope/pkg", "express/node_modules/debug"
pub fn scan_node_modules(dir: &Path, prefix: &str, found: &mut HashSet<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files and .bin
        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();

        // Skip symlinks (workspace members)
        if path.is_symlink() {
            continue;
        }

        if !path.is_dir() {
            continue;
        }

        if name.starts_with('@') {
            // Scoped package directory - recurse one level
            for sub_entry in fs::read_dir(&path)? {
                let sub_entry = sub_entry?;
                let sub_path = sub_entry.path();

                // Skip symlinks (workspace members)
                if sub_path.is_symlink() {
                    continue;
                }

                let sub_name = sub_entry.file_name().to_string_lossy().to_string();
                let full_name = format!("{}/{}", name, sub_name);
                let pkg_name = if prefix.is_empty() {
                    full_name
                } else {
                    format!("{}/{}", prefix, full_name)
                };
                found.insert(pkg_name);
            }
        } else {
            let pkg_name = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            // Check for nested node_modules
            let nested = path.join("node_modules");
            if nested.exists() {
                scan_node_modules(&nested, &format!("{}/node_modules", pkg_name), found)?;
            }

            found.insert(pkg_name);
        }
    }
    Ok(())
}

/// Get all top-level packages in node_modules (non-recursive).
///
/// This is a simpler version that only looks at the top level,
/// useful for quick package listings.
pub fn list_top_level_packages(node_modules: &Path) -> io::Result<HashSet<String>> {
    let mut packages = HashSet::new();

    if !node_modules.exists() {
        return Ok(packages);
    }

    for entry in fs::read_dir(node_modules)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files and .bin
        if name.starts_with('.') {
            continue;
        }

        let path = entry.path();

        // Skip symlinks
        if path.is_symlink() {
            continue;
        }

        if !path.is_dir() {
            continue;
        }

        if name.starts_with('@') {
            // Scoped package directory
            for sub_entry in fs::read_dir(&path)? {
                let sub_entry = sub_entry?;
                let sub_path = sub_entry.path();

                if sub_path.is_symlink() {
                    continue;
                }

                let sub_name = sub_entry.file_name().to_string_lossy().to_string();
                packages.insert(format!("{}/{}", name, sub_name));
            }
        } else {
            packages.insert(name);
        }
    }

    Ok(packages)
}

/// Recursively remove empty directories.
///
/// Walks a directory tree bottom-up, removing any directories that become empty.
/// Non-empty directories are left intact. Useful for cleanup after package removal.
pub fn cleanup_empty_dirs(dir: &Path) -> io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            cleanup_empty_dirs(&path)?;
            // Try to remove if empty (ignore errors for non-empty dirs)
            let _ = fs::remove_dir(&path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_package(base: &Path, name: &str) {
        let pkg_path = if name.contains('/') {
            // Scoped package
            let parts: Vec<&str> = name.splitn(2, '/').collect();
            let scope_dir = base.join(parts[0]);
            fs::create_dir_all(&scope_dir).unwrap();
            scope_dir.join(parts[1])
        } else {
            base.join(name)
        };
        fs::create_dir_all(&pkg_path).unwrap();
        fs::write(pkg_path.join("package.json"), "{}").unwrap();
    }

    #[test]
    fn test_scan_empty_dir() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        let mut found = HashSet::new();
        scan_node_modules(&nm, "", &mut found).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn test_scan_regular_packages() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        create_package(&nm, "lodash");
        create_package(&nm, "express");

        let mut found = HashSet::new();
        scan_node_modules(&nm, "", &mut found).unwrap();

        assert!(found.contains("lodash"));
        assert!(found.contains("express"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn test_scan_scoped_packages() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        create_package(&nm, "@babel/core");
        create_package(&nm, "@babel/parser");

        let mut found = HashSet::new();
        scan_node_modules(&nm, "", &mut found).unwrap();

        assert!(found.contains("@babel/core"));
        assert!(found.contains("@babel/parser"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn test_scan_nested_node_modules() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");

        create_package(&nm, "express");
        let nested = nm.join("express/node_modules");
        create_package(&nested, "debug");

        let mut found = HashSet::new();
        scan_node_modules(&nm, "", &mut found).unwrap();

        assert!(found.contains("express"));
        assert!(found.contains("express/node_modules/debug"));
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn test_scan_skips_hidden() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        create_package(&nm, "lodash");
        fs::create_dir(nm.join(".bin")).unwrap();
        fs::create_dir(nm.join(".cache")).unwrap();

        let mut found = HashSet::new();
        scan_node_modules(&nm, "", &mut found).unwrap();

        assert!(found.contains("lodash"));
        assert!(!found.contains(".bin"));
        assert!(!found.contains(".cache"));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn test_list_top_level_simple() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");

        create_package(&nm, "lodash");
        create_package(&nm, "@scope/pkg");

        let packages = list_top_level_packages(&nm).unwrap();

        assert!(packages.contains("lodash"));
        assert!(packages.contains("@scope/pkg"));
        assert_eq!(packages.len(), 2);
    }

    #[test]
    fn test_list_top_level_nonexistent() {
        let temp = TempDir::new().unwrap();
        let nm = temp.path().join("node_modules");

        let packages = list_top_level_packages(&nm).unwrap();
        assert!(packages.is_empty());
    }
}
