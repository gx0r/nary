use futures::stream::{self, StreamExt};
use owo_colors::{OwoColorize, Stream};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};

use nary_lib::{
    build_package_lock, calculate_depends, create_client, deps_from_lockfile, get_audit_summary,
    install_dep_with_tarball_url, path_to_dependencies, path_to_root_dependency, read_package_lock,
    scan_node_modules, write_package_lock, LifecycleRunner, ScriptAudit, WorkspaceConfig,
};

use crate::error::Result;
use crate::{InstallArgs, InstallResult, MAX_CONCURRENT, RENDER_DEBOUNCE_MS};

/// Prompt user and run lifecycle scripts if approved
pub(crate) fn prompt_and_run_lifecycle_scripts(
    audits: &[ScriptAudit],
    runner: &Arc<LifecycleRunner>,
    assume_yes: bool,
    show_details: bool,
) {
    if audits.is_empty() {
        return;
    }

    if show_details {
        eprintln!(
            "\n{} {} packages want to run lifecycle scripts:\n",
            "⚠".if_supports_color(Stream::Stderr, |s| s.yellow()),
            audits.len()
        );
        for audit in audits {
            eprintln!(
                "  {}",
                audit
                    .package
                    .if_supports_color(Stream::Stderr, |s| s.bold())
            );
            for script in &audit.scripts {
                eprintln!("    {}: {}", script.name, script.command);
            }
        }
        eprintln!();
    } else {
        eprintln!("\n{} packages want to run lifecycle scripts", audits.len());
    }

    let should_run = if assume_yes {
        true
    } else if atty::is(atty::Stream::Stdin) {
        eprint!("Run scripts? [y/N] ");
        io::stderr().flush().ok();
        let mut input = String::new();
        io::stdin().read_line(&mut input).ok();
        matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
    } else {
        eprintln!("Skipping scripts (non-interactive, use -y to run)");
        false
    };

    if should_run {
        eprintln!("Running lifecycle scripts...");
        for audit in audits {
            if let Err(e) = runner.run_lifecycle_scripts(&audit.package) {
                eprintln!(
                    "warn: lifecycle scripts for {} failed: {}",
                    audit.package, e
                );
            }
        }
    }
}

pub async fn run_install(args: &InstallArgs) -> Result<()> {
    let root_path = Path::new(".");
    let include_dev = !args.production;

    // Check if this is a workspace
    if let Some(workspace) = WorkspaceConfig::detect(root_path) {
        eprintln!(
            "Detected workspace with {} members:",
            workspace.members.len()
        );
        for member in &workspace.members {
            eprintln!("  - {} ({})", member.name, member.path.display());
        }

        install_workspace(
            &workspace,
            include_dev,
            args.ignore_scripts,
            args.no_package_lock,
            args.assume_yes,
            !args.no_sandbox,
        )
        .await
    } else {
        install(
            root_path,
            include_dev,
            args.ignore_scripts,
            args.no_package_lock,
            args.assume_yes,
            !args.no_sandbox,
        )
        .await
    }
}

