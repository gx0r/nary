use std::collections::{HashMap, HashSet, VecDeque};

use crate::lockfile::PackageLock;

/// Information about a root dependency from package.json
#[derive(Debug, Clone)]
pub struct RootDependency {
    /// Version constraint (e.g., "^4.0.0")
    pub constraint: String,
    /// Whether this is a dev dependency
    pub is_dev: bool,
    /// Whether this is an optional dependency
    pub is_optional: bool,
}

/// A node in a dependency path
#[derive(Debug, Clone)]
pub struct DependencyPathNode {
    /// Package name (e.g., "lodash", "@babel/core")
    pub name: String,
    /// Resolved version installed (e.g., "4.17.21")
    pub version: String,
    /// Version constraint specified by parent (e.g., "^4.0.0")
    /// None for the root project
    pub requested: Option<String>,
    /// Install path in node_modules (e.g., "node_modules/lodash")
    pub install_path: String,
    /// Whether this is a dev dependency
    pub is_dev: bool,
    /// Whether this is an optional dependency
    pub is_optional: bool,
}

/// A complete path from root to a target package
#[derive(Debug, Clone)]
pub struct DependencyPath {
    /// Ordered list of nodes from root to target
    pub nodes: Vec<DependencyPathNode>,
}

/// Result of the "why" query
#[derive(Debug)]
pub struct WhyResult {
    /// The package that was queried
    pub package: String,
    /// All paths leading to this package (grouped by install location)
    pub paths: Vec<DependencyPath>,
    /// All versions of this package found
    pub versions: Vec<String>,
}

/// Information about a parent dependency
#[derive(Debug, Clone)]
struct ParentInfo {
    /// Install path of the parent (e.g., "node_modules/express" or "" for root)
    parent_path: String,
    /// Version constraint specified (e.g., "^4.0.0")
    version_constraint: String,
}

/// BFS queue entry: (current_path, path_so_far with requested constraints)
type BfsQueue = VecDeque<(String, Vec<(String, Option<String>)>)>;

/// Build a reverse dependency graph from the lockfile
/// Returns: child_path -> Vec<ParentInfo>
fn build_reverse_graph(
    lock: &PackageLock,
    root_deps: &HashMap<String, RootDependency>,
) -> HashMap<String, Vec<ParentInfo>> {
    let mut reverse: HashMap<String, Vec<ParentInfo>> = HashMap::new();

    // Add root dependencies from package.json
    for (dep_name, root_dep) in root_deps {
        let child_path = format!("node_modules/{}", dep_name);
        if lock.packages.contains_key(&child_path) {
            reverse.entry(child_path).or_default().push(ParentInfo {
                parent_path: String::new(), // root
                version_constraint: root_dep.constraint.clone(),
            });
        }
    }

    for (parent_path, entry) in &lock.packages {
        // For each dependency this package has
        for (dep_name, version_constraint) in &entry.dependencies {
            // Find where this dependency is installed
            // It could be:
            // 1. Hoisted to a parent node_modules
            // 2. Nested under this package's node_modules
            let child_path = find_dependency_path(lock, parent_path, dep_name);

            if let Some(child_path) = child_path {
                reverse.entry(child_path).or_default().push(ParentInfo {
                    parent_path: parent_path.clone(),
                    version_constraint: version_constraint.clone(),
                });
            }
        }
    }

    reverse
}

/// Find where a dependency is installed, starting from a parent path
/// Searches upward through nested node_modules to find the package
fn find_dependency_path(lock: &PackageLock, parent_path: &str, dep_name: &str) -> Option<String> {
    // Start by looking in the parent's nested node_modules
    let mut search_base = parent_path.to_string();

    loop {
        let candidate = if search_base.is_empty() {
            format!("node_modules/{}", dep_name)
        } else {
            format!("{}/node_modules/{}", search_base, dep_name)
        };

        if lock.packages.contains_key(&candidate) {
            return Some(candidate);
        }

        // Move up one level
        if search_base.is_empty() {
            break;
        }

        // Remove the last /node_modules/xxx segment
        if let Some(pos) = search_base.rfind("/node_modules/") {
            search_base = search_base[..pos].to_string();
        } else if search_base.starts_with("node_modules/") {
            search_base = String::new();
        } else {
            break;
        }
    }

    None
}

