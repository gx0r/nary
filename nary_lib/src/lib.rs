use node_semver::{Range, Version};
use reqwest::Client;
pub use reqwest::Client as HttpClient;
use serde_json::Value;
use snafu::ResultExt;
use std::fs;
use std::path::Path;
use tar::Archive;

#[cfg(unix)]
use std::os::unix::fs::symlink;

pub mod error;
pub use error::{Error, Result};

pub mod config;
pub use config::{
    get_global_dir, NpmrcConfig, OverridesConfig, RegistryConfig, ResolvedPackage, DEFAULT_REGISTRY,
};

mod pack;
use crate::pack::{gunzip, unpack_archive};

pub mod integrity;
pub use integrity::{compute_sha512_integrity, verify_integrity};

mod cache;
pub use crate::cache::{
    cache_tarball, clear_cache, dir_size, get_cache_dir, get_cached_tarball, load_root_metadata,
    load_root_metadata_async, load_version_metadata, load_version_metadata_async,
    save_root_metadata, save_root_metadata_async, save_version_metadata,
    save_version_metadata_async, CachedMetadata, PATH_SEGMENT_ENCODE_SET,
};

pub mod deps;
pub use deps::{
    calculate_depends, calculate_depends_with_config, calculate_depends_with_options,
    parse_overrides_from_json, parse_package_spec, path_to_dependencies, path_to_overrides,
    path_to_root_dependency, Dependency, ResolveOptions, ResolvedInfo,
};

pub mod lockfile;
pub use lockfile::{deps_from_lockfile, read_package_lock, PackageLock};

pub mod lockfile_writer;
pub use lockfile_writer::{build_package_lock, write_package_lock, PackageLockWrite};

pub mod lifecycle;
#[cfg(target_os = "macos")]
pub use lifecycle::generate_sandbox_profile;
pub use lifecycle::{LifecycleRunner, ScriptAudit, ScriptInfo, LIFECYCLE_SCRIPTS};

pub mod workspace;
pub use workspace::{WorkspaceConfig, WorkspaceMember};

pub mod version;
pub use version::{bump_version, find_max_satisfying_version, has_prerelease, VersionError};

pub mod scan;
pub use scan::{cleanup_empty_dirs, list_top_level_packages, scan_node_modules};

pub mod audit;
pub use audit::{
    build_audit_payload, get_audit_summary, get_audit_summary_from_lock, parse_audit_advisories,
    parse_audit_response, Advisory, AuditResponse, AuditResult, AuditSummary,
};

pub mod maturity;
pub use maturity::{
    check_version_maturity, get_version_publish_time, MaturityCheckResult, MaturityConfig,
    MaturityFallbackInfo, DEFAULT_MATURITY_MINUTES,
};

pub mod why;
pub use why::{
    find_dependency_paths, find_dependency_paths_with_options, find_dependents,
    find_dependents_with_options, format_why_json, format_why_text, RootDependency, WhyOptions,
    WhyResult,
};

use crate::error::{
    DirCreateSnafu, GitCheckoutSnafu, GitCloneSnafu, HttpClientBuildSnafu, HttpRequestSnafu,
    HttpResponseSnafu, JsonParseSnafu, MissingFieldSnafu, NoMatchingVersionSnafu,
    NoMatureVersionSnafu, SemverRangeParseSnafu, SymlinkSnafu,
};

/// Create a shared HTTP client with connection pooling
pub fn create_client() -> Result<Client> {
    Client::builder()
        .pool_max_idle_per_host(50) // Match MAX_CONCURRENT
        .http2_adaptive_window(true) // Enable HTTP/2 with adaptive flow control
        .gzip(true)
        .brotli(true)
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context(HttpClientBuildSnafu)
}

/// Make a file executable (chmod +x)
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = fs::metadata(path) {
        let mut perms = metadata.permissions();
        let mode = perms.mode();
        // Add execute permission for owner, group, and others (where read is set)
        let new_mode = mode | ((mode & 0o444) >> 2);
        perms.set_mode(new_mode);
        // Safe to ignore: not critical if chmod fails
        let _ = fs::set_permissions(path, perms);
    }
}

