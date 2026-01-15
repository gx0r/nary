mod error;
use bytesize::ByteSize;
use error::{
    AuditRequestFailedSnafu, GlobalPackageNotFoundSnafu, MissingPackageNameSnafu, NoBinFieldSnafu,
    NoHomeDirectorySnafu, NoLatestVersionSnafu, NoLockfileSnafu, NoTarballUrlSnafu, NoVersionSnafu,
    Result, ScriptNotFoundSnafu, SymlinksNotSupportedSnafu,
};
use futures::stream::{self, StreamExt};
use owo_colors::{OwoColorize, Stream};
use snafu::OptionExt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};

use nary_lib::{
    build_audit_payload, build_package_lock, bump_version, calculate_depends,
    calculate_depends_with_options, cleanup_empty_dirs, create_client, deps_from_lockfile,
    dir_size, find_max_satisfying_version, get_audit_summary, get_global_dir,
    install_dep_with_tarball_url, parse_audit_advisories, parse_package_spec, path_to_dependencies,
    path_to_root_dependency, read_package_lock, scan_node_modules, write_package_lock, Advisory,
    AuditResponse, LifecycleRunner, ResolveOptions, ScriptAudit, WorkspaceConfig,
};

/// Maximum concurrent package installations
const MAX_CONCURRENT: usize = 50;

/// Maximum concurrent package metadata fetches
const MAX_FETCH_CONCURRENT: usize = 32;

/// Debounce interval for progress tree rendering (ms)
const RENDER_DEBOUNCE_MS: u64 = 80;

/// Result type for install operations: (name, existed, result, script_audit, deprecated)
type InstallResult = (
    String,
    bool,
    nary_lib::Result<()>,
    Option<ScriptAudit>,
    Option<String>,
);

/// nary - Fast, secure package manager
#[derive(Parser, Debug)]
#[command(name = "nary", version, about)]
struct Cli {
    /// Verbose mode (-v, -vv, -vvv, etc.)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Install dependencies from package.json
    #[command(visible_alias = "i")]
    Install(InstallArgs),

    /// Add a package to dependencies
    Add(AddArgs),

    /// Remove a package from dependencies
    #[command(visible_alias = "uninstall", visible_alias = "rm")]
    Remove(RemoveArgs),

    /// Run a script from package.json
    Run(RunArgs),

    /// Run the test script (shortcut for 'nary run test')
    #[command(visible_alias = "t")]
    Test(TestArgs),

    /// Run the start script (shortcut for 'nary run start')
    Start(StartArgs),

    /// Run the stop script (shortcut for 'nary run stop')
    Stop(StopArgs),

    /// Run stop then start scripts
    Restart(RestartArgs),

    /// List installed packages
    #[command(visible_alias = "ls")]
    List(ListArgs),

    /// Remove extraneous packages not in package.json/lockfile
    Prune(PruneArgs),

    /// Find duplicate packages in node_modules
    FindDupes(FindDupesArgs),

    /// Reduce duplication by hoisting packages
    Dedupe(DedupeArgs),

    /// Clean install from lockfile (CI/CD)
    Ci(CiArgs),

    /// Symlink a package for local development
    Link(LinkArgs),

    /// Remove a linked package
    Unlink(UnlinkArgs),

    /// Run a package binary (like npx)
    #[command(visible_alias = "x")]
    Exec(ExecArgs),

    /// Bump package version and create git tag
    Version(VersionArgs),

    /// Check dependencies for vulnerabilities
    Audit(AuditArgs),

    /// Show outdated packages
    Outdated(OutdatedArgs),

    /// Update packages to latest versions within semver range
    Update(UpdateArgs),

    /// Output the macOS sandbox profile (for testing)
    #[cfg(target_os = "macos")]
    SandboxProfile(SandboxProfileArgs),

    /// Manage the package cache
    Cache(CacheArgs),
}

#[derive(Args, Debug, Default, Clone)]
struct InstallArgs {
    /// Don't install any dev dependencies
    #[arg(long = "prod")]
    production: bool,

    /// Skip running lifecycle scripts (preinstall, install, postinstall)
    #[arg(long = "ignore-scripts")]
    ignore_scripts: bool,

    /// Don't generate package-lock.json
    #[arg(long = "no-package-lock")]
    no_package_lock: bool,

    /// Assume yes to all prompts (for CI)
    #[arg(short = 'y', long = "yes")]
    assume_yes: bool,

    /// Disable sandbox for lifecycle scripts (macOS only)
    #[arg(long = "no-sandbox")]
    no_sandbox: bool,

    /// Override the npm registry URL
    #[arg(long)]
    registry: Option<String>,
}

#[derive(Args, Debug)]
struct AddArgs {
    /// Package(s) to add (e.g., "lodash", "express@^4.0.0")
    #[arg(required = true)]
    packages: Vec<String>,

    /// Add as dev dependency
    #[arg(short = 'D', long)]
    dev: bool,

    /// Save exact version (no ^ prefix)
    #[arg(short = 'E', long)]
    exact: bool,
}

