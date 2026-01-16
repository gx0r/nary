use futures::stream::{self, StreamExt};
use snafu::OptionExt;
use std::fs;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use indicatif::{ProgressBar, ProgressStyle};

use nary_lib::{
    create_client, deps_from_lockfile, install_dep_with_tarball_url, read_package_lock,
    LifecycleRunner, ScriptAudit,
};

use crate::commands::install::prompt_and_run_lifecycle_scripts;
use crate::error::{NoLockfileSnafu, Result};
use crate::{CiArgs, MAX_CONCURRENT};

pub async fn run_ci(args: &CiArgs) -> Result<()> {
    let root_path = Path::new(".");
    let lockfile_path = root_path.join("package-lock.json");
    let offline = args.offline;

    if offline {
        eprintln!("Running in offline mode - using cached packages only");
    }

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
                    offline,
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
