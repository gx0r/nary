use owo_colors::{OwoColorize, Stream};
use snafu::OptionExt;
use std::fs;
use std::path::Path;

use nary_lib::{
    build_package_lock, calculate_depends_with_options, cleanup_empty_dirs, create_client,
    deps_from_lockfile, path_to_dependencies, path_to_overrides, path_to_root_dependency,
    read_package_lock, write_package_lock, MaturityConfig, RegistryConfig, ResolveOptions,
};

use crate::error::{NoLockfileSnafu, Result};
use crate::DedupeArgs;

pub async fn run_dedupe(args: &DedupeArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    let old_lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;
    let old_deps = deps_from_lockfile(&old_lock);

    // Re-resolve with fresh hoisting
    let client = create_client()?;
    let root = path_to_root_dependency(root_path)?;
    let dependencies = path_to_dependencies(root_path, true)?;

    // Parse overrides from package.json (dedupe should respect overrides)
    let overrides = path_to_overrides(root_path);

    let options = ResolveOptions {
        optimize: args.optimize,
        maturity: MaturityConfig::disabled(), // Dedupe operates on already-installed packages
        offline: false,                       // Dedupe may need fresh metadata for resolution
        overrides,
    };

    if args.optimize {
        eprintln!("Analyzing dependency tree (with optimal hoisting)...");
    } else {
        eprintln!("Analyzing dependency tree...");
    }
    let new_deps = calculate_depends_with_options(
        &client,
        &root,
        &dependencies,
        |_, _| {},
        &RegistryConfig::default(),
        &options,
    )
    .await?;

    // Find packages that need to move
    struct MoveInfo {
        name: String,
        version: String,
        old_path: String,
        new_path: String,
        is_hoist: bool, // true = moving up (deduped), false = moving down (conflict)
    }

    let mut moves: Vec<MoveInfo> = Vec::new();

    for (dep, new_info) in &new_deps {
        if let Some(old_info) = old_deps.get(dep) {
            if old_info.install_path != new_info.install_path {
                let old_depth = old_info.install_path.matches("/node_modules/").count();
                let new_depth = new_info.install_path.matches("/node_modules/").count();
                moves.push(MoveInfo {
                    name: dep.name.clone(),
                    version: dep.resolved.clone(),
                    old_path: old_info.install_path.clone(),
                    new_path: new_info.install_path.clone(),
                    is_hoist: new_depth < old_depth,
                });
            }
        }
    }

    if moves.is_empty() {
        eprintln!(
            "{} Already optimal - no changes needed.",
            "✓".if_supports_color(Stream::Stderr, |s| s.green())
        );
        if !args.optimize {
            eprintln!(
                "\n{}",
                "Tip: Run with --optimize for smarter version selection"
                    .if_supports_color(Stream::Stderr, |s| s.dimmed())
            );
        }
        return Ok(());
    }

    // Sort and partition moves
    moves.sort_by(|a, b| a.name.cmp(&b.name));
    let hoists: Vec<_> = moves.iter().filter(|m| m.is_hoist).collect();
    let nests: Vec<_> = moves.iter().filter(|m| !m.is_hoist).collect();

    // Print hoists section
    if !hoists.is_empty() {
        eprintln!(
            "\n{}:",
            format!("Deduplicating {} packages", hoists.len())
                .if_supports_color(Stream::Stderr, |s| s.green())
        );
        for m in &hoists {
            let name_ver = format!("{}@{}", m.name, m.version);
            eprintln!(
                "  {} {} {} → {}",
                "↑".if_supports_color(Stream::Stderr, |s| s.green()),
                name_ver.if_supports_color(Stream::Stderr, |s| s.bold()),
                m.old_path.if_supports_color(Stream::Stderr, |s| s.dimmed()),
                m.new_path
            );
        }
    }

    // Print nests section
    if !nests.is_empty() {
        eprintln!(
            "\n{}:",
            format!("Nesting {} packages (version conflicts)", nests.len())
                .if_supports_color(Stream::Stderr, |s| s.yellow())
        );
        for m in &nests {
            let name_ver = format!("{}@{}", m.name, m.version);
            eprintln!(
                "  {} {} {} → {}",
                "↓".if_supports_color(Stream::Stderr, |s| s.yellow()),
                name_ver.if_supports_color(Stream::Stderr, |s| s.bold()),
                m.old_path.if_supports_color(Stream::Stderr, |s| s.dimmed()),
                m.new_path
            );
        }
    }

    if args.dry_run {
        eprintln!(
            "\n{}",
            "Run without --dry-run to apply changes."
                .if_supports_color(Stream::Stderr, |s| s.dimmed())
        );
        if !args.optimize {
            eprintln!(
                "{}",
                "Tip: Try --optimize for potentially better deduplication"
                    .if_supports_color(Stream::Stderr, |s| s.dimmed())
            );
        }
        return Ok(());
    }

    eprintln!();

    // Apply moves
    let mut moved_count = 0;
    for m in &moves {
        let old_dir = root_path.join(&m.old_path);
        let new_dir = root_path.join(&m.new_path);

        // Check if old path still exists (may have been moved with parent)
        if !old_dir.exists() {
            if new_dir.exists() {
                // Package already at destination, skip
                continue;
            } else {
                // Neither path exists - unexpected state
                eprintln!(
                    "{} {} source path no longer exists",
                    "warn:".if_supports_color(Stream::Stderr, |s| s.yellow()),
                    m.name
                );
                continue;
            }
        }

        // Create parent directories
        if let Some(parent) = new_dir.parent() {
            fs::create_dir_all(parent)?;
        }

        // Move (or remove if new location already exists)
        if new_dir.exists() {
            fs::remove_dir_all(&old_dir)?;
        } else {
            fs::rename(&old_dir, &new_dir)?;
        }
        moved_count += 1;
    }

    // Clean up empty directories
    cleanup_empty_dirs(&root_path.join("node_modules"))?;

    // Write updated lockfile
    let lock = build_package_lock(&root.name, &root.resolved, &new_deps);
    write_package_lock(&lockfile_path, &lock)?;

    eprintln!(
        "{} Moved {} package{}",
        "✓".if_supports_color(Stream::Stderr, |s| s.green()),
        moved_count,
        if moved_count == 1 { "" } else { "s" }
    );
    Ok(())
}
