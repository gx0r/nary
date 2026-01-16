mod commands;
mod error;

use error::Result;
use snafu::ErrorCompat;

use clap::{Args, Parser, Subcommand};

use nary_lib::ScriptAudit;

/// Maximum concurrent package installations
pub(crate) const MAX_CONCURRENT: usize = 50;

/// Maximum concurrent package metadata fetches
pub(crate) const MAX_FETCH_CONCURRENT: usize = 32;

/// Debounce interval for progress tree rendering (ms)
pub(crate) const RENDER_DEBOUNCE_MS: u64 = 80;

/// Result type for install operations: (name, existed, result, script_audit, deprecated)
pub(crate) type InstallResult = (
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

    /// Explain why a package is installed
    #[command(visible_alias = "explain")]
    Why(WhyArgs),

    /// Output the macOS sandbox profile (for testing)
    #[cfg(target_os = "macos")]
    SandboxProfile(SandboxProfileArgs),

    /// Manage the package cache
    Cache(CacheArgs),
}

#[derive(Args, Debug, Default, Clone)]
pub(crate) struct InstallArgs {
    /// Don't install any dev dependencies
    #[arg(long = "prod")]
    pub production: bool,

    /// Skip running lifecycle scripts (preinstall, install, postinstall)
    #[arg(long = "ignore-scripts")]
    pub ignore_scripts: bool,

    /// Don't generate package-lock.json
    #[arg(long = "no-package-lock")]
    pub no_package_lock: bool,

    /// Assume yes to all prompts (for CI)
    #[arg(short = 'y', long = "yes")]
    pub assume_yes: bool,

    /// Disable sandbox for lifecycle scripts (macOS only)
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,

    /// Override the npm registry URL
    #[arg(long)]
    pub registry: Option<String>,

    /// Allow installation of packages published within the maturity period
    #[arg(long = "allow-new-packages")]
    pub allow_new_packages: bool,

    /// Use only packages from cache, never fetch from network
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug)]
pub(crate) struct AddArgs {
    /// Package(s) to add (e.g., "lodash", "express@^4.0.0")
    #[arg(required = true)]
    pub packages: Vec<String>,

    /// Add as dev dependency
    #[arg(short = 'D', long)]
    pub dev: bool,

    /// Save exact version (no ^ prefix)
    #[arg(short = 'E', long)]
    pub exact: bool,

    /// Allow installation of packages published within the maturity period
    #[arg(long = "allow-new-packages")]
    pub allow_new_packages: bool,

    /// Use only packages from cache, never fetch from network
    #[arg(long)]
    pub offline: bool,
}

#[derive(Args, Debug)]
pub(crate) struct RemoveArgs {
    /// Package(s) to remove
    #[arg(required = true)]
    pub packages: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct RunArgs {
    /// Script name to run
    pub script: String,

    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug)]
