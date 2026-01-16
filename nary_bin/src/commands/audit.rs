use futures::stream::{self, StreamExt};
use owo_colors::{OwoColorize, Stream};
use snafu::OptionExt;
use std::fs;
use std::path::Path;
use tabled::{
    settings::{object::Columns, Style, Width},
    Table, Tabled,
};

use nary_lib::{
    build_audit_payload, create_client, find_max_satisfying_version, parse_audit_advisories,
    read_package_lock, Advisory, AuditResponse,
};

use crate::error::{AuditRequestFailedSnafu, NoLockfileSnafu, Result};
use crate::{commands, AuditArgs, InstallArgs, OutdatedArgs, UpdateArgs, MAX_FETCH_CONCURRENT};

pub async fn run_audit(args: &AuditArgs) -> Result<()> {
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

pub async fn run_outdated(args: &OutdatedArgs) -> Result<()> {
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
    #[derive(Tabled)]
    struct OutdatedRow {
        #[tabled(rename = "Package")]
        package: String,
        #[tabled(rename = "Current")]
        current: String,
        #[tabled(rename = "Wanted")]
        wanted: String,
        #[tabled(rename = "Latest")]
        latest: String,
        #[tabled(rename = "Type")]
        dep_type: String,
        #[tabled(rename = "Homepage")]
        homepage: String,
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
                        let homepage = metadata["homepage"].as_str().unwrap_or("").to_string();

                        // Color wanted yellow if differs from current (safe update)
                        let wanted_display = if current != wanted {
                            wanted
                                .if_supports_color(Stream::Stdout, |s| s.yellow())
                                .to_string()
                        } else {
                            wanted.clone()
                        };

                        // Color latest cyan if differs from wanted (breaking change)
                        let latest_display = if latest != wanted {
                            latest
                                .if_supports_color(Stream::Stdout, |s| s.cyan())
                                .to_string()
                        } else {
                            latest.clone()
                        };

                        Some(OutdatedRow {
                            package: name.clone(),
                            current,
                            wanted: wanted_display,
                            latest: latest_display,
                            dep_type: if *is_dev {
                                "devDependencies".to_string()
                            } else {
                                "dependencies".to_string()
                            },
                            homepage,
                        })
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

    let mut outdated: Vec<OutdatedRow> = results.into_iter().flatten().collect();
    outdated.sort_by(|a, b| a.package.to_lowercase().cmp(&b.package.to_lowercase()));

    if outdated.is_empty() {
        eprintln!("All packages are up to date.");
        return Ok(());
    }

    if args.json {
        let mut map = serde_json::Map::new();
        for row in &outdated {
            map.insert(
                row.package.clone(),
                serde_json::json!({
                    "current": row.current,
                    "wanted": row.wanted,
                    "latest": row.latest,
                    "type": row.dep_type,
                    "homepage": row.homepage,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }

    // Print table using tabled
    let mut table = Table::new(&outdated);
    table
        .with(Style::blank())
        .modify(Columns::new(5..=5), Width::truncate(50).suffix("…"));
    println!("{}", table);

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

pub async fn run_update(args: &UpdateArgs) -> Result<()> {
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

    commands::run_install(&InstallArgs::default()).await
}
