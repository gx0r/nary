use nary_lib::{read_package_lock, scan_node_modules};
use snafu::OptionExt;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::error::{NoLockfileSnafu, Result};
use crate::{FindDupesArgs, ListArgs, PruneArgs};

pub fn run_list(args: &ListArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    if let Some(lock) = read_package_lock(&lockfile_path) {
        if args.json {
            // Output as JSON
            let mut packages: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            for (path, entry) in &lock.packages {
                if path.is_empty() {
                    continue; // Skip root
                }
                // Extract package name from path
                let depth = path.matches("/node_modules/").count();

                if let Some(max_depth) = args.depth {
                    if depth > max_depth {
                        continue;
                    }
                }

                packages.insert(
                    path.clone(),
                    serde_json::json!({
                        "version": entry.version,
                        "resolved": entry.resolved,
                    }),
                );
            }
            println!("{}", serde_json::to_string_pretty(&packages)?);
        } else {
            // Tree output
            let pkg_json_path = root_path.join("package.json");
            let content = fs::read_to_string(&pkg_json_path)?;
            let pkg: serde_json::Value = serde_json::from_str(&content)?;

            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("(root)");
            let version = pkg
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0");
            println!("{}@{}", name, version);

            let mut entries: Vec<_> = lock
                .packages
                .iter()
                .filter(|(path, _)| !path.is_empty())
                .collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));

            for (path, entry) in entries {
                let depth = path.matches("/node_modules/").count();

                if let Some(max_depth) = args.depth {
                    if depth > max_depth {
                        continue;
                    }
                }

                let name = path.rsplit('/').next().unwrap_or(path);
                let indent = "  ".repeat(depth);
                let version = entry.version.as_deref().unwrap_or("?");
                println!("{}├── {}@{}", indent, name, version);
            }
        }
    } else {
        eprintln!("No package-lock.json found. Run 'nary install' first.");
    }

    Ok(())
}

pub fn run_prune(args: &PruneArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");
    let node_modules = root_path.join("node_modules");

    if !node_modules.exists() {
        eprintln!("No node_modules directory found.");
        return Ok(());
    }

    // Get expected packages from lockfile
    let expected: HashSet<String> = if let Some(lock) = read_package_lock(&lockfile_path) {
        lock.packages
            .keys()
            .filter(|p| !p.is_empty() && p.starts_with("node_modules/"))
            .map(|p| p.strip_prefix("node_modules/").unwrap_or(p).to_string())
            .collect()
    } else {
        eprintln!("No package-lock.json found. Run 'nary install' first.");
        return Ok(());
    };

    // Scan node_modules for installed packages
    let mut found: HashSet<String> = HashSet::new();
    scan_node_modules(&node_modules, "", &mut found)?;

    // Find extraneous packages
    let extraneous: Vec<_> = found.difference(&expected).collect();

    if extraneous.is_empty() {
        eprintln!("No extraneous packages found.");
        return Ok(());
    }

    eprintln!("Found {} extraneous package(s):", extraneous.len());

    for pkg in &extraneous {
        let pkg_path = node_modules.join(pkg);
        if args.dry_run {
            eprintln!("  Would remove: {}", pkg);
        } else if let Err(e) = fs::remove_dir_all(&pkg_path) {
            eprintln!("  Failed to remove {}: {}", pkg, e);
        } else {
            eprintln!("  Removed: {}", pkg);
        }
    }

    if args.dry_run {
        eprintln!("\nRun without --dry-run to actually remove packages.");
    }

    Ok(())
}

pub fn run_find_dupes(args: &FindDupesArgs) -> Result<()> {
    let lockfile_path = Path::new(".").join("package-lock.json");
    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    // Build map: package_name -> [(version, path), ...]
    let mut packages: HashMap<String, Vec<(String, String)>> = HashMap::new();

    for (path, entry) in &lock.packages {
        if path.is_empty() {
            continue; // Skip root
        }

        let name = path.rsplit("node_modules/").next().unwrap_or(path);
        let version = entry.version.as_deref().unwrap_or("?");

        packages
            .entry(name.to_string())
            .or_default()
            .push((version.to_string(), path.clone()));
    }

    // Filter to duplicates only
    let dupes: HashMap<_, _> = packages
        .into_iter()
        .filter(|(_, locs)| locs.len() > 1)
        .collect();

    if dupes.is_empty() {
        eprintln!("No duplicate packages found.");
        return Ok(());
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&dupes)?);
    } else {
        eprintln!("Found {} packages with duplicates:\n", dupes.len());
        let mut sorted: Vec<_> = dupes.iter().collect();
        sorted.sort_by_key(|(name, _)| *name);

        for (name, locations) in sorted {
            println!("{}: {} copies", name, locations.len());
            for (ver, path) in locations {
                println!("  {} @ {}", ver, path);
            }
            println!();
        }
    }

    Ok(())
}