/// Create a symlink for a binary, making it executable first
#[cfg(unix)]
fn create_bin_symlink(bin_dir: &Path, link_path: &Path, target_path: &Path) -> Result<()> {
    make_executable(target_path);
    let _ = fs::remove_file(link_path);
    let relative_target =
        pathdiff::diff_paths(target_path, bin_dir).unwrap_or_else(|| target_path.to_path_buf());
    symlink(&relative_target, link_path).context(SymlinkSnafu {
        link: link_path.to_path_buf(),
        target: relative_target,
    })
}

/// Link binaries from a package's bin field to node_modules/.bin/
/// The bin field can be:
/// - A string: uses package name as bin name
/// - An object: maps bin names to file paths
#[cfg(unix)]
pub fn link_package_bins(node_modules: &Path, package_name: &str) -> Result<()> {
    let package_path = node_modules.join(package_name);
    let package_json_path = package_path.join("package.json");

    // Read package.json
    let package_json = match fs::read_to_string(&package_json_path) {
        Ok(content) => content,
        Err(_) => return Ok(()), // No package.json, skip
    };

    let pkg: Value = serde_json::from_str(&package_json).context(JsonParseSnafu {
        source_desc: package_json_path.display().to_string(),
    })?;

    // Check for bin field
    let bin = match pkg.get("bin") {
        Some(b) => b,
        None => return Ok(()), // No bin field, nothing to link
    };

    // Create .bin directory
    let bin_dir = node_modules.join(".bin");
    fs::create_dir_all(&bin_dir).context(DirCreateSnafu {
        path: bin_dir.clone(),
    })?;

    // Handle both string and object formats
    match bin {
        Value::String(target) => {
            // Single binary, use package name as command
            // Handle scoped packages: @scope/pkg -> pkg
            let cmd_name = package_name.rsplit('/').next().unwrap_or(package_name);
            let link_path = bin_dir.join(cmd_name);
            let target_path = package_path.join(target);
            create_bin_symlink(&bin_dir, &link_path, &target_path)?;
        }
        Value::Object(bins) => {
            // Multiple binaries
            for (cmd_name, target) in bins {
                if let Some(target_str) = target.as_str() {
                    let link_path = bin_dir.join(cmd_name);
                    let target_path = package_path.join(target_str);
                    create_bin_symlink(&bin_dir, &link_path, &target_path)?;
                }
            }
        }
        _ => {} // Invalid bin format, skip
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn link_package_bins(_node_modules: &Path, _package_name: &str) -> Result<()> {
    // Bin linking not implemented for non-Unix platforms
    Ok(())
}

/// Check if URL is a git dependency URL
fn is_git_url(url: &str) -> bool {
    url.starts_with("git://") || url.starts_with("git+")
}

/// Normalize git URL for cloning - strip git+ prefix and upgrade git:// to https://
fn normalize_git_clone_url(url: &str) -> String {
    // Strip git+ prefix (npm lockfile convention)
    let url = url.strip_prefix("git+").unwrap_or(url);

    // Upgrade git:// to https:// (git:// was disabled by GitHub in 2022)
    if url.starts_with("git://") {
        return url.replacen("git://", "https://", 1);
    }

    url.to_string()
}

/// Install a dependency using a pre-resolved tarball URL and optional integrity hash
/// The install_path should be the full path like "node_modules/lodash" or
/// "node_modules/express/node_modules/lodash" for nested dependencies
/// If offline is true, only use cached tarballs - fail if not in cache
pub async fn install_dep_with_tarball_url(
    client: &Client,
    dep: &Dependency,
    install_path: &str,
    tarball_url: Option<&str>,
    integrity: Option<&str>,
    offline: bool,
) -> Result<()> {
    let package_path = Path::new(install_path);

    // Handle git dependencies - check both dep.requested and tarball_url (from lockfile's "resolved" field)
    let git_url_raw = if is_git_url(&dep.requested) {
        Some(dep.requested.clone())
    } else if let Some(url) = tarball_url {
        if is_git_url(url) {
            Some(url.to_string())
        } else {
            None
        }
    } else {
        None
    };

    if let Some(git_url_raw) = git_url_raw {
        use std::process::Command;

        // Skip if already installed (check for package.json, not just directory)
        // An empty directory from a failed clone should not be considered installed
        if package_path.join("package.json").exists() {
            return Ok(());
        }

        // In offline mode, git dependencies that aren't already installed fail
        if offline {
            return Err(Error::OfflineTarballNotCached {
                package: dep.name.clone(),
                version: "git".to_string(),
            });
        }

        // Remove empty directory from previous failed install (safe to ignore errors)
        if package_path.exists() {
            let _ = fs::remove_dir_all(package_path);
        }

        // Create parent directories for nested paths
        if let Some(parent) = package_path.parent() {
            fs::create_dir_all(parent).context(DirCreateSnafu {
                path: parent.to_path_buf(),
            })?;
        }

        // Split URL and ref (e.g., "git+ssh://github.com/user/repo.git#commit")
        let (repo_part, git_ref) = match git_url_raw.rfind('#') {
            Some(pos) => (&git_url_raw[..pos], Some(&git_url_raw[pos + 1..])),
            None => (git_url_raw.as_str(), None),
        };

        // Normalize the URL for cloning (strip git+ prefix, upgrade git:// to https://)
        let clone_url = normalize_git_clone_url(repo_part);

        // Check if ref is a commit hash (40 hex chars) - needs full clone
        let is_commit =
            git_ref.is_some_and(|r| r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit()));

        // Clone with system git - more robust than git2 crate
        // Handles SSH keys/agents/config/insteadOf rules automatically
        let clone_result = if is_commit {
            // Full clone needed for commit hashes
            Command::new("git")
                .args(["clone", "--quiet", &clone_url])
                .arg(package_path)
                .status()
        } else {
            // Shallow clone for branches/tags or no ref
            Command::new("git")
                .args(["clone", "--depth=1", "--quiet", &clone_url])
                .arg(package_path)
                .status()
        };

        let status = clone_result.context(GitCloneSnafu {
            url: clone_url.clone(),
        })?;

        if !status.success() {
            return Err(std::io::Error::other(format!(
                "git clone failed with exit code {:?}",
                status.code()
            )))
            .context(GitCloneSnafu {
                url: clone_url.clone(),
            });
        }

        // Checkout specific ref if provided
        if let Some(ref_str) = git_ref {
            if is_commit {
                // For commits, just checkout directly
                let checkout_status = Command::new("git")
                    .args(["-C"])
                    .arg(package_path)
                    .args(["checkout", "--quiet", ref_str])
                    .status()
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    })?;

                if !checkout_status.success() {
                    return Err(std::io::Error::other(format!(
                        "git checkout failed with exit code {:?}",
                        checkout_status.code()
                    )))
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    });
                }
            } else {
                // For shallow clones, fetch the specific ref first
                let fetch_status = Command::new("git")
                    .args(["-C"])
                    .arg(package_path)
                    .args(["fetch", "--depth=1", "--quiet", "origin", ref_str])
                    .status()
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    })?;

                if !fetch_status.success() {
                    return Err(std::io::Error::other(format!(
                        "git fetch failed with exit code {:?}",
                        fetch_status.code()
                    )))
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    });
                }

                let checkout_status = Command::new("git")
                    .args(["-C"])
                    .arg(package_path)
                    .args(["checkout", "--quiet", ref_str])
                    .status()
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    })?;

                if !checkout_status.success() {
                    return Err(std::io::Error::other(format!(
                        "git checkout failed with exit code {:?}",
                        checkout_status.code()
                    )))
                    .context(GitCheckoutSnafu {
                        url: clone_url.clone(),
                        git_ref: ref_str.to_string(),
                    });
                }
            }
        }

        // Link binaries for git dependencies (only for root-level packages)
        if !install_path.contains("/node_modules/") {
            let node_modules = Path::new("node_modules");
            let install_name = dep.alias.as_ref().unwrap_or(&dep.name);
            link_package_bins(node_modules, install_name)?;
        }
        return Ok(());
    }

    // In offline mode, use only cached tarballs
    let (tarball_bytes, tarball_source) = if offline {
        let bytes = get_cached_tarball(&dep.name, &dep.resolved).await?;
        // Use a descriptive source for error messages in offline mode
        let source = format!("cache:{}@{}", dep.name, dep.resolved);
        (bytes, source)
    } else {
        // Use provided tarball URL, or fall back to fetching metadata
        let url = match tarball_url {
            Some(url) => url.to_string(),
            None => {
                let metadata = fetch_package_root_metadata(client, dep).await?;
                let versions = &metadata["versions"].as_object().ok_or_else(|| {
                    MissingFieldSnafu {
                        package: dep.name.clone(),
                        field: "versions",
                    }
                    .build()
                })?;
                let version_metadata = versions.get(&dep.resolved).ok_or_else(|| {
                    MissingFieldSnafu {
                        package: dep.name.clone(),
                        field: "resolved version",
                    }
                    .build()
                })?;
                version_metadata["dist"]["tarball"]
                    .as_str()
                    .ok_or_else(|| {
                        MissingFieldSnafu {
                            package: dep.name.clone(),
                            field: "dist.tarball",
                        }
                        .build()
                    })?
                    .to_string()
            }
        };

        let bytes = cache_tarball(client, &dep.name, &dep.resolved, &url, integrity).await?;
        (bytes, url)
    };

    // Move blocking I/O (gunzip + unpack) to a blocking thread pool
    // Note: async-tar doesn't handle PAX extended headers well, so we use sync tar
    let package_path_owned = package_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let tarball = gunzip(tarball_bytes, &tarball_source)?;
        let mut archive = Archive::new(tarball.as_slice());
        unpack_archive(&mut archive, &package_path_owned, &tarball_source)
    })
    .await
    .map_err(|e| Error::ExtractionTaskPanic {
        message: e.to_string(),
    })??;

    // Link binaries to node_modules/.bin/ (only for root-level packages)
    if !install_path.contains("/node_modules/") {
        let node_modules = Path::new("node_modules");
        let install_name = dep.alias.as_ref().unwrap_or(&dep.name);
        link_package_bins(node_modules, install_name)?;
    }

    Ok(())
}