/// Extract package name from install path
/// "node_modules/lodash" -> "lodash"
/// "node_modules/@scope/pkg" -> "@scope/pkg"
/// "node_modules/express/node_modules/qs" -> "qs"
fn name_from_path(path: &str) -> String {
    path.rsplit("node_modules/")
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Options for finding dependency paths
#[derive(Debug, Default)]
pub struct WhyOptions {
    /// Root dependencies from package.json (name -> RootDependency)
    pub root_deps: HashMap<String, RootDependency>,
    /// Filter to a specific version (e.g., "4.17.21")
    pub version_filter: Option<String>,
}

/// Find all dependency paths leading to a package
pub fn find_dependency_paths(lock: &PackageLock, package_name: &str) -> WhyResult {
    find_dependency_paths_with_options(lock, package_name, &WhyOptions::default())
}

/// Find all dependency paths leading to a package with options
pub fn find_dependency_paths_with_options(
    lock: &PackageLock,
    package_name: &str,
    options: &WhyOptions,
) -> WhyResult {
    let reverse_graph = build_reverse_graph(lock, &options.root_deps);

    // Find all install paths matching the package name (and optionally version)
    let target_paths: Vec<String> = lock
        .packages
        .iter()
        .filter(|(path, entry)| {
            if path.is_empty() {
                return false;
            }
            let name = name_from_path(path);
            if name != package_name {
                return false;
            }
            // Filter by version if specified
            if let Some(ref ver_filter) = options.version_filter {
                if let Some(ref ver) = entry.version {
                    return ver == ver_filter;
                }
                return false;
            }
            true
        })
        .map(|(path, _)| path.clone())
        .collect();

    let mut all_paths = Vec::new();
    let mut versions = HashSet::new();

    // For each instance of the package, find all paths back to root
    for target_path in &target_paths {
        if let Some(entry) = lock.packages.get(target_path.as_str()) {
            if let Some(v) = &entry.version {
                versions.insert(v.clone());
            }
        }

        // BFS from target back to root
        let paths = bfs_to_root(lock, &reverse_graph, target_path, &options.root_deps);
        all_paths.extend(paths);
    }

    let mut versions: Vec<String> = versions.into_iter().collect();
    versions.sort();

    WhyResult {
        package: package_name.to_string(),
        paths: all_paths,
        versions,
    }
}

/// Find all packages that depend on the given package (inverse of why)
pub fn find_dependents(lock: &PackageLock, package_name: &str) -> WhyResult {
    find_dependents_with_options(lock, package_name, &WhyOptions::default())
}

/// Find all packages that depend on the given package with options
pub fn find_dependents_with_options(
    lock: &PackageLock,
    package_name: &str,
    options: &WhyOptions,
) -> WhyResult {
    // Find all packages that list this package as a dependency
    let mut dependents: Vec<(String, String, String)> = Vec::new(); // (dependent_path, dependent_name, constraint)

    for (path, entry) in &lock.packages {
        if path.is_empty() {
            continue;
        }
        for (dep_name, constraint) in &entry.dependencies {
            if dep_name == package_name {
                let name = name_from_path(path);
                dependents.push((path.clone(), name, constraint.clone()));
            }
        }
    }

    // Also check root dependencies
    if let Some(root_dep) = options.root_deps.get(package_name) {
        dependents.push((
            String::new(),
            "(root)".to_string(),
            root_dep.constraint.clone(),
        ));
    }

    // Build paths (each dependent is a simple one-hop path)
    let mut paths = Vec::new();
    let mut versions = HashSet::new();

    // Find the target package's version(s)
    for (path, entry) in &lock.packages {
        if path.is_empty() {
            continue;
        }
        let name = name_from_path(path);
        if name == package_name {
            if let Some(ref ver_filter) = options.version_filter {
                if entry.version.as_ref() == Some(ver_filter) {
                    if let Some(v) = &entry.version {
                        versions.insert(v.clone());
                    }
                }
            } else if let Some(v) = &entry.version {
                versions.insert(v.clone());
            }
        }
    }

    for (dep_path, dep_name, constraint) in dependents {
        let entry = lock.packages.get(&dep_path);
        let (version, is_dev, is_optional) = if dep_path.is_empty() {
            let root_dep = options.root_deps.get(package_name);
            (
                lock.version.clone().unwrap_or_default(),
                root_dep.map(|d| d.is_dev).unwrap_or(false),
                root_dep.map(|d| d.is_optional).unwrap_or(false),
            )
        } else {
            (
                entry.and_then(|e| e.version.clone()).unwrap_or_default(),
                entry.map(|e| e.dev).unwrap_or(false),
                entry.map(|e| e.optional).unwrap_or(false),
            )
        };

        paths.push(DependencyPath {
            nodes: vec![DependencyPathNode {
                name: dep_name,
                version,
                requested: Some(constraint),
                install_path: dep_path,
                is_dev,
                is_optional,
            }],
        });
    }

    let mut versions: Vec<String> = versions.into_iter().collect();
    versions.sort();

    WhyResult {
        package: package_name.to_string(),
        paths,
        versions,
    }
}

/// Check if a path is a direct dependency (top-level, not nested)
fn is_direct_dependency(path: &str) -> bool {
    // "node_modules/foo" is direct, "node_modules/foo/node_modules/bar" is not
    path.starts_with("node_modules/") && !path.contains("/node_modules/")
}

/// BFS from a target path back to root, collecting all paths
fn bfs_to_root(
    lock: &PackageLock,
    reverse_graph: &HashMap<String, Vec<ParentInfo>>,
    start_path: &str,
    root_deps: &HashMap<String, RootDependency>,
) -> Vec<DependencyPath> {
    let mut result = Vec::new();

    let mut queue: BfsQueue = VecDeque::new();
    queue.push_back((start_path.to_string(), vec![(start_path.to_string(), None)]));

    let mut visited_states: HashSet<String> = HashSet::new();

    while let Some((current_path, path_so_far)) = queue.pop_front() {
        // Create a state key to avoid infinite loops
        let state_key = format!("{}:{}", current_path, path_so_far.len());
        if visited_states.contains(&state_key) {
            continue;
        }
        visited_states.insert(state_key);

        // Limit path length to prevent infinite loops
        if path_so_far.len() > 50 {
            continue;
        }

        if current_path.is_empty() {
            // Reached root - build the path
            // Note: path_so_far stores (path, constraint_used_to_request_child)
            // So we need to shift constraints: each node gets the constraint from
            // the NEXT entry in path_so_far (which after reversal is the previous entry)
            let reversed: Vec<_> = path_so_far.iter().rev().collect();
            let mut nodes = Vec::new();

            for (i, (path, _)) in reversed.iter().enumerate() {
                let entry = lock.packages.get(path.as_str());
                let (name, version, is_dev, is_optional) = if path.is_empty() {
                    let name = lock.name.clone().unwrap_or_else(|| "(root)".to_string());
                    let version = lock.version.clone().unwrap_or_else(|| "0.0.0".to_string());
                    (name, version, false, false)
                } else {
                    let name = name_from_path(path);
                    let version = entry
                        .and_then(|e| e.version.clone())
                        .unwrap_or_else(|| "?".to_string());
                    // Check if this is a direct child of root - use root_deps for flags
                    let (is_dev, is_optional) = if i == 1 {
                        // First node after root - check root_deps
                        root_deps
                            .get(&name)
                            .map(|d| (d.is_dev, d.is_optional))
                            .unwrap_or_else(|| {
                                (
                                    entry.map(|e| e.dev).unwrap_or(false),
                                    entry.map(|e| e.optional).unwrap_or(false),
                                )
                            })
                    } else {
                        (
                            entry.map(|e| e.dev).unwrap_or(false),
                            entry.map(|e| e.optional).unwrap_or(false),
                        )
                    };
                    (name, version, is_dev, is_optional)
                };

                // The constraint for this node comes from the previous node in the
                // reversed list (which is the parent that requested this node)
                let requested = if i == 0 {
                    None // Root has no parent
                } else {
                    reversed[i - 1].1.clone()
                };

                nodes.push(DependencyPathNode {
                    name,
                    version,
                    requested,
                    install_path: (*path).clone(),
                    is_dev,
                    is_optional,
                });
            }

            if nodes.len() > 1 {
                result.push(DependencyPath { nodes });
            }
            continue;
        }

        // Get parents of current node
        let has_explicit_parents = reverse_graph.contains_key(&current_path);
        if let Some(parents) = reverse_graph.get(&current_path) {
            for parent in parents {
                let mut new_path = path_so_far.clone();
                new_path.push((
                    parent.parent_path.clone(),
                    Some(parent.version_constraint.clone()),
                ));
                queue.push_back((parent.parent_path.clone(), new_path));
            }
        }

        // If this is a direct dependency of root (top-level, not nested) AND
        // there are no explicit parents in the reverse graph, add path to root.
        // This handles the case where lockfile v3 doesn't list root dependencies
        // in the "" entry.
        if !has_explicit_parents && is_direct_dependency(&current_path) {
            let mut new_path = path_so_far.clone();
            // We don't know the exact constraint without reading package.json,
            // so use "*" as a placeholder
            new_path.push((String::new(), Some("*".to_string())));
            queue.push_back((String::new(), new_path));
        }
    }

    result
}

/// Format the why result as human-readable text
pub fn format_why_text(result: &WhyResult) -> String {
    if result.paths.is_empty() {
        return format!("Package '{}' is not installed", result.package);
    }

    let mut output = String::new();

    // Group paths by the target's install location
    let mut by_location: HashMap<String, Vec<&DependencyPath>> = HashMap::new();
    for path in &result.paths {
        if let Some(last) = path.nodes.last() {
            by_location
                .entry(last.install_path.clone())
                .or_default()
                .push(path);
        }
    }

    let mut locations: Vec<_> = by_location.keys().cloned().collect();
    locations.sort();

    for location in locations {
        let paths = &by_location[&location];
        if let Some(first_path) = paths.first() {
            if let Some(target) = first_path.nodes.last() {
                output.push_str(&format!(
                    "{}@{} {}\n",
                    target.name, target.version, target.install_path
                ));
            }
        }

        // Show each dependency chain
        for path in paths {
            // Skip root, show from first real dependency
            let chain: Vec<_> = path.nodes.iter().skip(1).collect();
            if chain.is_empty() {
                continue;
            }

            // The first node after root requested the next one
            for (i, node) in chain.iter().enumerate() {
                let indent = "  ".repeat(i + 1);
                let constraint = node.requested.as_deref().unwrap_or("*");

                // Build marker suffix for dev/optional
                let marker = if node.is_dev && node.is_optional {
                    " (dev, optional)"
                } else if node.is_dev {
                    " (dev)"
                } else if node.is_optional {
                    " (optional)"
                } else {
                    ""
                };

                if i == 0 {
                    // First level - requested by root
                    output.push_str(&format!(
                        "{}{} {}@\"{}\"{} from the root project\n",
                        indent,
                        if node.is_dev { "dev" } else { "   " },
                        node.name,
                        constraint,
                        if node.is_optional { " (optional)" } else { "" }
                    ));
                } else {
                    // Nested - show what requested it
                    let prev = chain[i - 1];
                    output.push_str(&format!(
                        "{}{}@\"{}\"{} from {}@{}\n",
                        indent, node.name, constraint, marker, prev.name, prev.version
                    ));
                    output.push_str(&format!("{}  {}\n", indent, prev.install_path));
                }
            }
        }
        output.push('\n');
    }

    output.trim_end().to_string()
}

/// Format the why result as JSON
pub fn format_why_json(result: &WhyResult) -> serde_json::Value {
    use serde_json::json;

    if result.paths.is_empty() {
        return json!({
            "package": result.package,
            "installed": false,
            "paths": []
        });
    }

    // Group by install location
    let mut by_location: HashMap<String, Vec<&DependencyPath>> = HashMap::new();
    for path in &result.paths {
        if let Some(last) = path.nodes.last() {
            by_location
                .entry(last.install_path.clone())
                .or_default()
                .push(path);
        }
    }

    let locations: Vec<serde_json::Value> = by_location
        .iter()
        .map(|(location, paths)| {
            let version = paths
                .first()
                .and_then(|p| p.nodes.last())
                .map(|n| n.version.clone())
                .unwrap_or_default();

            let dependents: Vec<serde_json::Value> = paths
                .iter()
                .map(|path| {
                    let chain: Vec<serde_json::Value> = path
                        .nodes
                        .iter()
                        .skip(1) // Skip root
                        .map(|node| {
                            json!({
                                "name": node.name,
                                "version": node.version,
                                "spec": node.requested,
                                "dev": node.is_dev,
                                "location": node.install_path
                            })
                        })
                        .collect();
                    json!({ "chain": chain })
                })
                .collect();

            json!({
                "version": version,
                "location": location,
                "dependents": dependents
            })
        })
        .collect();

    json!({
        "package": result.package,
        "installed": true,
        "versions": result.versions,
        "locations": locations
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::{PackageEntry, PackageLock};
    use indexmap::IndexMap;

    fn make_entry(version: &str, deps: Vec<(&str, &str)>) -> PackageEntry {
        let mut dependencies = IndexMap::new();
        for (name, constraint) in deps {
            dependencies.insert(name.to_string(), constraint.to_string());
        }
        PackageEntry {
            version: Some(version.to_string()),
            dependencies,
            ..Default::default()
        }
    }

    #[test]
    fn test_find_direct_dependency() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("lodash", "^4.17.0")]),
        );
        packages.insert(
            "node_modules/lodash".to_string(),
            make_entry("4.17.21", vec![]),
        );

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "lodash");
        assert!(!result.paths.is_empty());
        assert_eq!(result.versions, vec!["4.17.21"]);
    }

    #[test]
    fn test_find_transitive_dependency() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("express", "^4.0.0")]),
        );
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.11.0", vec![]));

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "qs");
        assert!(!result.paths.is_empty());
        assert_eq!(result.versions, vec!["6.11.0"]);

        // Check that the path goes through express
        let path = &result.paths[0];
        let names: Vec<&str> = path.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"express"));
    }

    #[test]
    fn test_package_not_found() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![]));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "nonexistent");
        assert!(result.paths.is_empty());
        assert!(result.versions.is_empty());
    }

    #[test]
    fn test_multiple_versions() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("express", "^4.0.0"), ("qs", "^6.12.0")]),
        );
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.12.0", vec![]));
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            make_entry("6.5.0", vec![]),
        );

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "qs");
        assert_eq!(result.versions.len(), 2);
        assert!(result.versions.contains(&"6.12.0".to_string()));
        assert!(result.versions.contains(&"6.5.0".to_string()));
    }

    #[test]
    fn test_format_not_found() {
        let result = WhyResult {
            package: "nonexistent".to_string(),
            paths: vec![],
            versions: vec![],
        };
        let output = format_why_text(&result);
        assert!(output.contains("not installed"));
    }

    #[test]
    fn test_name_from_path_simple() {
        assert_eq!(name_from_path("node_modules/lodash"), "lodash");
        assert_eq!(name_from_path("node_modules/express"), "express");
    }

    #[test]
    fn test_name_from_path_scoped() {
        assert_eq!(name_from_path("node_modules/@scope/pkg"), "@scope/pkg");
        assert_eq!(name_from_path("node_modules/@babel/core"), "@babel/core");
        assert_eq!(name_from_path("node_modules/@types/node"), "@types/node");
    }

    #[test]
    fn test_name_from_path_nested() {
        assert_eq!(name_from_path("node_modules/express/node_modules/qs"), "qs");
        assert_eq!(
            name_from_path("node_modules/a/node_modules/b/node_modules/c"),
            "c"
        );
        assert_eq!(
            name_from_path("node_modules/foo/node_modules/@scope/bar"),
            "@scope/bar"
        );
    }

    #[test]
    fn test_name_from_path_edge_cases() {
        // Empty or unusual paths
        assert_eq!(name_from_path(""), "");
        assert_eq!(name_from_path("lodash"), "lodash");
    }

    #[test]
    fn test_is_direct_dependency() {
        assert!(is_direct_dependency("node_modules/lodash"));
        assert!(is_direct_dependency("node_modules/@scope/pkg"));
        assert!(!is_direct_dependency("node_modules/a/node_modules/b"));
        assert!(!is_direct_dependency(
            "node_modules/x/node_modules/@scope/y"
        ));
        assert!(!is_direct_dependency("")); // root is not a direct dependency
        assert!(!is_direct_dependency("some/other/path"));
    }

    #[test]
    fn test_find_dependency_path_hoisted() {
        // Test that find_dependency_path looks up the tree
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![]));
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.11.0", vec![]));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        // qs is hoisted to root, so express's dep on qs should find it there
        let result = find_dependency_path(&lock, "node_modules/express", "qs");
        assert_eq!(result, Some("node_modules/qs".to_string()));
    }

    #[test]
    fn test_find_dependency_path_nested() {
        // Test that nested dependency is found before hoisted
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![]));
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.5.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.11.0", vec![]));
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            make_entry("6.5.0", vec![]),
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        // express should find its nested qs first
        let result = find_dependency_path(&lock, "node_modules/express", "qs");
        assert_eq!(
            result,
            Some("node_modules/express/node_modules/qs".to_string())
        );
    }

    #[test]
    fn test_find_dependency_path_not_found() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![]));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_path(&lock, "node_modules/express", "lodash");
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_dependents_simple() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("express", "^4.0.0")]),
        );
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.11.0", vec![]));

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependents(&lock, "qs");
        assert_eq!(result.paths.len(), 1);
        assert_eq!(result.paths[0].nodes[0].name, "express");
    }

    #[test]
    fn test_find_dependents_multiple() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("express", "^4.0.0"), ("koa", "^2.0.0")]),
        );
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert(
            "node_modules/koa".to_string(),
            make_entry("2.14.0", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.11.0", vec![]));

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependents(&lock, "qs");
        assert_eq!(result.paths.len(), 2);
        let names: Vec<&str> = result
            .paths
            .iter()
            .map(|p| p.nodes[0].name.as_str())
            .collect();
        assert!(names.contains(&"express"));
        assert!(names.contains(&"koa"));
    }

    #[test]
    fn test_find_dependents_with_root() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("lodash", "^4.0.0")]),
        );
        packages.insert(
            "node_modules/lodash".to_string(),
            make_entry("4.17.21", vec![]),
        );

        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let mut root_deps = HashMap::new();
        root_deps.insert(
            "lodash".to_string(),
            RootDependency {
                constraint: "^4.0.0".to_string(),
                is_dev: false,
                is_optional: false,
            },
        );

        let result = find_dependents_with_options(
            &lock,
            "lodash",
            &WhyOptions {
                root_deps,
                version_filter: None,
            },
        );
        assert!(result.paths.iter().any(|p| p.nodes[0].name == "(root)"));
    }

    #[test]
    fn test_find_dependency_paths_deep_chain() {
        // a -> b -> c -> d
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![("a", "^1.0.0")]));
        packages.insert(
            "node_modules/a".to_string(),
            make_entry("1.0.0", vec![("b", "^1.0.0")]),
        );
        packages.insert(
            "node_modules/b".to_string(),
            make_entry("1.0.0", vec![("c", "^1.0.0")]),
        );
        packages.insert(
            "node_modules/c".to_string(),
            make_entry("1.0.0", vec![("d", "^1.0.0")]),
        );
        packages.insert("node_modules/d".to_string(), make_entry("1.0.0", vec![]));

        let lock = PackageLock {
            name: Some("test".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "d");
        assert!(!result.paths.is_empty());

        // Check the chain length - should be root -> a -> b -> c -> d
        let path = &result.paths[0];
        assert_eq!(path.nodes.len(), 5); // root + a + b + c + d
    }

    #[test]
    fn test_find_dependency_paths_with_version_filter() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("express", "^4.0.0"), ("qs", "^6.12.0")]),
        );
        packages.insert(
            "node_modules/express".to_string(),
            make_entry("4.18.2", vec![("qs", "^6.0.0")]),
        );
        packages.insert("node_modules/qs".to_string(), make_entry("6.12.0", vec![]));
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            make_entry("6.5.0", vec![]),
        );

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        // Filter to only version 6.5.0
        let result = find_dependency_paths_with_options(
            &lock,
            "qs",
            &WhyOptions {
                root_deps: HashMap::new(),
                version_filter: Some("6.5.0".to_string()),
            },
        );

        assert_eq!(result.versions, vec!["6.5.0"]);
        // Should only have paths to the 6.5.0 version
        for path in &result.paths {
            if let Some(last) = path.nodes.last() {
                assert_eq!(last.version, "6.5.0");
            }
        }
    }

    #[test]
    fn test_empty_lockfile() {
        let packages = IndexMap::new();
        let lock = PackageLock {
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "anything");
        assert!(result.paths.is_empty());
        assert!(result.versions.is_empty());
    }

    #[test]
    fn test_root_only_lockfile() {
        let mut packages = IndexMap::new();
        packages.insert("".to_string(), make_entry("1.0.0", vec![]));

        let lock = PackageLock {
            name: Some("my-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "lodash");
        assert!(result.paths.is_empty());
    }

    #[test]
    fn test_scoped_package_dependency() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("@babel/core", "^7.0.0")]),
        );
        packages.insert(
            "node_modules/@babel/core".to_string(),
            make_entry("7.23.0", vec![("@babel/types", "^7.0.0")]),
        );
        packages.insert(
            "node_modules/@babel/types".to_string(),
            make_entry("7.23.0", vec![]),
        );

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "@babel/types");
        assert!(!result.paths.is_empty());
        assert_eq!(result.versions, vec!["7.23.0"]);
    }

    #[test]
    fn test_format_why_json_installed() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("lodash", "^4.0.0")]),
        );
        packages.insert(
            "node_modules/lodash".to_string(),
            make_entry("4.17.21", vec![]),
        );

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let result = find_dependency_paths(&lock, "lodash");
        let json = format_why_json(&result);

        assert_eq!(json["package"], "lodash");
        assert_eq!(json["installed"], true);
        assert!(json["versions"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("4.17.21")));
    }

    #[test]
    fn test_format_why_json_not_installed() {
        let result = WhyResult {
            package: "nonexistent".to_string(),
            paths: vec![],
            versions: vec![],
        };
        let json = format_why_json(&result);

        assert_eq!(json["package"], "nonexistent");
        assert_eq!(json["installed"], false);
        assert!(json["paths"].as_array().unwrap().is_empty());
    }

    #[test]
    fn test_dev_and_optional_flags() {
        let mut packages = IndexMap::new();
        packages.insert(
            "".to_string(),
            make_entry("1.0.0", vec![("jest", "^29.0.0")]),
        );
        let mut jest_entry = make_entry("29.0.0", vec![]);
        jest_entry.dev = true;
        packages.insert("node_modules/jest".to_string(), jest_entry);

        let lock = PackageLock {
            name: Some("test-app".to_string()),
            lockfile_version: 3,
            packages,
            ..Default::default()
        };

        let mut root_deps = HashMap::new();
        root_deps.insert(
            "jest".to_string(),
            RootDependency {
                constraint: "^29.0.0".to_string(),
                is_dev: true,
                is_optional: false,
            },
        );

        let result = find_dependency_paths_with_options(
            &lock,
            "jest",
            &WhyOptions {
                root_deps,
                version_filter: None,
            },
        );

        assert!(!result.paths.is_empty());
    }
}