struct TestArgs {
    /// Arguments to pass to the test script
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ListArgs {
    /// Maximum depth to display (0 = top-level only)
    #[arg(long)]
    pub depth: Option<usize>,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub(crate) struct PruneArgs {
    /// Only show what would be removed (dry run)
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub(crate) struct FindDupesArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[command(after_help = "Tip: Use --optimize to pick versions that satisfy the most semver ranges")]
pub(crate) struct DedupeArgs {
    /// Only show what would change (dry run)
    #[arg(long)]
    pub dry_run: bool,

    /// Use optimal hoisting (pick version satisfying most ranges)
    #[arg(long)]
    pub optimize: bool,
}

#[derive(Args, Debug)]
pub(crate) struct CiArgs {
    /// Skip running lifecycle scripts
    #[arg(long = "ignore-scripts")]
    pub ignore_scripts: bool,

    /// Assume yes to all prompts
    #[arg(short = 'y', long = "yes")]
    pub assume_yes: bool,

    /// Disable sandbox for lifecycle scripts (macOS only)
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,

    /// Allow installation of packages published within the maturity period
    #[arg(long = "allow-new-packages")]
    pub allow_new_packages: bool,

    /// Use only packages from cache, never fetch from network
    #[arg(long)]
    pub offline: bool,
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
pub(crate) struct LinkArgs {
    /// Package to link from global (omit to link current package globally)
    pub package: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct UnlinkArgs {
    /// Package to unlink (omit to unlink current package from global)
    pub package: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct ExecArgs {
    /// Package to run (e.g., "cowsay" or "typescript@5.0")
    #[arg(required = true)]
    pub package: String,

    /// Arguments to pass to the package binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Disable sandbox (macOS only, sandbox enabled by default)
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
}

#[derive(Args, Debug)]
pub(crate) struct VersionArgs {
    /// Version bump type or explicit version (major, minor, patch, premajor, preminor, prepatch, prerelease, or semver)
    #[arg(required = true)]
    pub bump: String,

    /// Prerelease identifier (e.g., "alpha", "beta", "rc")
    #[arg(long)]
    pub preid: Option<String>,

    /// Don't create git tag
    #[arg(long)]
    pub no_git_tag_version: bool,
}

#[derive(Args, Debug)]
pub(crate) struct AuditArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Automatically fix vulnerabilities
    #[arg(long)]
    pub fix: bool,
}

#[derive(Args, Debug)]
pub(crate) struct OutdatedArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Only show top-level dependencies
    #[arg(long)]
    pub depth: Option<usize>,
}

#[derive(Args, Debug)]
pub(crate) struct UpdateArgs {
    /// Specific package(s) to update (updates all if omitted)
    pub packages: Vec<String>,

    /// Update to latest version, ignoring semver range
    #[arg(long)]
    pub latest: bool,

    /// Only show what would be updated (dry run)
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub(crate) struct WhyArgs {
    /// Package name to explain (e.g., "lodash", "lodash@4.17.21", "@babel/core")
    #[arg(required = true)]
    pub package: String,

    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Show packages that depend on this package (inverse query)
    #[arg(long)]
    pub dependents: bool,
}

#[cfg(target_os = "macos")]
#[derive(Args, Debug)]
pub(crate) struct SandboxProfileArgs {
    /// Project root directory (defaults to current directory)
    #[arg(short, long)]
    pub project: Option<String>,
}

#[derive(Args, Debug)]
pub(crate) struct CacheArgs {
    #[command(subcommand)]
    pub command: CacheCommands,
}

#[derive(Subcommand, Debug)]
pub(crate) enum CacheCommands {
    /// Remove all cached packages
    Clean,

    /// Show cache location and size
    Ls,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{}", snafu::Report::from_error(&e));
        if std::env::var("RUST_BACKTRACE").is_ok() {
            if let Some(bt) = ErrorCompat::backtrace(&e) {
                eprintln!("\n{bt}");
            }
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Install(args)) => commands::run_install(&args).await,
        Some(Commands::Add(args)) => commands::run_add(&args).await,
        Some(Commands::Remove(args)) => commands::run_remove(&args).await,
        Some(Commands::Run(args)) => commands::run_script(&args),
        Some(Commands::Test(args)) => commands::run_script(&RunArgs {
            script: "test".to_string(),
            args: args.args,
        }),
        Some(Commands::List(args)) => commands::run_list(&args),
        Some(Commands::Prune(args)) => commands::run_prune(&args),
        Some(Commands::FindDupes(args)) => commands::run_find_dupes(&args),
        Some(Commands::Dedupe(args)) => commands::run_dedupe(&args).await,
        Some(Commands::Ci(args)) => commands::run_ci(&args).await,
        Some(Commands::Start(args)) => commands::run_script(&RunArgs {
            script: "start".to_string(),
            args: args.args,
        }),
        Some(Commands::Stop(args)) => commands::run_script(&RunArgs {
            script: "stop".to_string(),
            args: args.args,
        }),
        Some(Commands::Restart(args)) => {
            // Run stop then start
            let _ = commands::run_script(&RunArgs {
                script: "stop".to_string(),
                args: args.args.clone(),
            });
            commands::run_script(&RunArgs {
                script: "start".to_string(),
                args: args.args,
            })
        }
        Some(Commands::Link(args)) => commands::run_link(&args),
        Some(Commands::Unlink(args)) => commands::run_unlink(&args),
        Some(Commands::Exec(args)) => commands::run_exec(&args).await,
        Some(Commands::Version(args)) => commands::run_version(&args),
        Some(Commands::Audit(args)) => commands::run_audit(&args).await,
        Some(Commands::Outdated(args)) => commands::run_outdated(&args).await,
        Some(Commands::Update(args)) => commands::run_update(&args).await,
        Some(Commands::Why(args)) => commands::run_why(&args),
        #[cfg(target_os = "macos")]
        Some(Commands::SandboxProfile(args)) => commands::run_sandbox_profile(&args),
        Some(Commands::Cache(args)) => commands::run_cache(&args),
        None => {
            // Default: show help
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }
    }
}