/// Execute a request and parse the response as JSON
async fn fetch_json(request: reqwest::RequestBuilder, url: &str) -> Result<Value> {
    let response = request.send().await.context(HttpRequestSnafu {
        url: url.to_string(),
    })?;
    let body = response.text().await.context(HttpResponseSnafu {
        url: url.to_string(),
    })?;
    serde_json::from_str(&body).context(JsonParseSnafu {
        source_desc: url.to_string(),
    })
}

/// Metadata for a specific version of a package
pub async fn fetch_package_version_metadata(
    client: &Client,
    dep: &Dependency,
    version: &str,
) -> Result<Value> {
    let config = RegistryConfig::default();
    fetch_package_version_metadata_with_config(client, dep, version, &config).await
}

/// Metadata for a specific version of a package (with config)
pub async fn fetch_package_version_metadata_with_config(
    client: &Client,
    dep: &Dependency,
    version: &str,
    config: &RegistryConfig,
) -> Result<Value> {
    let url = config.version_url(&dep.name, version);
    let request = config.authenticated_get(client, &url);
    fetch_json(request, &url).await
}

/// Metadata for all versions (simple fetch, no caching)
pub async fn fetch_package_root_metadata(client: &Client, dep: &Dependency) -> Result<Value> {
    let config = RegistryConfig::default();
    fetch_package_root_metadata_with_config(client, dep, &config).await
}