/// Install a workspace - collects deps from all members and installs to root
async fn install_workspace(
    workspace: &WorkspaceConfig,
    include_dev: bool,
    ignore_scripts: bool,
    no_package_lock: bool,
    assume_yes: bool,
    sandbox: bool,
) -> Result<()> {
    use nary_lib::{workspace::is_workspace_protocol, Dependency};
    use std::sync::atomic::AtomicU64;

    let root_path = &workspace.root_path;
    // Safe to ignore: dir may already exist
    let _ = fs::create_dir(root_path.join("node_modules"));
    let client = create_client()?;

    // Collect all dependencies from all workspace members
    let mut all_deps: Vec<Dependency> = Vec::new();
    let mut workspace_links: Vec<(String, String)> = Vec::new(); // (from_member, to_member)

    // Add root dependencies
    if let Ok(root_deps) = path_to_dependencies(root_path, include_dev) {
        all_deps.extend(root_deps);
    }

    // Add dependencies from each member
    for member in &workspace.members {
        if let Ok(member_deps) = path_to_dependencies(&member.abs_path, include_dev) {
            for dep in member_deps {
                if is_workspace_protocol(&dep.requested) {
                    // This is a workspace reference - record it for symlinking
                    workspace_links.push((member.name.clone(), dep.name.clone()));
                } else {
                    // Regular dependency - add to install list if not already there
                    if !all_deps
                        .iter()
                        .any(|d| d.name == dep.name && d.requested == dep.requested)
                    {
                        all_deps.push(dep);
                    }
                }
            }
        }
    }

    // Check for existing lockfile
    let lockfile_path = root_path.join("package-lock.json");
    let root = path_to_root_dependency(root_path)?;

    let depends = if let Some(lock) = read_package_lock(&lockfile_path) {
        eprintln!("Using existing package-lock.json");
        deps_from_lockfile(&lock)
    } else {
        // Spinner for resolution phase
        let spinner = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.cyan} {msg}")
                    .unwrap(),
            )
            .with_finish(ProgressFinish::AndClear);
        spinner.enable_steady_tick(std::time::Duration::from_millis(100));
        spinner.set_message("Resolving workspace dependencies...");

        let depends = calculate_depends(&client, &root, &all_deps, |name, version| {
            spinner.set_message(format!("Resolving {}@{}", name, version));
        })
        .await?;

        spinner.finish_and_clear();

        // Write package-lock.json
        if !no_package_lock {
            let lock = build_package_lock(&root.name, &root.resolved, &depends);
            write_package_lock(&lockfile_path, &lock)?;
            eprintln!("Created package-lock.json");
        }

        depends
    };

    // Install all dependencies
    let multi = MultiProgress::new();
    let total = depends.len() as u64;
    let pb = multi.add(ProgressBar::new(total));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
            .unwrap(),
    );
    pb.set_message("Installing...");

    let counter = Arc::new(AtomicU64::new(0));
    let node_modules = root_path.join("node_modules");
    let runner = Arc::new(LifecycleRunner::with_sandbox(&node_modules, sandbox));

    let results: Vec<(String, bool, nary_lib::Result<()>, Option<ScriptAudit>)> =
        stream::iter(depends.iter().map(|(dep, info)| {
            let client = client.clone();
            let dep = dep.clone();
            let info = info.clone();
            let counter = counter.clone();
            let pb = pb.clone();
            let runner = runner.clone();
            async move {
                let result = install_dep_with_tarball_url(
                    &client,
                    &dep,
                    &info.install_path,
                    info.tarball_url.as_deref(),
                    info.integrity.as_deref(),
                )
                .await;

                let audit = if !ignore_scripts
                    && result.is_ok()
                    && !info.install_path.contains("/node_modules/")
                {
                    let scripts = runner.get_lifecycle_scripts(&dep.name);
                    if scripts.is_empty() {
                        None
                    } else {
                        Some(ScriptAudit {
                            package: dep.name.clone(),
                            scripts,
                        })
                    }
                } else {
                    None
                };

                let count = counter.fetch_add(1, Ordering::Relaxed);
                pb.set_position(count + 1);
                (dep.name.clone(), dep.is_optional, result, audit)
            }
        }))
        .buffer_unordered(MAX_CONCURRENT)
        .collect()
        .await;

    pb.finish_and_clear();

    // Check for errors and collect script audits
    let mut audits: Vec<ScriptAudit> = Vec::new();
    for (name, is_optional, result, audit) in results {
        if let Err(e) = result {
            if is_optional {
                eprintln!("warn: optional dependency {} failed: {}", name, e);
            } else {
                return Err(e.into());
            }
        } else if let Some(a) = audit {
            audits.push(a);
        }
    }

    // Handle lifecycle scripts if needed
    if !ignore_scripts {
        prompt_and_run_lifecycle_scripts(&audits, &runner, assume_yes, false);
    }

    // Create symlinks for workspace members in root node_modules
    let node_modules = root_path.join("node_modules");
    for member in &workspace.members {
        let link_path = node_modules.join(&member.name);

        // Create parent dir for scoped packages (safe to ignore if exists)
        if member.name.contains('/') {
            if let Some(parent) = link_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
        }

        // Remove existing before creating symlink (safe to ignore if doesn't exist)
        let _ = fs::remove_file(&link_path);
        let _ = fs::remove_dir_all(&link_path);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = pathdiff::diff_paths(&member.abs_path, &node_modules)
                .unwrap_or_else(|| member.abs_path.clone());
            if let Err(e) = symlink(&target, &link_path) {
                eprintln!("warn: failed to symlink {}: {}", member.name, e);
            } else {
                eprintln!("Linked {} -> {}", member.name, target.display());
            }
        }

        #[cfg(not(unix))]
        {
            eprintln!("warn: workspace symlinks not supported on this platform");
        }
    }

    // Create node_modules in each member with symlinks to workspace deps
    for member in &workspace.members {
        let member_node_modules = member.abs_path.join("node_modules");
        let _ = fs::create_dir_all(&member_node_modules);

        // Create .bin directory
        let _ = fs::create_dir_all(member_node_modules.join(".bin"));

        // Symlink to root node_modules for hoisted deps
        // For each workspace reference, create symlink to sibling
        for (from_member, to_member) in &workspace_links {
            if from_member == &member.name {
                if let Some(target_member) = workspace.get_member(to_member) {
                    let link_path = member_node_modules.join(to_member);

                    // Create parent dir for scoped packages
                    if to_member.contains('/') {
                        if let Some(parent) = link_path.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                    }

                    let _ = fs::remove_file(&link_path);
                    let _ = fs::remove_dir_all(&link_path);

                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::symlink;
                        let target =
                            pathdiff::diff_paths(&target_member.abs_path, &member_node_modules)
                                .unwrap_or_else(|| target_member.abs_path.clone());
                        let _ = symlink(&target, &link_path);
                    }
                }
            }
        }
    }

    eprintln!("Workspace installed successfully");
    Ok(())
}

