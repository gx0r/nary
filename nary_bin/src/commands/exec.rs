use futures::stream::{self, StreamExt};
use snafu::OptionExt;
use std::fs;
use std::path::Path;

use nary_lib::{create_client, get_global_dir, parse_package_spec};

use crate::error::{NoBinFieldSnafu, NoHomeDirectorySnafu, NoVersionSnafu, Result};
use crate::{commands, ExecArgs, MAX_CONCURRENT};

pub async fn run_exec(args: &ExecArgs) -> Result<()> {
    let (pkg_name, version) = parse_package_spec(&args.package);
    let sandbox = !args.no_sandbox;

    // First, check if it exists in local node_modules/.bin
    let local_bin = Path::new("node_modules/.bin");
    let bin_name = pkg_name.rsplit('/').next().unwrap_or(&pkg_name);
    let local_bin_path = local_bin.join(bin_name);

    if local_bin_path.exists() {
        // Run from local - use current directory as project root
        let project_root = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        return commands::run_binary(&local_bin_path, &args.args, sandbox, &project_root);
    }

    // Check global bin
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_bin_path = global_dir.join("bin").join(bin_name);
    if global_bin_path.exists() {
        // Run from global - use global dir as project root
        return commands::run_binary(&global_bin_path, &args.args, sandbox, &global_dir);
    }

    // Need to download and run
    let client = create_client()?;
    let requested_version = version.unwrap_or_else(|| "latest".to_string());

    // Fetch package metadata to get version info (uses cache, so fast)
    let root_dep = nary_lib::Dependency {
        name: pkg_name.clone(),
        requested: requested_version.clone(),
        resolved: String::new(),
        is_optional: false,
        alias: None,
    };
    let metadata = nary_lib::fetch_package_root_metadata(&client, &root_dep).await?;

    // Get the resolved version
    let resolved_version = if requested_version == "latest" {
        metadata["dist-tags"]["latest"]
            .as_str()
            .context(NoVersionSnafu {
                package: pkg_name.to_string(),
            })?
            .to_string()
    } else {
        // Use semver matching for version ranges
        nary_lib::find_max_satisfying_version(&metadata, &requested_version).ok_or_else(|| {
            NoVersionSnafu {
                package: pkg_name.to_string(),
            }
            .build()
        })?
    };

    // Install to temp directory
    let temp_dir = std::env::temp_dir().join(format!(
        "nary-exec-{}-{}",
        pkg_name.replace('/', "-"),
        resolved_version
    ));
    let node_modules = temp_dir.join("node_modules");
    let install_path = node_modules.join(&pkg_name);
    let pkg_json_path = install_path.join("package.json");

    // Check for package.json, not just directory (handles incomplete installs)
    if !pkg_json_path.exists() {
        eprintln!("Installing {}@{}...", pkg_name, resolved_version);

        // Remove incomplete installation if it exists
        if temp_dir.exists() {
            let _ = fs::remove_dir_all(&temp_dir);
        }

        // Create root dependency with resolved version
        let root_pkg = nary_lib::Dependency {
            name: pkg_name.clone(),
            requested: resolved_version.clone(), // Use resolved version as requested
            resolved: resolved_version.clone(),
            is_optional: false,
            alias: None,
        };

        // Resolve all dependencies
        let depends = nary_lib::calculate_depends(
            &client,
            &root_pkg,
            std::slice::from_ref(&root_pkg),
            |_, _| {},
        )
        .await?;

        // Install all dependencies in parallel
        let results: Vec<_> = stream::iter(depends.iter().map(|(dep, info)| {
            let client = client.clone();
            let dep = dep.clone();
            let info = info.clone();
            let temp_dir = temp_dir.clone();
            async move {
                // Convert relative install path to absolute path in temp dir
                let abs_install_path = temp_dir.join(&info.install_path);
                let result = nary_lib::install_dep_with_tarball_url(
                    &client,
                    &dep,
                    &abs_install_path.to_string_lossy(),
                    info.tarball_url.as_deref(),
                    info.integrity.as_deref(),
                )
                .await;
                (dep.name.clone(), result)
            }
        }))
        .buffer_unordered(MAX_CONCURRENT)
        .collect()
        .await;

        // Check for errors
        for (name, result) in results {
            if let Err(e) = result {
                eprintln!("Failed to install {}: {}", name, e);
                return Err(e.into());
            }
        }
    }

    // Find and run the binary
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
    commands::run_binary(&bin_path, &args.args, sandbox, &temp_dir)
}