/// Metadata for all versions (with config)
pub async fn fetch_package_root_metadata_with_config(
    client: &Client,
    dep: &Dependency,
    config: &RegistryConfig,
) -> Result<Value> {
    let url = config.metadata_url(&dep.name);
    let request = config.authenticated_get(client, &url);
    fetch_json(request, &url).await
}

/// Result of a conditional fetch
pub enum FetchResult {
    /// 304 Not Modified - use cached data
    NotModified,
    /// 200 OK - fresh data with optional new ETag
    Fresh { data: Value, etag: Option<String> },
}

/// Fetch root metadata with conditional request (If-None-Match) if ETag is provided
pub async fn fetch_package_root_metadata_conditional(
    client: &Client,
    dep: &Dependency,
    cached_etag: Option<&str>,
) -> Result<FetchResult> {
    let config = RegistryConfig::default();
    fetch_package_root_metadata_conditional_with_config(client, dep, cached_etag, &config).await
}

/// Fetch root metadata with conditional request (with config)
pub async fn fetch_package_root_metadata_conditional_with_config(
    client: &Client,
    dep: &Dependency,
    cached_etag: Option<&str>,
    config: &RegistryConfig,
) -> Result<FetchResult> {
    let url = config.metadata_url(&dep.name);
    let mut request = config.authenticated_get(client, &url);

    if let Some(etag) = cached_etag {
        request = request.header("If-None-Match", etag);
    }

    let response = request
        .send()
        .await
        .context(HttpRequestSnafu { url: url.clone() })?;

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchResult::NotModified);
    }

    let new_etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body = response
        .text()
        .await
        .context(HttpResponseSnafu { url: url.clone() })?;

    let data: Value = serde_json::from_str(&body).context(JsonParseSnafu {
        source_desc: url.clone(),
    })?;

    Ok(FetchResult::Fresh {
        data,
        etag: new_etag,
    })
}