#[derive(Args, Debug)]
struct RemoveArgs {
    /// Package(s) to remove
    #[arg(required = true)]
    packages: Vec<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// Script name to run
    script: String,

    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct TestArgs {
    /// Arguments to pass to the test script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Maximum depth to display (0 = top-level only)
    #[arg(long)]
    depth: Option<usize>,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct PruneArgs {
    /// Only show what would be removed (dry run)
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct FindDupesArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
#[command(after_help = "Tip: Use --optimize to pick versions that satisfy the most semver ranges")]
struct DedupeArgs {
    /// Only show what would change (dry run)
    #[arg(long)]
    dry_run: bool,

    /// Use optimal hoisting (pick version satisfying most ranges)
    #[arg(long)]
    optimize: bool,
}

#[derive(Args, Debug)]
struct CiArgs {
    /// Skip running lifecycle scripts
    #[arg(long = "ignore-scripts")]
    ignore_scripts: bool,

    /// Assume yes to all prompts
    #[arg(short = 'y', long = "yes")]
    assume_yes: bool,

    /// Disable sandbox for lifecycle scripts (macOS only)
    #[arg(long = "no-sandbox")]
    no_sandbox: bool,
}

#[derive(Args, Debug)]
struct StartArgs {
    /// Arguments to pass to the start script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct StopArgs {
    /// Arguments to pass to the stop script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct RestartArgs {
    /// Arguments to pass to the scripts
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct LinkArgs {
    /// Package to link from global (omit to link current package globally)
    package: Option<String>,
}

#[derive(Args, Debug)]
struct UnlinkArgs {
    /// Package to unlink (omit to unlink current package from global)
    package: Option<String>,
}

#[derive(Args, Debug)]
struct ExecArgs {
    /// Package to run (e.g., "cowsay" or "typescript@5.0")
    #[arg(required = true)]
    package: String,

    /// Arguments to pass to the package binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
struct VersionArgs {
    /// Version bump type or explicit version (major, minor, patch, premajor, preminor, prepatch, prerelease, or semver)
    #[arg(required = true)]
    bump: String,

    /// Prerelease identifier (e.g., "alpha", "beta", "rc")
    #[arg(long)]
    preid: Option<String>,

    /// Don't create git tag
    #[arg(long)]
    no_git_tag_version: bool,
}

#[derive(Args, Debug)]
struct AuditArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Automatically fix vulnerabilities
    #[arg(long)]
    fix: bool,
}

#[derive(Args, Debug)]
struct OutdatedArgs {
    /// Output as JSON
    #[arg(long)]
    json: bool,

    /// Only show top-level dependencies
    #[arg(long)]
    depth: Option<usize>,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    /// Specific package(s) to update (updates all if omitted)
    packages: Vec<String>,

    /// Update to latest version, ignoring semver range
    #[arg(long)]
    latest: bool,

    /// Only show what would be updated (dry run)
    #[arg(long)]
    dry_run: bool,
}

#[cfg(target_os = "macos")]
#[derive(Args, Debug)]
struct SandboxProfileArgs {
    /// Project root directory (defaults to current directory)
    #[arg(short, long)]
    project: Option<String>,
}

#[derive(Args, Debug)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommands,
}

#[derive(Subcommand, Debug)]
enum CacheCommands {
    /// Remove all cached packages
    Clean,

    /// Show cache location and size
    Ls,
}

/// Prompt user and run lifecycle scripts if approved
fn prompt_and_run_lifecycle_scripts(
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

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        if std::env::var("RUST_BACKTRACE").is_ok() {
            eprintln!("{:?}", e);
        } else {
            eprintln!("{}", snafu::Report::from_error(&e));
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Install(args)) => run_install(&args).await,
        Some(Commands::Add(args)) => run_add(&args).await,
        Some(Commands::Remove(args)) => run_remove(&args).await,
        Some(Commands::Run(args)) => run_script(&args),
        Some(Commands::Test(args)) => run_script(&RunArgs {
            script: "test".to_string(),
            args: args.args,
        }),
        Some(Commands::List(args)) => run_list(&args),
        Some(Commands::Prune(args)) => run_prune(&args),
        Some(Commands::FindDupes(args)) => run_find_dupes(&args),
        Some(Commands::Dedupe(args)) => run_dedupe(&args).await,
        Some(Commands::Ci(args)) => run_ci(&args).await,
        Some(Commands::Start(args)) => run_script(&RunArgs {
            script: "start".to_string(),
            args: args.args,
        }),
        Some(Commands::Stop(args)) => run_script(&RunArgs {
            script: "stop".to_string(),
            args: args.args,
        }),
        Some(Commands::Restart(args)) => {
            // Run stop then start
            let _ = run_script(&RunArgs {
                script: "stop".to_string(),
                args: args.args.clone(),
            });
            run_script(&RunArgs {
                script: "start".to_string(),
                args: args.args,
            })
        }
        Some(Commands::Link(args)) => run_link(&args),
        Some(Commands::Unlink(args)) => run_unlink(&args),
        Some(Commands::Exec(args)) => run_exec(&args).await,
        Some(Commands::Version(args)) => run_version(&args),
        Some(Commands::Audit(args)) => run_audit(&args).await,
        Some(Commands::Outdated(args)) => run_outdated(&args).await,
        Some(Commands::Update(args)) => run_update(&args).await,
        #[cfg(target_os = "macos")]
        Some(Commands::SandboxProfile(args)) => run_sandbox_profile(&args),
        Some(Commands::Cache(args)) => run_cache(&args),
        None => {
            // Default: show help
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}

async fn run_install(args: &InstallArgs) -> Result<()> {
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

async fn run_add(args: &AddArgs) -> Result<()> {
    let client = create_client()?;
    let root_path = Path::new(".");

    for pkg_spec in &args.packages {
        // Parse package@version
        let (name, version) = parse_package_spec(pkg_spec);

        // Resolve version if not specified
        let resolved_version = match version {
            Some(v) => v,
            None => {
                // Fetch latest version from registry
                let dep = nary_lib::Dependency {
                    name: name.clone(),
                    requested: "*".to_string(),
                    resolved: String::new(),
                    is_optional: false,
                    alias: None,
                };
                let metadata = nary_lib::fetch_package_root_metadata(&client, &dep).await?;
                let latest =
                    metadata["dist-tags"]["latest"]
                        .as_str()
                        .context(NoLatestVersionSnafu {
                            package: name.clone(),
                        })?;
                if args.exact {
                    latest.to_string()
                } else {
                    format!("^{}", latest)
                }
            }
        };

        // Read and update package.json
        let pkg_json_path = root_path.join("package.json");
        let content = fs::read_to_string(&pkg_json_path)?;
        let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

        let section = if args.dev {
            "devDependencies"
        } else {
            "dependencies"
        };

        if pkg.get(section).is_none() {
            pkg[section] = serde_json::json!({});
        }
        pkg[section][&name] = serde_json::Value::String(resolved_version.clone());

        // Write back with 2-space indent
        let output = serde_json::to_string_pretty(&pkg)?;
        fs::write(&pkg_json_path, output + "\n")?;

        eprintln!("Added {}@{} to {}", name, resolved_version, section);
    }

    // Remove package-lock.json so install re-resolves (safe to ignore if doesn't exist)
    let _ = fs::remove_file(root_path.join("package-lock.json"));

    // Run install
    run_install(&InstallArgs::default()).await
}

async fn run_remove(args: &RemoveArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    for name in &args.packages {
        let mut found = false;

        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(deps) = pkg.get_mut(section).and_then(|d| d.as_object_mut()) {
                if deps.remove(name).is_some() {
                    eprintln!("Removed {} from {}", name, section);
                    found = true;
                }
            }
        }

        if !found {
            eprintln!("warn: {} not found in package.json", name);
        }

        // Remove from node_modules
        let pkg_path = root_path.join("node_modules").join(name);
        if pkg_path.exists() {
            fs::remove_dir_all(&pkg_path)?;
            eprintln!("Removed {}", pkg_path.display());
        }
    }

    // Write back package.json
    let output = serde_json::to_string_pretty(&pkg)?;
    fs::write(&pkg_json_path, output + "\n")?;

    Ok(())
}

fn run_script(args: &RunArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;

    let script_cmd = pkg
        .get("scripts")
        .and_then(|s| s.get(&args.script))
        .and_then(|v| v.as_str())
        .context(ScriptNotFoundSnafu {
            script: args.script.clone(),
        })?;

    // Build command with extra args
    let full_cmd = if args.args.is_empty() {
        script_cmd.to_string()
    } else {
        format!("{} {}", script_cmd, args.args.join(" "))
    };

    // Build PATH with node_modules/.bin prepended
    let bin_path =
        fs::canonicalize("node_modules/.bin").unwrap_or_else(|_| "node_modules/.bin".into());
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_path.display(), current_path);

    // Get package name for env vars
    let pkg_name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let pkg_version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("");

    eprintln!("> {}", full_cmd);

    let status = Command::new("sh")
        .arg("-c")
        .arg(&full_cmd)
        .env("PATH", &new_path)
        .env("npm_lifecycle_event", &args.script)
        .env("npm_package_name", pkg_name)
        .env("npm_package_version", pkg_version)
        .status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

fn run_list(args: &ListArgs) -> Result<()> {
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

fn run_prune(args: &PruneArgs) -> Result<()> {
    use std::collections::HashSet;

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
    use std::sync::atomic::AtomicU64;

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
    let start = std::time::Instant::now();

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
    use dashmap::DashMap;

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
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;
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

fn run_find_dupes(args: &FindDupesArgs) -> Result<()> {
    use std::collections::HashMap;

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

async fn run_dedupe(args: &DedupeArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    let old_lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;
    let old_deps = deps_from_lockfile(&old_lock);

    // Re-resolve with fresh hoisting
    let client = create_client()?;
    let root = path_to_root_dependency(root_path)?;
    let dependencies = path_to_dependencies(root_path, true)?;

    let options = ResolveOptions {
        optimize: args.optimize,
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
        &nary_lib::RegistryConfig::default(),
        &options,
    )
    .await?;

    // Find packages that need to move
    struct MoveInfo {
        name: String,
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

    // Count hoists vs nests
    let hoisted = moves.iter().filter(|m| m.is_hoist).count();
    let nested = moves.len() - hoisted;

    eprintln!();
    if hoisted > 0 {
        eprintln!(
            "{} {} package{} can be hoisted (deduplicated)",
            "↑".if_supports_color(Stream::Stderr, |s| s.green()),
            hoisted,
            if hoisted == 1 { "" } else { "s" }
        );
    }
    if nested > 0 {
        eprintln!(
            "{} {} package{} need nesting (version conflicts)",
            "↓".if_supports_color(Stream::Stderr, |s| s.yellow()),
            nested,
            if nested == 1 { "" } else { "s" }
        );
    }
    eprintln!();

    for m in &moves {
        let arrow = if m.is_hoist {
            "↑"
                .if_supports_color(Stream::Stderr, |s| s.green())
                .to_string()
        } else {
            "↓"
                .if_supports_color(Stream::Stderr, |s| s.yellow())
                .to_string()
        };
        eprintln!(
            "  {} {} {} → {}",
            arrow,
            m.name.if_supports_color(Stream::Stderr, |s| s.bold()),
            m.old_path.if_supports_color(Stream::Stderr, |s| s.dimmed()),
            m.new_path
        );
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

async fn run_ci(args: &CiArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    // Require lockfile
    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    // Remove node_modules completely
    let node_modules = root_path.join("node_modules");
    if node_modules.exists() {
        eprintln!("Removing node_modules...");
        fs::remove_dir_all(&node_modules)?;
    }
    fs::create_dir(&node_modules)?;

    // Get deps directly from lockfile (no resolution)
    let depends = deps_from_lockfile(&lock);
    let client = create_client()?;
    let sandbox = !args.no_sandbox;

    eprintln!("Installing {} packages from lockfile...", depends.len());

    // Simple progress bar for CI
    let pb = ProgressBar::new(depends.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
            .unwrap(),
    );

    let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let runner = Arc::new(LifecycleRunner::with_sandbox(&node_modules, sandbox));
    let ignore_scripts = args.ignore_scripts;

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
                pb.set_message(dep.name.clone());
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

    // Handle lifecycle scripts
    if !args.ignore_scripts {
        prompt_and_run_lifecycle_scripts(&audits, &runner, args.assume_yes, false);
    }

    eprintln!("Done.");
    Ok(())
}

fn run_link(args: &LinkArgs) -> Result<()> {
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_modules = global_dir.join("lib/node_modules");
    let global_bin = global_dir.join("bin");

    fs::create_dir_all(&global_modules)?;
    fs::create_dir_all(&global_bin)?;

    match &args.package {
        None => {
            // Link current package globally
            let root_path = Path::new(".");
            let pkg_json_path = root_path.join("package.json");
            let content = fs::read_to_string(&pkg_json_path)?;
            let pkg: serde_json::Value = serde_json::from_str(&content)?;

            let name = pkg
                .get("name")
                .and_then(|n| n.as_str())
                .context(MissingPackageNameSnafu)?;

            let abs_path = fs::canonicalize(root_path)?;
            let link_path = global_modules.join(name);

            // Create parent for scoped packages
            if name.contains('/') {
                if let Some(parent) = link_path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }

            // Remove existing link/dir
            let _ = fs::remove_file(&link_path);
            let _ = fs::remove_dir_all(&link_path);

            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(&abs_path, &link_path)?;
            }
            #[cfg(not(unix))]
            {
                return SymlinksNotSupportedSnafu.fail();
            }

            eprintln!("Linked {} -> {}", name, abs_path.display());

            // Link binaries
            if let Some(bin) = pkg.get("bin") {
                link_binaries(name, &abs_path, bin, &global_bin)?;
            }

            Ok(())
        }
        Some(pkg_name) => {
            // Link global package to local node_modules
            let global_pkg = global_modules.join(pkg_name);
            if !global_pkg.exists() {
                return GlobalPackageNotFoundSnafu {
                    package: pkg_name.to_string(),
                }
                .fail();
            }

            let local_modules = Path::new("node_modules");
            fs::create_dir_all(local_modules)?;

            let link_path = local_modules.join(pkg_name);

            // Create parent for scoped packages
            if pkg_name.contains('/') {
                if let Some(parent) = link_path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }

            let _ = fs::remove_file(&link_path);
            let _ = fs::remove_dir_all(&link_path);

            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                // Get relative path from local node_modules to global
                let target =
                    pathdiff::diff_paths(&global_pkg, local_modules).unwrap_or(global_pkg.clone());
                symlink(&target, &link_path)?;
            }
            #[cfg(not(unix))]
            {
                return SymlinksNotSupportedSnafu.fail();
            }

            eprintln!("Linked {} -> {}", pkg_name, global_pkg.display());
            Ok(())
        }
    }
}

fn link_binaries(
    pkg_name: &str,
    pkg_path: &Path,
    bin: &serde_json::Value,
    bin_dir: &Path,
) -> Result<()> {
    match bin {
        serde_json::Value::String(script) => {
            // Single binary with package name
            let bin_name = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
            create_bin_link(bin_name, pkg_path, script, bin_dir)?;
        }
        serde_json::Value::Object(map) => {
            // Multiple binaries
            for (bin_name, script_val) in map {
                if let Some(script) = script_val.as_str() {
                    create_bin_link(bin_name, pkg_path, script, bin_dir)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn create_bin_link(bin_name: &str, pkg_path: &Path, script: &str, bin_dir: &Path) -> Result<()> {
    let target = pkg_path.join(script);
    let link_path = bin_dir.join(bin_name);

    let _ = fs::remove_file(&link_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&target, &link_path)?;
        // Make executable
        use std::os::unix::fs::PermissionsExt;
        if target.exists() {
            let mut perms = fs::metadata(&target)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&target, perms)?;
        }
    }

    eprintln!("  Linked binary: {}", bin_name);
    Ok(())
}

fn run_unlink(args: &UnlinkArgs) -> Result<()> {
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_modules = global_dir.join("lib/node_modules");
    let global_bin = global_dir.join("bin");

    match &args.package {
        None => {
            // Unlink current package from global
            let root_path = Path::new(".");
            let pkg_json_path = root_path.join("package.json");
            let content = fs::read_to_string(&pkg_json_path)?;
            let pkg: serde_json::Value = serde_json::from_str(&content)?;

            let name = pkg
                .get("name")
                .and_then(|n| n.as_str())
                .context(MissingPackageNameSnafu)?;

            let link_path = global_modules.join(name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path).or_else(|_| fs::remove_dir_all(&link_path))?;
                eprintln!("Unlinked {} from global", name);
            } else {
                eprintln!("Package {} not linked globally", name);
            }

            // Remove binaries
            if let Some(bin) = pkg.get("bin") {
                unlink_binaries(name, bin, &global_bin)?;
            }

            Ok(())
        }
        Some(pkg_name) => {
            // Unlink package from local node_modules
            let link_path = Path::new("node_modules").join(pkg_name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path).or_else(|_| fs::remove_dir_all(&link_path))?;
                eprintln!("Unlinked {} from local node_modules", pkg_name);
            } else {
                eprintln!("Package {} not found in node_modules", pkg_name);
            }
            Ok(())
        }
    }
}

fn unlink_binaries(pkg_name: &str, bin: &serde_json::Value, bin_dir: &Path) -> Result<()> {
    match bin {
        serde_json::Value::String(_) => {
            let bin_name = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
            let link_path = bin_dir.join(bin_name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path)?;
                eprintln!("  Unlinked binary: {}", bin_name);
            }
        }
        serde_json::Value::Object(map) => {
            for bin_name in map.keys() {
                let link_path = bin_dir.join(bin_name);
                if link_path.exists() || link_path.is_symlink() {
                    fs::remove_file(&link_path)?;
                    eprintln!("  Unlinked binary: {}", bin_name);
                }
            }
        }
        _ => {}
    }
    Ok(())
}

async fn run_exec(args: &ExecArgs) -> Result<()> {
    let (pkg_name, version) = parse_package_spec(&args.package);

    // First, check if it exists in local node_modules/.bin
    let local_bin = Path::new("node_modules/.bin");
    let bin_name = pkg_name.rsplit('/').next().unwrap_or(&pkg_name);
    let local_bin_path = local_bin.join(bin_name);

    if local_bin_path.exists() {
        // Run from local
        return run_binary(&local_bin_path, &args.args);
    }

    // Check global bin
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_bin_path = global_dir.join("bin").join(bin_name);
    if global_bin_path.exists() {
        return run_binary(&global_bin_path, &args.args);
    }

    // Need to download and run
    eprintln!("Downloading {}...", args.package);

    let client = create_client()?;
    let dep = nary_lib::Dependency {
        name: pkg_name.clone(),
        requested: version.unwrap_or_else(|| "*".to_string()),
        resolved: String::new(),
        is_optional: false,
        alias: None,
    };

    // Fetch package metadata to get version and tarball
    let metadata = nary_lib::fetch_package_root_metadata(&client, &dep).await?;
    let latest = metadata["dist-tags"]["latest"]
        .as_str()
        .context(NoVersionSnafu {
            package: pkg_name.to_string(),
        })?;

    let version_meta = &metadata["versions"][latest];
    let tarball_url = version_meta["dist"]["tarball"]
        .as_str()
        .context(NoTarballUrlSnafu {
            package: pkg_name.to_string(),
        })?;

    // Install to temp directory
    let temp_dir = std::env::temp_dir().join(format!(
        "nary-exec-{}-{}",
        pkg_name.replace('/', "-"),
        latest
    ));
    let install_path = temp_dir.join("node_modules").join(&pkg_name);

    if !install_path.exists() {
        fs::create_dir_all(install_path.parent().unwrap())?;
        nary_lib::install_dep_with_tarball_url(
            &client,
            &dep,
            &install_path.to_string_lossy(),
            Some(tarball_url),
            None,
        )
        .await?;
    }

    // Find and run the binary
    let pkg_json_path = install_path.join("package.json");
    let content = fs::read_to_string(&pkg_json_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;

    let bin_script = if let Some(bin) = pkg.get("bin") {
        match bin {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(map) => {
                // Try package name first, then first binary
                map.get(bin_name)
                    .or_else(|| map.values().next())
                    .and_then(|v| v.as_str())
                    .map(String::from)
            }
            _ => None,
        }
    } else {
        None
    };

    let bin_script = bin_script.context(NoBinFieldSnafu {
        package: pkg_name.to_string(),
    })?;

    let bin_path = install_path.join(&bin_script);
    run_binary(&bin_path, &args.args)
}

fn run_binary(bin_path: &Path, args: &[String]) -> Result<()> {
    let status = Command::new(bin_path).args(args).status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn run_version(args: &VersionArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");
    let content = fs::read_to_string(&pkg_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    let current = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");

    let new_version = bump_version(current, &args.bump, args.preid.as_deref())?;

    // Run preversion script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("preversion") {
            eprintln!("Running preversion script...");
            let _ = run_script(&RunArgs {
                script: "preversion".to_string(),
                args: vec![],
            });
        }
    }

    // Update package.json
    pkg["version"] = serde_json::Value::String(new_version.clone());
    let output = serde_json::to_string_pretty(&pkg)?;
    fs::write(&pkg_json_path, output + "\n")?;

    eprintln!("v{}", new_version);

    // Run version script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("version") {
            eprintln!("Running version script...");
            let _ = run_script(&RunArgs {
                script: "version".to_string(),
                args: vec![],
            });
        }
    }

    // Git operations (unless --no-git-tag-version)
    if !args.no_git_tag_version {
        // Check if we're in a git repo
        let git_check = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .output();

        if let Ok(output) = git_check {
            if output.status.success() {
                // Stage package.json
                let _ = Command::new("git").args(["add", "package.json"]).status();

                // Commit
                let commit_msg = format!("v{}", new_version);
                let _ = Command::new("git")
                    .args(["commit", "-m", &commit_msg])
                    .status();

                // Tag
                let tag = format!("v{}", new_version);
                let _ = Command::new("git").args(["tag", &tag]).status();

                eprintln!("Created git commit and tag: {}", tag);
            }
        }
    }

    // Run postversion script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("postversion") {
            eprintln!("Running postversion script...");
            let _ = run_script(&RunArgs {
                script: "postversion".to_string(),
                args: vec![],
            });
        }
    }

    Ok(())
}

async fn run_audit(args: &AuditArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");

    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    let audit_payload = build_audit_payload(&lock);
    let client = create_client()?;

    // Send audit request to npm registry (bulk advisory endpoint)
    let packages = audit_payload.as_object().map(|o| o.len()).unwrap_or(0);
    let total_versions: usize = audit_payload
        .as_object()
        .map(|o| {
            o.values()
                .filter_map(|v| v.as_array())
                .map(|a| a.len())
                .sum()
        })
        .unwrap_or(0);
    eprintln!(
        "Auditing {} packages ({} versions)...",
        packages, total_versions
    );

    let resp = client
        .post("https://registry.npmjs.org/-/npm/v1/security/advisories/bulk")
        .header("Content-Type", "application/json")
        .body(audit_payload.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        return AuditRequestFailedSnafu {
            status: resp.status(),
        }
        .fail();
    }

    if args.json {
        let audit_result: serde_json::Value = resp.json().await?;
        println!("{}", serde_json::to_string_pretty(&audit_result)?);
        return Ok(());
    }

    let audit_result: AuditResponse = resp.json().await?;
    let advisory_list: Vec<Advisory> = parse_audit_advisories(&audit_result);

    // Count by severity
    let mut critical: u64 = 0;
    let mut high: u64 = 0;
    let mut moderate: u64 = 0;
    let mut low: u64 = 0;
    let mut info: u64 = 0;

    for advisory in &advisory_list {
        match advisory.severity.as_str() {
            "critical" => critical += 1,
            "high" => high += 1,
            "moderate" => moderate += 1,
            "low" => low += 1,
            "info" => info += 1,
            _ => {}
        }
    }

    let total = critical + high + moderate + low + info;

    if total == 0 {
        eprintln!("No vulnerabilities found.");
        return Ok(());
    }

    eprintln!(
        "\nFound {} vulnerabilities ({} advisories):\n",
        total,
        advisory_list.len()
    );

    if critical > 0 {
        eprintln!(
            "  {}",
            format!("{} critical", critical).if_supports_color(Stream::Stderr, |s| s.red())
        );
    }
    if high > 0 {
        eprintln!(
            "  {}",
            format!("{} high", high).if_supports_color(Stream::Stderr, |s| s.bright_red())
        );
    }
    if moderate > 0 {
        eprintln!(
            "  {}",
            format!("{} moderate", moderate).if_supports_color(Stream::Stderr, |s| s.yellow())
        );
    }
    if low > 0 {
        eprintln!(
            "  {}",
            format!("{} low", low).if_supports_color(Stream::Stderr, |s| s.cyan())
        );
    }
    if info > 0 {
        eprintln!("  {} info", info);
    }

    // Show advisory details
    if !advisory_list.is_empty() {
        eprintln!("\nAdvisories:\n");
        for advisory in &advisory_list {
            let severity_colored: String = match advisory.severity.as_str() {
                "critical" => advisory
                    .severity
                    .if_supports_color(Stream::Stderr, |s| s.red())
                    .to_string(),
                "high" => advisory
                    .severity
                    .if_supports_color(Stream::Stderr, |s| s.bright_red())
                    .to_string(),
                "moderate" => advisory
                    .severity
                    .if_supports_color(Stream::Stderr, |s| s.yellow())
                    .to_string(),
                "low" => advisory
                    .severity
                    .if_supports_color(Stream::Stderr, |s| s.cyan())
                    .to_string(),
                _ => advisory.severity.clone(),
            };

            eprintln!(
                "  #{} {} - {}",
                advisory.id, severity_colored, advisory.title
            );
            eprintln!("      Package: {}", advisory.package);
            if let Some(url) = &advisory.url {
                eprintln!("      More info: {}", url);
            }
            eprintln!();
        }
    }

    if args.fix {
        eprintln!("Attempting to fix vulnerabilities...");
        // For now, just suggest running update
        eprintln!("Run 'nary update' to update packages to latest versions.");
    } else {
        eprintln!("Run 'nary audit --fix' to attempt automatic fixes.");
    }

    // Exit with non-zero if vulnerabilities found
    if critical > 0 || high > 0 {
        std::process::exit(1);
    }

    Ok(())
}

async fn run_outdated(args: &OutdatedArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");
    let lockfile_path = root_path.join("package-lock.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;

    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    // Collect all dependencies from package.json
    let mut deps_to_check: Vec<(String, String, bool)> = Vec::new(); // (name, wanted, is_dev)

    if let Some(deps) = pkg.get("dependencies").and_then(|d| d.as_object()) {
        for (name, version) in deps {
            if let Some(v) = version.as_str() {
                deps_to_check.push((name.clone(), v.to_string(), false));
            }
        }
    }

    if let Some(deps) = pkg.get("devDependencies").and_then(|d| d.as_object()) {
        for (name, version) in deps {
            if let Some(v) = version.as_str() {
                deps_to_check.push((name.clone(), v.to_string(), true));
            }
        }
    }

    if deps_to_check.is_empty() {
        eprintln!("No dependencies found.");
        return Ok(());
    }

    let client = create_client()?;

    // Check each dependency
    struct OutdatedInfo {
        current: String,
        wanted: String,
        latest: String,
        dep_type: String,
    }

    eprintln!("Checking {} packages...", deps_to_check.len());

    // Fetch all packages in parallel
    let results: Vec<_> = stream::iter(deps_to_check.iter().map(|(name, wanted_range, is_dev)| {
        let client = &client;
        let lock = &lock;
        async move {
            // Get current installed version from lockfile
            let lock_path = format!("node_modules/{}", name);
            let current = lock
                .packages
                .get(&lock_path)
                .and_then(|e| e.version.as_ref())
                .map(|v| v.to_string())
                .unwrap_or_else(|| "MISSING".to_string());

            // Fetch latest from registry
            let dep = nary_lib::Dependency {
                name: name.clone(),
                requested: wanted_range.clone(),
                resolved: String::new(),
                is_optional: false,
                alias: None,
            };

            match nary_lib::fetch_package_root_metadata(client, &dep).await {
                Ok(metadata) => {
                    let latest = metadata["dist-tags"]["latest"]
                        .as_str()
                        .unwrap_or("?")
                        .to_string();

                    // Calculate "wanted" - the max version satisfying the semver range
                    let wanted = find_max_satisfying_version(&metadata, wanted_range)
                        .unwrap_or_else(|| latest.clone());

                    // Only show if there's something to update
                    if current != wanted || current != latest {
                        Some((
                            name.clone(),
                            OutdatedInfo {
                                current,
                                wanted,
                                latest,
                                dep_type: if *is_dev {
                                    "dev".to_string()
                                } else {
                                    "prod".to_string()
                                },
                            },
                        ))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    eprintln!("warn: could not fetch {}: {}", name, e);
                    None
                }
            }
        }
    }))
    .buffer_unordered(MAX_FETCH_CONCURRENT)
    .collect()
    .await;

    let outdated: Vec<_> = results.into_iter().flatten().collect();

    if outdated.is_empty() {
        eprintln!("All packages are up to date.");
        return Ok(());
    }

    if args.json {
        let mut map = serde_json::Map::new();
        for (name, info) in &outdated {
            map.insert(
                name.clone(),
                serde_json::json!({
                    "current": info.current,
                    "wanted": info.wanted,
                    "latest": info.latest,
                    "type": info.dep_type,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }

    // Print table
    println!(
        "{:<30} {:>12} {:>12} {:>12} {:>8}",
        "Package", "Current", "Wanted", "Latest", "Type"
    );
    println!("{}", "-".repeat(78));

    for (name, info) in &outdated {
        let current_str = format!("{:>12}", info.current);
        let current_display = if info.current == "MISSING" {
            current_str
                .if_supports_color(Stream::Stdout, |s| s.red())
                .to_string()
        } else if info.current != info.wanted {
            current_str
                .if_supports_color(Stream::Stdout, |s| s.yellow())
                .to_string()
        } else {
            current_str
        };

        let latest_str = format!("{:>12}", info.latest);
        let latest_display = if info.wanted != info.latest {
            latest_str
                .if_supports_color(Stream::Stdout, |s| s.cyan())
                .to_string()
        } else {
            latest_str
        };

        println!(
            "{:<30} {} {:>12} {} {:>8}",
            name, current_display, info.wanted, latest_display, info.dep_type
        );
    }

    println!();
    println!(
        "{} = update available within semver range (run 'nary update')",
        "yellow".if_supports_color(Stream::Stdout, |s| s.yellow())
    );
    println!(
        "{} = newer major/minor available (run 'nary update --latest')",
        "cyan".if_supports_color(Stream::Stdout, |s| s.cyan())
    );

    Ok(())
}

async fn run_update(args: &UpdateArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");
    let lockfile_path = root_path.join("package-lock.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    let lock = read_package_lock(&lockfile_path).context(NoLockfileSnafu)?;

    let client = create_client()?;

    // Collect dependencies to update
    let mut updates: Vec<(String, String, String, bool)> = Vec::new(); // (name, current, new_version, is_dev)

    let sections = [("dependencies", false), ("devDependencies", true)];

    for (section, is_dev) in sections {
        if let Some(deps) = pkg.get(section).and_then(|d| d.as_object()) {
            for (name, version_val) in deps {
                // Skip if specific packages requested and this isn't one
                if !args.packages.is_empty() && !args.packages.contains(name) {
                    continue;
                }

                let wanted_range = version_val.as_str().unwrap_or("*");

                // Get current version
                let lock_path = format!("node_modules/{}", name);
                let current = lock
                    .packages
                    .get(&lock_path)
                    .and_then(|e| e.version.as_ref())
                    .map(|v| v.to_string())
                    .unwrap_or_default();

                // Fetch metadata
                let dep = nary_lib::Dependency {
                    name: name.clone(),
                    requested: wanted_range.to_string(),
                    resolved: String::new(),
                    is_optional: false,
                    alias: None,
                };

                match nary_lib::fetch_package_root_metadata(&client, &dep).await {
                    Ok(metadata) => {
                        let new_version = if args.latest {
                            // Use absolute latest
                            metadata["dist-tags"]["latest"]
                                .as_str()
                                .unwrap_or(&current)
                                .to_string()
                        } else {
                            // Use max satisfying semver range
                            find_max_satisfying_version(&metadata, wanted_range)
                                .unwrap_or_else(|| current.clone())
                        };

                        if new_version != current && !current.is_empty() {
                            updates.push((name.clone(), current, new_version, is_dev));
                        }
                    }
                    Err(e) => {
                        eprintln!("warn: could not fetch {}: {}", name, e);
                    }
                }
            }
        }
    }

    if updates.is_empty() {
        eprintln!("All packages are up to date.");
        return Ok(());
    }

    // Show what will be updated
    eprintln!("Packages to update:\n");
    for (name, current, new_ver, _is_dev) in &updates {
        eprintln!("  {} {} -> {}", name, current, new_ver);
    }
    eprintln!();

    if args.dry_run {
        eprintln!("Run without --dry-run to apply updates.");
        return Ok(());
    }

    // Update package.json if --latest (changes the version ranges)
    if args.latest {
        for (name, _current, new_ver, is_dev) in &updates {
            let section = if *is_dev {
                "devDependencies"
            } else {
                "dependencies"
            };
            if let Some(deps) = pkg.get_mut(section).and_then(|d| d.as_object_mut()) {
                deps.insert(
                    name.clone(),
                    serde_json::Value::String(format!("^{}", new_ver)),
                );
            }
        }
        let output = serde_json::to_string_pretty(&pkg)?;
        fs::write(&pkg_json_path, output + "\n")?;
        eprintln!("Updated package.json");
    }

    // Remove lockfile and reinstall to get new versions
    fs::remove_file(&lockfile_path)?;
    eprintln!("Removed package-lock.json, reinstalling...\n");

    run_install(&InstallArgs::default()).await
}

/// Output the macOS sandbox profile for use in tests
#[cfg(target_os = "macos")]
fn run_sandbox_profile(args: &SandboxProfileArgs) -> Result<()> {
    let project_root = match &args.project {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    let profile = nary_lib::generate_sandbox_profile(&project_root);
    print!("{}", profile);
    Ok(())
}

/// Manage the package cache
fn run_cache(args: &CacheArgs) -> Result<()> {
    match &args.command {
        CacheCommands::Clean => {
            let bytes = nary_lib::clear_cache()?;
            eprintln!("Cleared cache ({} freed)", ByteSize::b(bytes));
            Ok(())
        }
        CacheCommands::Ls => {
            let cache_dir = nary_lib::get_cache_dir()?;
            let size = dir_size(&cache_dir);
            eprintln!("Cache location: {}", cache_dir.display());
            eprintln!("Cache size: {}", ByteSize::b(size));
            Ok(())
        }
    }
}
