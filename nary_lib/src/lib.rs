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
pub use config::{get_global_dir, NpmrcConfig, RegistryConfig, DEFAULT_REGISTRY};

mod pack;
use crate::pack::{gunzip, unpack_archive};

pub mod integrity;
pub use integrity::{compute_sha512_integrity, verify_integrity};

mod cache;
pub use crate::cache::{
    cache_tarball, clear_cache, dir_size, get_cache_dir, load_root_metadata,
    load_root_metadata_async, load_version_metadata, load_version_metadata_async,
    save_root_metadata, save_root_metadata_async, save_version_metadata,
    save_version_metadata_async, CachedMetadata, PATH_SEGMENT_ENCODE_SET,
};

pub mod deps;
pub use deps::{
    calculate_depends, calculate_depends_with_config, calculate_depends_with_options,
    parse_package_spec, path_to_dependencies, path_to_root_dependency, Dependency, ResolveOptions,
    ResolvedInfo,
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

use crate::error::{
    DirCreateSnafu, GitCheckoutSnafu, GitCloneSnafu, HttpClientBuildSnafu, HttpRequestSnafu,
    HttpResponseSnafu, JsonParseSnafu, MissingFieldSnafu, NoMatchingVersionSnafu,
    SemverRangeParseSnafu, SymlinkSnafu,
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

/// Install a dependency using a pre-resolved tarball URL and optional integrity hash
/// The install_path should be the full path like "node_modules/lodash" or
/// "node_modules/express/node_modules/lodash" for nested dependencies
pub async fn install_dep_with_tarball_url(
    client: &Client,
    dep: &Dependency,
    install_path: &str,
    tarball_url: Option<&str>,
    integrity: Option<&str>,
) -> Result<()> {
    let package_path = Path::new(install_path);

    // Handle git dependencies
    if dep.requested.starts_with("git://") {
        use git2::{build::RepoBuilder, FetchOptions, Repository};

        // Skip if already installed (check for package.json, not just directory)
        // An empty directory from a failed clone should not be considered installed
        if package_path.join("package.json").exists() {
            return Ok(());
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

        // Check if a ref looks like a commit hash (40 hex chars)
        fn is_commit_hash(s: &str) -> bool {
            s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
        }

        if let Some(x) = dep.requested.rfind('#') {
            let (repo, hash) = dep.requested.split_at(x);
            let repo_url = repo.replacen("git://", "https://", 1);
            let git_ref = &hash[1..]; // Skip the '#'

            if is_commit_hash(git_ref) {
                // Commit hash: need full clone to ensure commit is available
                let repo_cloned =
                    Repository::clone(&repo_url, package_path).context(GitCloneSnafu {
                        url: repo_url.to_string(),
                    })?;
                let obj = repo_cloned
                    .revparse_single(git_ref)
                    .context(GitCheckoutSnafu {
                        url: repo.to_string(),
                        git_ref: git_ref.to_string(),
                    })?;
                repo_cloned
                    .set_head_detached(obj.id())
                    .context(GitCheckoutSnafu {
                        url: repo.to_string(),
                        git_ref: git_ref.to_string(),
                    })?;
                repo_cloned.checkout_head(None).context(GitCheckoutSnafu {
                    url: repo.to_string(),
                    git_ref: git_ref.to_string(),
                })?;
            } else {
                // Tag or branch: shallow clone, then fetch specific ref, then checkout
                let mut fetch_opts = FetchOptions::new();
                fetch_opts.depth(1);
                // Clone default branch first (shallow)
                let repo_cloned = RepoBuilder::new()
                    .fetch_options(fetch_opts)
                    .clone(&repo_url, package_path)
                    .context(GitCloneSnafu {
                        url: repo_url.to_string(),
                    })?;
                // Fetch the specific tag/branch
                let mut remote = repo_cloned.find_remote("origin").context(GitCloneSnafu {
                    url: repo_url.to_string(),
                })?;
                let mut fetch_opts2 = FetchOptions::new();
                fetch_opts2.depth(1);
                let refspec = format!("refs/tags/{0}:refs/tags/{0}", git_ref);
                remote
                    .fetch(&[&refspec], Some(&mut fetch_opts2), None)
                    .or_else(|_| {
                        // Try as branch if tag fetch fails
                        let refspec = format!("refs/heads/{0}:refs/remotes/origin/{0}", git_ref);
                        remote.fetch(&[&refspec], Some(&mut fetch_opts2), None)
                    })
                    .context(GitCheckoutSnafu {
                        url: repo.to_string(),
                        git_ref: git_ref.to_string(),
                    })?;
                // Checkout the ref
                let obj = repo_cloned
                    .revparse_single(git_ref)
                    .context(GitCheckoutSnafu {
                        url: repo.to_string(),
                        git_ref: git_ref.to_string(),
                    })?;
                repo_cloned
                    .set_head_detached(obj.id())
                    .context(GitCheckoutSnafu {
                        url: repo.to_string(),
                        git_ref: git_ref.to_string(),
                    })?;
                repo_cloned.checkout_head(None).context(GitCheckoutSnafu {
                    url: repo.to_string(),
                    git_ref: git_ref.to_string(),
                })?;
            }
        } else {
            // No ref specified: shallow clone default branch
            let repo_url = dep.requested.replacen("git://", "https://", 1);
            let mut fetch_opts = FetchOptions::new();
            fetch_opts.depth(1);
            RepoBuilder::new()
                .fetch_options(fetch_opts)
                .clone(&repo_url, package_path)
                .context(GitCloneSnafu { url: repo_url })?;
        }

        // Link binaries for git dependencies (only for root-level packages)
        if !install_path.contains("/node_modules/") {
            let node_modules = Path::new("node_modules");
            let install_name = dep.alias.as_ref().unwrap_or(&dep.name);
            link_package_bins(node_modules, install_name)?;
        }
        return Ok(());
    }

    // Use provided tarball URL, or fall back to fetching metadata
    let tarball_url = match tarball_url {
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

    let tarball_bytes =
        cache_tarball(client, &dep.name, &dep.resolved, &tarball_url, integrity).await?;

    // Move blocking I/O (gunzip + unpack) to a blocking thread pool
    // Note: async-tar doesn't handle PAX extended headers well, so we use sync tar
    let package_path_owned = package_path.to_path_buf();
    let tarball_url_owned = tarball_url.clone();
    tokio::task::spawn_blocking(move || {
        let tarball = gunzip(tarball_bytes, &tarball_url_owned)?;
        let mut archive = Archive::new(tarball.as_slice());
        unpack_archive(&mut archive, &package_path_owned, &tarball_url_owned)
    })
    .await
    .unwrap_or_else(|e| panic!("extraction task panicked: {e}"))?;

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