/// Get the current platform (os, cpu) matching npm's naming conventions
pub fn get_current_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else {
        "unknown"
    };

    let cpu = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "ia32"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else {
        "unknown"
    };

    (os, cpu)
}

/// Check if the current platform matches the given os/cpu constraints
/// Returns true if no constraints are specified (None means "all platforms")
pub fn platform_matches(os: Option<&[String]>, cpu: Option<&[String]>) -> bool {
    let (current_os, current_cpu) = get_current_platform();
    let os_ok = os
        .map(|list| list.iter().any(|o| o == current_os))
        .unwrap_or(true);
    let cpu_ok = cpu
        .map(|list| list.iter().any(|c| c == current_cpu))
        .unwrap_or(true);
    os_ok && cpu_ok
}

pub fn fetch_matching_version_metadata<'a>(
    dep: &Dependency,
    root_metadata: &'a serde_json::Value,
) -> Result<(&'a String, &'a Value)> {
    let required_version: Range = dep.requested.parse().map_err(|_| {
        SemverRangeParseSnafu {
            package: dep.name.clone(),
            range: dep.requested.clone(),
        }
        .build()
    })?;

    let versions = &root_metadata["versions"].as_object().ok_or_else(|| {
        MissingFieldSnafu {
            package: dep.name.clone(),
            field: "versions",
        }
        .build()
    })?;

    // Collect all versions matching the semver range
    let mut matching: Vec<(&String, &Value, Version)> = Vec::new();
    for (ver_str, ver_data) in versions.iter() {
        // Skip versions that don't parse as valid semver (e.g., canary/nightly builds)
        let Ok(parsed) = ver_str.parse::<Version>() else {
            continue;
        };
        if parsed.satisfies(&required_version) {
            matching.push((ver_str, ver_data, parsed));
        }
    }

    // Sort by semver descending (highest first) to match npm behavior
    matching.sort_by(|a, b| b.2.cmp(&a.2));

    // Return the highest matching version
    matching.first().map(|(s, v, _)| (*s, *v)).ok_or_else(|| {
        NoMatchingVersionSnafu {
            package: dep.name.clone(),
            requested: dep.requested.clone(),
        }
        .build()
    })
}