async fn install(
    root_path: &Path,
    include_dev: bool,
    ignore_scripts: bool,
    no_package_lock: bool,
    assume_yes: bool,
    sandbox: bool,
) -> Result<()> {
    use dashmap::DashMap;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    let start = Instant::now();

    // Track existing packages before install for pruning
    let mut existing_packages = std::collections::HashSet::new();
    let nm = Path::new("node_modules");
    if nm.exists() {
        scan_node_modules(nm, "", &mut existing_packages)?;
    }

    let _ = fs::create_dir("node_modules");
    let client = create_client()?;

    // Check for existing package-lock.json
    let lockfile_path = root_path.join("package-lock.json");
    let root = path_to_root_dependency(root_path)?;

    let depends = if let Some(lock) = read_package_lock(&lockfile_path) {
        eprintln!("Using existing package-lock.json");
        deps_from_lockfile(&lock)
    } else {
        let dependencies = path_to_dependencies(root_path, include_dev)?;

        // Spinner for resolution phase
        let spinner = ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.cyan} {msg}")
                    .unwrap(),
            )
            .with_finish(ProgressFinish::AndClear);
        spinner.enable_steady_tick(std::time::Duration::from_millis(100));
        spinner.set_message("Resolving dependencies...");

        let depends = calculate_depends(&client, &root, &dependencies, |name, version| {
            spinner.set_message(format!("Resolving {}@{}", name, version));
        })
        .await?;

        spinner.finish_and_clear();

        // Write package-lock.json
        if !no_package_lock {
            let lock = build_package_lock(&root.name, &root.resolved, &depends);
            write_package_lock(&lockfile_path, &lock)?;
            eprintln!("Created package-lock.json");
        }

        depends
    };

    // Set up multi-progress for tree view
    let multi = MultiProgress::new();

    // Progress bar for install phase
    let total = depends.len() as u64;
    let pb = multi.add(ProgressBar::new(total));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
            .unwrap(),
    );
    pb.set_message("Installing...");

    // Create tree display - responsive to terminal size
    // Reserve: 1 for progress bar, 2 for buffer/prompt
    let max_tree_lines = terminal_size::terminal_size()
        .map(|(_, h)| (h.0 as usize).saturating_sub(3))
        .unwrap_or(20)
        .max(5); // At least 5 lines

    // Create fixed tree line slots that will be reused
    let tree_lines: Vec<ProgressBar> = (0..max_tree_lines)
        .map(|_| {
            let line = multi.add(ProgressBar::new_spinner());
            line.set_style(
                ProgressStyle::default_spinner()
                    .template("  {msg}")
                    .unwrap(),
            );
            line.set_message("");
            line
        })
        .collect();
    let tree_lines = Arc::new(tree_lines);

    // Track in-flight packages with their install paths for tree structure
    #[derive(Clone, Debug)]
    struct InFlightPkg {
        name: String,
        path: String,
        depth: usize,
    }
    // Use DashMap instead of Mutex<Vec> to reduce contention with 50 concurrent tasks
    let in_flight: Arc<DashMap<String, InFlightPkg>> = Arc::new(DashMap::new());

    // Debounce rendering - track last render time to avoid excessive updates
    let render_epoch = Instant::now();
    let last_render_ms = Arc::new(AtomicU64::new(0));

    // Function to render a tree from in-flight packages (debounced)
    let render_tree = {
        let tree_lines = tree_lines.clone();
        let in_flight = in_flight.clone();
        let last_render_ms = last_render_ms.clone();
        move || {
            // Debounce: skip render if less than RENDER_DEBOUNCE_MS since last render
            let now_ms = render_epoch.elapsed().as_millis() as u64;
            let last = last_render_ms.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last) < RENDER_DEBOUNCE_MS {
                return; // Skip this render
            }
            last_render_ms.store(now_ms, Ordering::Relaxed);

            // Collect values from DashMap (no mutex lock needed)
            let pkgs: Vec<_> = in_flight.iter().map(|r| r.value().clone()).collect();

            // Sort packages by path for natural tree order
            let mut sorted_pkgs: Vec<_> = pkgs.iter().collect();
            sorted_pkgs.sort_by(|a, b| a.path.cmp(&b.path));

            // Render tree lines with proper indentation and connectors
            let mut lines: Vec<String> = Vec::new();

            for (idx, pkg) in sorted_pkgs.iter().enumerate() {
                // Check if this is the last item at its depth level
                let is_last_at_depth = !sorted_pkgs.iter().skip(idx + 1).any(|p| {
                    p.depth == pkg.depth && {
                        // Same parent path
                        let parent_a = pkg
                            .path
                            .rsplit_once("/node_modules/")
                            .map(|x| x.0)
                            .unwrap_or("");
                        let parent_b = p
                            .path
                            .rsplit_once("/node_modules/")
                            .map(|x| x.0)
                            .unwrap_or("");
                        parent_a == parent_b
                    }
                });

                // Build prefix: for each ancestor depth, check if there are more siblings
                let mut prefix = String::new();
                for d in 0..pkg.depth {
                    // Get the ancestor path at depth d
                    let parts: Vec<_> = pkg.path.split("/node_modules/").collect();
                    let ancestor_path = parts[..=d].join("/node_modules/");

                    // Check if there are more packages under this ancestor after current
                    let has_more_siblings = sorted_pkgs.iter().skip(idx + 1).any(|p| {
                        let p_parts: Vec<_> = p.path.split("/node_modules/").collect();
                        p_parts.len() > d && p_parts[..=d].join("/node_modules/") == ancestor_path
                    });

                    prefix.push_str(if has_more_siblings { "│  " } else { "   " });
                }

                let connector = if is_last_at_depth { "└─" } else { "├─" };
                lines.push(format!("{}{} {}", prefix, connector, pkg.name));
            }

            // Update progress bars with tree lines
            let spinner_style = ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap();
            let empty_style = ProgressStyle::default_spinner()
                .template("  {msg}")
                .unwrap();

            for (i, line_pb) in tree_lines.iter().enumerate() {
                if i < lines.len() {
                    line_pb.set_style(spinner_style.clone());
                    line_pb
                        .enable_steady_tick(std::time::Duration::from_millis(RENDER_DEBOUNCE_MS));
                    line_pb.set_message(lines[i].clone());
                } else {
                    line_pb.set_style(empty_style.clone());
                    line_pb.disable_steady_tick();
                    line_pb.set_message("");
                }
            }
        }
    };

    // Use atomic counter for thread-safe progress updates
    let counter = Arc::new(AtomicU64::new(0));

    // Create lifecycle runner to check for scripts during install (avoids post-install lag)
    let node_modules = Path::new("./node_modules");
    let runner = Arc::new(LifecycleRunner::with_sandbox(node_modules, sandbox));

    // Install dependencies with limited concurrency to avoid "too many open files"

    let results: Vec<InstallResult> = stream::iter(depends.iter().map(|(dep, info)| {
        let client = client.clone();
        let dep = dep.clone();
        let info = info.clone();
        let counter = counter.clone();
        let pb = pb.clone();
        let runner = runner.clone();
        let in_flight = in_flight.clone();
        let render_tree = render_tree.clone();
        let deprecated = info.deprecated.clone();
        async move {
            // Calculate depth from install path (count /node_modules/ segments)
            let depth = info.install_path.matches("/node_modules/").count();

            // Add to in-flight with path info (lock-free insert)
            in_flight.insert(
                info.install_path.clone(),
                InFlightPkg {
                    name: dep.name.clone(),
                    path: info.install_path.clone(),
                    depth,
                },
            );
            render_tree();

            let result = install_dep_with_tarball_url(
                &client,
                &dep,
                &info.install_path,
                info.tarball_url.as_deref(),
                info.integrity.as_deref(),
            )
            .await;

            // Check for lifecycle scripts right after install (while files are hot in cache)
            // Only check root-level packages (not nested node_modules)
            let audit = if !ignore_scripts
                && result.is_ok()
                && !info.install_path.contains("/node_modules/")
            {
                let scripts = runner.get_lifecycle_scripts(&dep.name);
                if scripts.is_empty() {
                    None
                } else {
                    Some(ScriptAudit {
                        package: dep.name.clone(),
                        scripts,
                    })
                }
            } else {
                None
            };

            // Remove from in-flight (lock-free remove)
            in_flight.remove(&info.install_path);
            render_tree();

            let count = counter.fetch_add(1, Ordering::Relaxed);
            pb.set_position(count + 1);
            (dep.name.clone(), dep.is_optional, result, audit, deprecated)
        }
    }))
    .buffer_unordered(MAX_CONCURRENT)
    .collect()
    .await;

    // Finish all progress bars properly to avoid terminal jump
    pb.finish_and_clear();
    for line in tree_lines.iter() {
        line.finish_and_clear();
    }
    drop(multi);

    // Check for errors, collect script audits, and deprecation warnings
    let mut audits: Vec<ScriptAudit> = Vec::new();
    let mut deprecations: Vec<(String, String)> = Vec::new();
    for (name, is_optional, result, audit, deprecated) in results {
        if let Err(e) = result {
            if is_optional {
                eprintln!("warn: optional dependency {} failed: {}", name, e);
            } else {
                return Err(e.into());
            }
        } else {
            if let Some(a) = audit {
                audits.push(a);
            }
            if let Some(msg) = deprecated {
                deprecations.push((name.clone(), msg));
            }
        }
    }

    // Show deprecation warnings
    if !deprecations.is_empty() {
        eprintln!();
        for (pkg, msg) in &deprecations {
            eprintln!(
                "{} {}: {}",
                "warning".if_supports_color(Stream::Stderr, |s| s.yellow()),
                pkg.if_supports_color(Stream::Stderr, |s| s.bold()),
                msg.if_supports_color(Stream::Stderr, |s| s.dimmed())
            );
        }
    }

    // Prune orphaned packages
    let expected: std::collections::HashSet<String> = depends
        .iter()
        .map(|(_, info)| {
            info.install_path
                .strip_prefix("node_modules/")
                .unwrap_or(&info.install_path)
                .to_string()
        })
        .collect();

    let orphaned: Vec<_> = existing_packages.difference(&expected).cloned().collect();
    let removed_count = orphaned.len();
    for path in &orphaned {
        let full_path = Path::new("node_modules").join(path);
        if full_path.exists() {
            let _ = fs::remove_dir_all(&full_path);
        }
    }

    // Calculate added count (packages that weren't already installed)
    let added_count = depends.len() - existing_packages.intersection(&expected).count();

    // Print summary
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64();
    eprintln!();
    if removed_count > 0 {
        eprintln!(
            "Added {} packages, removed {} packages, and audited {} packages in {:.1}s",
            added_count,
            removed_count,
            depends.len(),
            secs
        );
    } else {
        eprintln!(
            "Added {} packages and audited {} packages in {:.1}s",
            added_count,
            depends.len(),
            secs
        );
    }

    // Run audit and show vulnerability summary
    if let Ok(audit) = get_audit_summary(&client, &lockfile_path).await {
        if audit.total > 0 {
            let mut parts = Vec::new();
            if audit.low > 0 {
                parts.push(format!("{} low", audit.low));
            }
            if audit.moderate > 0 {
                parts.push(format!("{} moderate", audit.moderate));
            }
            if audit.high > 0 {
                parts.push(format!("{} high", audit.high));
            }
            if audit.critical > 0 {
                parts.push(format!("{} critical", audit.critical));
            }
            eprintln!("\n{} vulnerabilities ({})", audit.total, parts.join(", "));
        }
    }

    // Run lifecycle scripts (audits is empty if --ignore-scripts)
    prompt_and_run_lifecycle_scripts(&audits, &runner, assume_yes, true);

    Ok(())
}
