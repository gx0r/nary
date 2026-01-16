use nary_lib::{
    find_dependency_paths_with_options, find_dependents_with_options, format_why_json,
    format_why_text, read_package_lock, RootDependency, WhyOptions,
};
use snafu::OptionExt;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::{NoLockfileSnafu, Result};
use crate::WhyArgs;

/// Parse package.json to get root dependencies with their constraints
fn read_root_dependencies(root_path: &Path) -> HashMap<String, RootDependency> {
    let mut deps = HashMap::new();

    let pkg_json_path = root_path.join("package.json");
    let content = match fs::read_to_string(&pkg_json_path) {
        Ok(c) => c,
        Err(_) => return deps,
    };

    let pkg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return deps,
    };

    // Regular dependencies
    if let Some(dependencies) = pkg.get("dependencies").and_then(|d| d.as_object()) {
        for (name, constraint) in dependencies {
            if let Some(constraint_str) = constraint.as_str() {
                deps.insert(
                    name.clone(),
                    RootDependency {
                        constraint: constraint_str.to_string(),
                        is_dev: false,
                        is_optional: false,
                    },
                );
            }
        }
    }

    // Dev dependencies
    if let Some(dev_deps) = pkg.get("devDependencies").and_then(|d| d.as_object()) {
        for (name, constraint) in dev_deps {
            if let Some(constraint_str) = constraint.as_str() {
                deps.insert(
                    name.clone(),
                    RootDependency {
                        constraint: constraint_str.to_string(),
                        is_dev: true,
                        is_optional: false,
                    },
                );
            }
        }
    }

    // Optional dependencies
    if let Some(opt_deps) = pkg.get("optionalDependencies").and_then(|d| d.as_object()) {
        for (name, constraint) in opt_deps {
            if let Some(constraint_str) = constraint.as_str() {
                deps.insert(
                    name.clone(),
                    RootDependency {
                        constraint: constraint_str.to_string(),
                        is_dev: false,
                        is_optional: true,
                    },
                );
            }
        }
    }

    deps
}

/// Parse package spec like "lodash@4.17.21" into (name, optional_version)
fn parse_package_spec(spec: &str) -> (&str, Option<&str>) {
    // Handle scoped packages like @types/node@20.0.0
    if spec.starts_with('@') {
        // Find the second @ which separates name from version
        if let Some(pos) = spec[1..].find('@') {
            let split_pos = pos + 1;
            return (&spec[..split_pos], Some(&spec[split_pos + 1..]));
        }
        return (spec, None);
    }

    // Regular package
    if let Some(pos) = spec.find('@') {
        (&spec[..pos], Some(&spec[pos + 1..]))
    } else {
        (spec, None)
    }
}

pub fn run_why(args: &WhyArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    // Parse package name and optional version
    let (package_name, version_filter) = parse_package_spec(&args.package);

    // Read root dependencies from package.json
    let root_deps = read_root_dependencies(root_path);

    let options = WhyOptions {
        root_deps,
        version_filter: version_filter.map(|s| s.to_string()),
    };

    let result = if args.dependents {
        find_dependents_with_options(&lock, package_name, &options)
    } else {
        find_dependency_paths_with_options(&lock, package_name, &options)
    };

    if args.json {
        let json = format_why_json(&result);
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else if args.dependents {
        // Format dependents output differently
        if result.paths.is_empty() {
            println!("No packages depend on '{}'", package_name);
        } else {
            println!(
                "{} package(s) depend on {}:",
                result.paths.len(),
                package_name
            );
            for path in &result.paths {
                if let Some(node) = path.nodes.first() {
                    let marker = if node.is_dev && node.is_optional {
                        " (dev, optional)"
                    } else if node.is_dev {
                        " (dev)"
                    } else if node.is_optional {
                        " (optional)"
                    } else {
                        ""
                    };
                    println!(
                        "  {}@{} requires \"{}\"{}",
                        node.name,
                        node.version,
                        node.requested.as_deref().unwrap_or("*"),
                        marker
                    );
                }
            }
        }
    } else {
        let output = format_why_text(&result);
        println!("{}", output);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_package_spec_simple() {
        assert_eq!(parse_package_spec("lodash"), ("lodash", None));
    }

    #[test]
    fn test_parse_package_spec_with_version() {
        assert_eq!(
            parse_package_spec("lodash@4.17.21"),
            ("lodash", Some("4.17.21"))
        );
    }

    #[test]
    fn test_parse_package_spec_scoped() {
        assert_eq!(parse_package_spec("@types/node"), ("@types/node", None));
    }

    #[test]
    fn test_parse_package_spec_scoped_with_version() {
        assert_eq!(
            parse_package_spec("@types/node@20.0.0"),
            ("@types/node", Some("20.0.0"))
        );
    }
}