/// Result from version resolution with maturity checking
#[derive(Clone, Debug)]
pub struct VersionResolveResult<'a> {
    /// The selected version string
    pub version: &'a String,
    /// The metadata for the selected version
    pub metadata: &'a Value,
    /// If a fallback was used due to maturity, contains info about the skipped version
    pub maturity_fallback: Option<MaturityFallbackInfo>,
}

/// Fetch matching version metadata with maturity filtering.
///
/// This function extends `fetch_matching_version_metadata` to also filter by package age.
/// If the newest matching version is too new (published within the maturity period),
/// it will fall back to the next oldest version that meets the age requirement.
pub fn fetch_matching_version_metadata_with_maturity<'a>(
    dep: &Dependency,
    root_metadata: &'a serde_json::Value,
    maturity_config: &MaturityConfig,
) -> Result<VersionResolveResult<'a>> {
    let required_version: Range = dep.requested.parse().map_err(|_| {
        SemverRangeParseSnafu {
            package: dep.name.clone(),
            range: dep.requested.clone(),
        }
        .build()
    })?;

    let versions = &root_metadata["versions"].as_object().ok_or_else(|| {
        MissingFieldSnafu {
            package: dep.name.clone(),
            field: "versions",
        }
        .build()
    })?;

    // Collect all versions matching the semver range
    let mut matching: Vec<(&String, &Value, Version)> = Vec::new();
    for (ver_str, ver_data) in versions.iter() {
        // Skip versions that don't parse as valid semver (e.g., canary/nightly builds)
        let Ok(parsed) = ver_str.parse::<Version>() else {
            continue;
        };
        if parsed.satisfies(&required_version) {
            matching.push((ver_str, ver_data, parsed));
        }
    }

    // Sort by semver descending (highest first) to match npm behavior
    matching.sort_by(|a, b| b.2.cmp(&a.2));

    if matching.is_empty() {
        return Err(NoMatchingVersionSnafu {
            package: dep.name.clone(),
            requested: dep.requested.clone(),
        }
        .build());
    }

    // If maturity checking is disabled for this package, return the highest version
    if !maturity_config.should_check(&dep.name) {
        let (version, metadata, _) = matching[0];
        return Ok(VersionResolveResult {
            version,
            metadata,
            maturity_fallback: None,
        });
    }

    // Find the first version that passes maturity check
    let mut skipped_fallback: Option<MaturityFallbackInfo> = None;

    for (ver_str, ver_data, _) in &matching {
        match check_version_maturity(root_metadata, ver_str, maturity_config) {
            MaturityCheckResult::Mature | MaturityCheckResult::NoTimeData => {
                return Ok(VersionResolveResult {
                    version: ver_str,
                    metadata: ver_data,
                    maturity_fallback: skipped_fallback,
                });
            }
            MaturityCheckResult::TooNew {
                published_at,
                age_minutes,
            } => {
                // Record the first skipped version for user feedback
                if skipped_fallback.is_none() {
                    skipped_fallback = Some(MaturityFallbackInfo {
                        skipped_version: (*ver_str).clone(),
                        skipped_published_at: published_at,
                        skipped_age_minutes: age_minutes,
                        required_age_minutes: maturity_config.minimum_age_minutes,
                    });
                }
            }
        }
    }

    // All matching versions are too new
    let fallback = skipped_fallback.unwrap();
    Err(NoMatureVersionSnafu {
        package: dep.name.clone(),
        requested: dep.requested.clone(),
        newest_version: fallback.skipped_version,
        age_minutes: fallback.skipped_age_minutes,
        required_minutes: fallback.required_age_minutes,
    }
    .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn make_dep(name: &str, requested: &str) -> Dependency {
        Dependency {
            name: name.to_string(),
            requested: requested.to_string(),
            resolved: String::new(),
            is_optional: false,
            alias: None,
            install_path: None,
        }
    }

    #[test]
    fn test_maturity_disabled_returns_highest_version() {
        let dep = make_dep("lodash", "^4.0.0");
        let metadata = json!({
            "versions": {
                "4.17.20": {},
                "4.17.21": {},
                "4.16.0": {}
            }
        });

        let config = MaturityConfig::disabled();
        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.version, "4.17.21");
        assert!(result.maturity_fallback.is_none());
    }

    #[test]
    fn test_maturity_falls_back_to_older_version() {
        let dep = make_dep("lodash", "^4.0.0");

        // 4.17.21 published 1 hour ago (too new)
        // 4.17.20 published 1 week ago (mature)
        let recent = Utc::now() - Duration::hours(1);
        let old = Utc::now() - Duration::days(7);

        let metadata = json!({
            "versions": {
                "4.17.20": {},
                "4.17.21": {},
                "4.16.0": {}
            },
            "time": {
                "4.17.21": recent.to_rfc3339(),
                "4.17.20": old.to_rfc3339(),
                "4.16.0": old.to_rfc3339()
            }
        });

        let config = MaturityConfig {
            minimum_age_minutes: 4320, // 3 days
            excluded_packages: vec![],
            allow_new_packages: false,
        };

        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.version, "4.17.20"); // Fell back to older version
        assert!(result.maturity_fallback.is_some());

        let fallback = result.maturity_fallback.unwrap();
        assert_eq!(fallback.skipped_version, "4.17.21");
    }

    #[test]
    fn test_maturity_excluded_package_gets_newest() {
        let dep = make_dep("lodash", "^4.0.0");

        let recent = Utc::now() - Duration::hours(1);

        let metadata = json!({
            "versions": {
                "4.17.20": {},
                "4.17.21": {}
            },
            "time": {
                "4.17.21": recent.to_rfc3339(),
                "4.17.20": recent.to_rfc3339()
            }
        });

        let config = MaturityConfig {
            minimum_age_minutes: 4320,
            excluded_packages: vec!["lodash".to_string()], // Excluded!
            allow_new_packages: false,
        };

        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.version, "4.17.21"); // Gets newest despite being new
        assert!(result.maturity_fallback.is_none());
    }

    #[test]
    fn test_maturity_all_versions_too_new_returns_error() {
        let dep = make_dep("new-package", "^1.0.0");

        let recent = Utc::now() - Duration::hours(1);

        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.0.1": {}
            },
            "time": {
                "1.0.0": recent.to_rfc3339(),
                "1.0.1": recent.to_rfc3339()
            }
        });

        let config = MaturityConfig {
            minimum_age_minutes: 4320, // 3 days
            excluded_packages: vec![],
            allow_new_packages: false,
        };

        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("No mature version"));
    }

    #[test]
    fn test_maturity_no_time_data_treats_as_mature() {
        let dep = make_dep("old-package", "^1.0.0");

        // No time field at all
        let metadata = json!({
            "versions": {
                "1.0.0": {},
                "1.0.1": {}
            }
        });

        let config = MaturityConfig {
            minimum_age_minutes: 4320,
            excluded_packages: vec![],
            allow_new_packages: false,
        };

        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.version, "1.0.1"); // Gets newest (no time = treated as mature)
        assert!(result.maturity_fallback.is_none());
    }

    #[test]
    fn test_maturity_scoped_package_exclusion() {
        let dep = make_dep("@types/node", "^18.0.0");

        let recent = Utc::now() - Duration::hours(1);

        let metadata = json!({
            "versions": {
                "18.0.0": {},
                "18.19.0": {}
            },
            "time": {
                "18.0.0": recent.to_rfc3339(),
                "18.19.0": recent.to_rfc3339()
            }
        });

        let config = MaturityConfig {
            minimum_age_minutes: 4320,
            excluded_packages: vec!["@types".to_string()], // Exclude @types/* packages
            allow_new_packages: false,
        };

        let result = fetch_matching_version_metadata_with_maturity(&dep, &metadata, &config);

        assert!(result.is_ok());
        let result = result.unwrap();
        assert_eq!(result.version, "18.19.0"); // Gets newest (excluded by prefix)
    }
}
