use reqwest::Client;
use snafu::ResultExt;

use dashmap::DashMap;
use futures::stream::{self, StreamExt};

use indexmap::IndexMap;
use serde_json::Value;
use std::{fs::File, io, path::Path, sync::Arc};
use tokio::sync::watch;

use crate::config::RegistryConfig;
use crate::error::{FileReadSnafu, JsonParseSnafu, OfflineMetadataNotCachedSnafu, Result};
use crate::{
    fetch_matching_version_metadata_with_maturity,
    fetch_package_root_metadata_conditional_with_config, fetch_package_root_metadata_with_config,
    fetch_package_version_metadata_with_config, load_root_metadata_async,
    load_version_metadata_async, platform_matches, save_root_metadata_async,
    save_version_metadata_async, FetchResult, MaturityConfig, MaturityFallbackInfo,
};

/// Thread-safe cache for package metadata to avoid duplicate network requests
pub struct MetadataCache {
    client: Client,
    config: RegistryConfig,
    root: DashMap<String, Arc<Value>>, // package name -> root metadata (all versions)
    version: DashMap<String, Arc<Value>>, // "name@version" -> specific version metadata
    // Track in-flight requests to coalesce duplicate concurrent fetches
    root_inflight: DashMap<String, watch::Receiver<Option<Arc<Value>>>>,
    version_inflight: DashMap<String, watch::Receiver<Option<Arc<Value>>>>,
    /// Offline mode: only use cached packages, fail if not available
    offline: bool,
}

/// Wait for an in-flight request to complete and return its result
async fn wait_for_inflight<K>(
    inflight_map: &DashMap<K, watch::Receiver<Option<Arc<Value>>>>,
    key: &K,
) -> Option<Arc<Value>>
where
    K: Eq + std::hash::Hash,
{
    let mut receiver = inflight_map.get(key)?.clone();
    while receiver.borrow().is_none() {
        if receiver.changed().await.is_err() {
            break;
        }
    }
    let result = receiver.borrow().clone();
    result
}

impl MetadataCache {
    pub fn new(client: Client) -> Self {
        Self::with_config(client, RegistryConfig::default())
    }

    pub fn with_config(client: Client, config: RegistryConfig) -> Self {
        Self::with_options(client, config, false)
    }

    pub fn with_options(client: Client, config: RegistryConfig, offline: bool) -> Self {
        Self {
            client,
            config,
            root: DashMap::new(),
            version: DashMap::new(),
            root_inflight: DashMap::new(),
            version_inflight: DashMap::new(),
            offline,
        }
    }

    /// Fetch root metadata for a package, using layered caching:
    /// 1. In-memory cache (fastest)
    /// 2. Wait for in-flight request if one exists
    /// 3. Filesystem cache with ETag validation
    /// 4. Network fetch (slowest) - skipped in offline mode
    async fn get_root_metadata(&self, dep: &Dependency) -> Result<Arc<Value>> {
        // 1. Check in-memory cache first (cheap Arc clone)
        if let Some(cached) = self.root.get(&dep.name) {
            return Ok(Arc::clone(&cached));
        }

        // 2. Check if there's an in-flight request we can wait on
        if let Some(data) = wait_for_inflight(&self.root_inflight, &dep.name).await {
            return Ok(data);
        }

        // 3. Check cache again (may have been populated while we waited)
        if let Some(cached) = self.root.get(&dep.name) {
            return Ok(Arc::clone(&cached));
        }

        // 4. Start a new request - register as in-flight
        let (tx, rx) = watch::channel(None);
        self.root_inflight.insert(dep.name.clone(), rx);

        // 5. Check filesystem cache and get ETag for conditional request (async)
        let fs_cached = load_root_metadata_async(&dep.name).await;

        // In offline mode, only use filesystem cache - no network requests
        if self.offline {
            let result = match &fs_cached {
                Some(cached) if cached.data["versions"].is_object() => {
                    let data = Arc::new(cached.data.clone());
                    self.root.insert(dep.name.clone(), Arc::clone(&data));
                    data
                }
                _ => {
                    // Not in cache and offline - fail
                    self.root_inflight.remove(&dep.name);
                    return OfflineMetadataNotCachedSnafu {
                        package: dep.name.clone(),
                    }
                    .fail();
                }
            };
            let _ = tx.send(Some(Arc::clone(&result)));
            self.root_inflight.remove(&dep.name);
            return Ok(result);
        }

        // Only use ETag if cached data has valid versions field
        let cached_etag = fs_cached.as_ref().and_then(|c| {
            if c.data["versions"].is_object() {
                c.etag.as_deref()
            } else {
                None // Don't use ETag for corrupted/incomplete cache
            }
        });

        // 6. Fetch with conditional request
        let result = match fetch_package_root_metadata_conditional_with_config(
            &self.client,
            dep,
            cached_etag,
            &self.config,
        )
        .await?
        {
            FetchResult::NotModified => {
                // 304 - use cached data (with validation)
                match &fs_cached {
                    Some(cached) if cached.data["versions"].is_object() => {
                        let data = Arc::new(cached.data.clone());
                        self.root.insert(dep.name.clone(), Arc::clone(&data));
                        data
                    }
                    _ => {
                        // Cache was invalid despite 304, fetch fresh
                        let fresh = fetch_package_root_metadata_with_config(
                            &self.client,
                            dep,
                            &self.config,
                        )
                        .await?;
                        save_root_metadata_async(&dep.name, &fresh, None).await;
                        let data = Arc::new(fresh);
                        self.root.insert(dep.name.clone(), Arc::clone(&data));
                        data
                    }
                }
            }
            FetchResult::Fresh { data, etag } => {
                // 200 - save to both caches (fire-and-forget async)
                save_root_metadata_async(&dep.name, &data, etag.as_deref()).await;
                let data = Arc::new(data);
                self.root.insert(dep.name.clone(), Arc::clone(&data));
                data
            }
        };

        // 7. Notify waiters and cleanup
        // Safe to ignore: receiver may have dropped if request was cancelled
        let _ = tx.send(Some(Arc::clone(&result)));
        self.root_inflight.remove(&dep.name);

        Ok(result)
    }

    /// Fetch version metadata for a resolved dependency, using layered caching.
    /// Version metadata is immutable once published, so no ETag needed.
    /// In offline mode, only uses cache - fails if not found.
    async fn get_version_metadata(&self, dep: &Dependency) -> Result<Arc<Value>> {
        let key = format!("{}@{}", dep.name, dep.resolved);

        // 1. Check in-memory cache (cheap Arc clone)
        if let Some(cached) = self.version.get(&key) {
            return Ok(Arc::clone(&cached));
        }

        // 2. Check if there's an in-flight request we can wait on
        if let Some(data) = wait_for_inflight(&self.version_inflight, &key).await {
            return Ok(data);
        }

        // 3. Check cache again (may have been populated while we waited)
        if let Some(cached) = self.version.get(&key) {
            return Ok(Arc::clone(&cached));
        }

        // 4. Start a new request - register as in-flight
        let (tx, rx) = watch::channel(None);
        self.version_inflight.insert(key.clone(), rx);

        // 5. Check filesystem cache (async)
        if let Some(cached) = load_version_metadata_async(&dep.name, &dep.resolved).await {
            let cached = Arc::new(cached);
            self.version.insert(key.clone(), Arc::clone(&cached));
            // Safe to ignore: receiver may have dropped if request was cancelled
            let _ = tx.send(Some(Arc::clone(&cached)));
            self.version_inflight.remove(&key);
            return Ok(cached);
        }

        // In offline mode, if not in cache, fail
        if self.offline {
            self.version_inflight.remove(&key);
            return OfflineMetadataNotCachedSnafu {
                package: format!("{}@{}", dep.name, dep.resolved),
            }
            .fail();
        }

        // 6. Fetch from network and save to both caches (async)
        let metadata = fetch_package_version_metadata_with_config(
            &self.client,
            dep,
            &dep.resolved,
            &self.config,
        )
        .await?;
        save_version_metadata_async(&dep.name, &dep.resolved, &metadata).await;
        let metadata = Arc::new(metadata);
        self.version.insert(key.clone(), Arc::clone(&metadata));

        // 7. Notify waiters and cleanup
        // Safe to ignore: receiver may have dropped if request was cancelled
        let _ = tx.send(Some(Arc::clone(&metadata)));
        self.version_inflight.remove(&key);

        Ok(metadata)
    }
}

/// Info needed to install a resolved dependency
#[derive(Clone, Debug)]
pub struct ResolvedInfo {
    pub tarball_url: Option<String>,
    pub integrity: Option<String>,
    pub dependencies: Vec<(String, String)>, // (name, version_range) for lockfile
    pub install_path: String, // e.g., "node_modules/lodash" or "node_modules/express/node_modules/lodash"
    pub deprecated: Option<String>, // Deprecation warning message if package is deprecated
    pub maturity_fallback: Option<MaturityFallbackInfo>, // Info if a newer version was skipped due to maturity check
}

/// A peer dependency requirement
#[derive(Clone, Debug)]
pub struct PeerDependency {
    pub package: String,    // package that requires the peer
    pub peer_name: String,  // name of the peer dependency
    pub peer_range: String, // version range required
    pub optional: bool,     // from peerDependenciesMeta
}

/// Platform constraints for optional dependencies
#[derive(Clone, Debug, Default)]
struct PlatformConstraints {
    os: Option<Vec<String>>,  // e.g., ["darwin", "linux"] or None for any
    cpu: Option<Vec<String>>, // e.g., ["arm64", "x64"] or None for any
}

/// Detect if a package is a platform-specific binary that should be nested
/// These are packages like @esbuild/darwin-arm64, @rollup/linux-x64, etc.
fn is_platform_binary(name: &str) -> bool {
    let scopes = ["@esbuild/", "@rollup/", "@swc/"];
    let patterns = [
        "darwin-", "linux-", "win32-", "freebsd-", "android-", "netbsd-", "openbsd-", "sunos-",
    ];
    scopes.iter().any(|s| name.starts_with(s)) && patterns.iter().any(|p| name.contains(p))
}

/// Result of resolving a single dependency (fetched in parallel)
#[derive(Clone)]
struct ResolvedDepInfo {
    resolved_dep: Dependency,
    transitive_deps: Vec<Dependency>,
    tarball_url: Option<String>,
    integrity: Option<String>,
    peer_deps: Vec<PeerDependency>,
    platform: PlatformConstraints,
    deprecated: Option<String>,
    maturity_fallback: Option<MaturityFallbackInfo>,
}

/// Fetch and resolve multiple dependencies in parallel using async
/// Limited to MAX_CONCURRENT to avoid "too many open files" errors
async fn fetch_deps_parallel(
    deps: &[Dependency],
    cache: &Arc<MetadataCache>,
    maturity_config: &MaturityConfig,
) -> Vec<Result<ResolvedDepInfo>> {
    const MAX_CONCURRENT: usize = 50;

    let maturity_config = maturity_config.clone();

    stream::iter(deps.iter().map(|unresolved_dep| {
        let cache = cache.clone();
        let unresolved_dep = unresolved_dep.clone();
        let maturity_config = maturity_config.clone();
        async move {
            // Handle git dependencies specially (no maturity check for git deps)
            if unresolved_dep.requested.starts_with("git://") {
                let resolved_dep = Dependency {
                    name: unresolved_dep.name.clone(),
                    requested: unresolved_dep.requested.clone(),
                    resolved: unresolved_dep.requested.clone(),
                    is_optional: unresolved_dep.is_optional,
                    alias: unresolved_dep.alias.clone(),
                };
                return Ok(ResolvedDepInfo {
                    resolved_dep,
                    transitive_deps: vec![],
                    tarball_url: None,
                    integrity: None,
                    peer_deps: vec![],
                    platform: PlatformConstraints::default(),
                    deprecated: None,
                    maturity_fallback: None,
                });
            }

            // Fetch root metadata and resolve version with maturity filtering
            let root_metadata = cache.get_root_metadata(&unresolved_dep).await?;
            let resolve_result = fetch_matching_version_metadata_with_maturity(
                &unresolved_dep,
                &root_metadata,
                &maturity_config,
            )?;

            let resolved_dep = Dependency {
                name: unresolved_dep.name.clone(),
                requested: unresolved_dep.requested.clone(),
                resolved: resolve_result.version.to_string(),
                is_optional: unresolved_dep.is_optional,
                alias: unresolved_dep.alias.clone(),
            };

            // Fetch version metadata to get transitive dependencies and tarball URL
            let version_metadata = cache.get_version_metadata(&resolved_dep).await?;
            // Transitive deps from package metadata are not marked as optional
            let mut transitive_deps =
                serde_json_value_to_dependencies(&version_metadata["dependencies"], false);
            // Also include optional dependencies (marked as optional)
            let optional_deps =
                serde_json_value_to_dependencies(&version_metadata["optionalDependencies"], true);
            transitive_deps.extend(optional_deps);

            // Extract tarball URL and integrity for later installation
            let tarball_url = version_metadata["dist"]["tarball"]
                .as_str()
                .map(|s| s.to_string());
            let integrity = version_metadata["dist"]["integrity"]
                .as_str()
                .map(|s| s.to_string());

            // Extract peer dependencies (checks peerDependenciesMeta for optional flags)
            let peer_deps = parse_peer_dependencies(&resolved_dep.name, &version_metadata);

            // Extract platform constraints (os and cpu)
            let os = version_metadata["os"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });
            let cpu = version_metadata["cpu"].as_array().map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            });

            // Extract deprecation warning if present
            let deprecated = version_metadata["deprecated"]
                .as_str()
                .map(|s| s.to_string());

            Ok(ResolvedDepInfo {
                resolved_dep,
                transitive_deps,
                tarball_url,
                integrity,
                peer_deps,
                platform: PlatformConstraints { os, cpu },
                deprecated,
                maturity_fallback: resolve_result.maturity_fallback,
            })
        }
    }))
    .buffer_unordered(MAX_CONCURRENT)
    .collect()
    .await
}

#[derive(Clone, Debug)]
pub struct Dependency {
    pub name: String,          // actual package name to fetch from registry
    pub requested: String,     // version range from package.json (e.g., "^4.0.0")
    pub resolved: String,      // actual version to install (e.g., "4.17.21")
    pub is_optional: bool,     // from optionalDependencies
    pub alias: Option<String>, // install under this name instead (for npm: aliases)
}

impl PartialEq for Dependency {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.resolved == other.resolved
    }
}

impl Eq for Dependency {}

impl std::hash::Hash for Dependency {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.resolved.hash(state);
    }
}

/// Key for tracking installed packages: (name, version, install_path)
/// This allows the same package version to be installed in multiple locations
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct InstallKey {
    name: String,
    version: String,
    install_path: String,
}

/// Compute optimal version to hoist for each package name.
/// For each package, find the version that satisfies the most requirements.
fn compute_optimal_hoisting(
    requirements: &std::collections::BTreeMap<String, Vec<String>>,
    resolved_versions: &std::collections::BTreeMap<(String, String), String>,
) -> std::collections::BTreeMap<String, String> {
    use node_semver::{Range, Version};

    let mut optimal: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

    for (name, ranges) in requirements {
        // Collect all resolved versions for this package
        let mut versions: Vec<(&String, Version)> = resolved_versions
            .iter()
            .filter(|((n, _), _)| n == name)
            .filter_map(|(_, v)| v.parse::<Version>().ok().map(|parsed| (v, parsed)))
            .collect();

        // Sort by semver descending (newest first) so newer versions win ties
        versions.sort_by(|a, b| b.1.cmp(&a.1));
        versions.dedup_by(|a, b| a.0 == b.0);

        if versions.is_empty() {
            continue;
        }

        // For each version, count how many ranges it satisfies
        let mut best_version = versions[0].0.clone();
        let mut best_count = 0usize;

        for (version, parsed_version) in &versions {
            let mut count = 0usize;

            for range_str in ranges {
                if let Ok(range) = range_str.parse::<Range>() {
                    if parsed_version.satisfies(&range) {
                        count += 1;
                    }
                } else if range_str == *version {
                    // Exact version match
                    count += 1;
                }
            }

            if count > best_count {
                best_count = count;
                best_version = (*version).clone();
            }
        }

        optimal.insert(name.clone(), best_version);
    }

    optimal
}

/// Options for dependency resolution
#[derive(Clone, Debug, Default)]
pub struct ResolveOptions {
    /// Use optimal hoisting: pick the version that satisfies the most requirements
    /// instead of "first encountered wins" (npm default behavior)
    pub optimize: bool,
    /// Configuration for package maturity age filtering
    pub maturity: MaturityConfig,
    /// Offline mode: only use cached packages, fail if not available
    pub offline: bool,
}

pub async fn calculate_depends<F>(
    client: &Client,
    root_pkg: &Dependency,
    deps: &[Dependency],
    on_resolve: F,
) -> Result<IndexMap<Dependency, ResolvedInfo>>
where
    F: Fn(&str, &str) + Clone + Send + Sync,
{
    calculate_depends_with_options(
        client,
        root_pkg,
        deps,
        on_resolve,
        &RegistryConfig::default(),
        &ResolveOptions::default(),
    )
    .await
}

pub async fn calculate_depends_with_config<F>(
    client: &Client,
    root_pkg: &Dependency,
    deps: &[Dependency],
    on_resolve: F,
    config: &RegistryConfig,
) -> Result<IndexMap<Dependency, ResolvedInfo>>
where
    F: Fn(&str, &str) + Clone + Send + Sync,
{
    calculate_depends_with_options(
        client,
        root_pkg,
        deps,
        on_resolve,
        config,
        &ResolveOptions::default(),
    )
    .await
}

pub async fn calculate_depends_with_options<F>(
    client: &Client,
    _root_pkg: &Dependency, // Reserved for future workspace root handling
    deps: &[Dependency],
    on_resolve: F,
    config: &RegistryConfig,
    options: &ResolveOptions,
) -> Result<IndexMap<Dependency, ResolvedInfo>>
where
    F: Fn(&str, &str) + Clone + Send + Sync,
{
    use node_semver::{Range, Version};

    let mut all_peer_deps: Vec<PeerDependency> = Vec::new();
    let cache = Arc::new(MetadataCache::with_options(
        client.clone(),
        config.clone(),
        options.offline,
    ));

    // ========== PHASE 1: Collect all requirements ==========
    // Traverse the entire tree to collect (name, range) requirements
    // and resolve each to a specific version, but don't make hoisting decisions yet.

    // Track which (name, range) -> resolved_version
    let mut resolved_metadata: std::collections::BTreeMap<(String, String), ResolvedDepInfo> =
        std::collections::BTreeMap::new();

    // Track all requirements: name -> [ranges that need it]
    let mut all_requirements: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    // Track resolved versions: (name, range) -> resolved_version
    let mut resolved_versions: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();

    // Track tree structure for Phase 3: (parent_path, dep, resolved_info)
    let mut tree_entries: Vec<(String, Dependency, ResolvedDepInfo)> = Vec::new();

    // BFS through tree
    let mut pending: Vec<(String, Vec<Dependency>)> = vec![("".to_string(), deps.to_vec())];
    let mut seen_ranges: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    while !pending.is_empty() {
        let mut to_resolve: Vec<(String, Dependency)> = Vec::new();

        for (parent_path, parent_deps) in &pending {
            for dep in parent_deps {
                to_resolve.push((parent_path.clone(), dep.clone()));
            }
        }

        if to_resolve.is_empty() {
            break;
        }

        // Extract deps that need metadata fetching
        let deps_needing_fetch: Vec<Dependency> = to_resolve
            .iter()
            .filter(|(_, d)| {
                !resolved_metadata.contains_key(&(d.name.clone(), d.requested.clone()))
            })
            .map(|(_, d)| d.clone())
            .collect();

        // Fetch metadata in parallel
        if !deps_needing_fetch.is_empty() {
            let results = fetch_deps_parallel(&deps_needing_fetch, &cache, &options.maturity).await;
            for result in results {
                let info = result?;
                let key = (
                    info.resolved_dep.name.clone(),
                    info.resolved_dep.requested.clone(),
                );
                resolved_versions.insert(key.clone(), info.resolved_dep.resolved.clone());
                resolved_metadata.insert(key, info);
            }
        }

        // Collect requirements and prepare next level
        let mut next_pending: Vec<(String, Vec<Dependency>)> = Vec::new();

        for (parent_path, unresolved_dep) in &to_resolve {
            let metadata_key = (
                unresolved_dep.name.clone(),
                unresolved_dep.requested.clone(),
            );

            // Track this requirement
            all_requirements
                .entry(unresolved_dep.name.clone())
                .or_default()
                .push(unresolved_dep.requested.clone());

            let Some(info) = resolved_metadata.get(&metadata_key) else {
                continue;
            };

            // Skip optional deps that don't match the current platform
            if info.resolved_dep.is_optional
                && !platform_matches(info.platform.os.as_deref(), info.platform.cpu.as_deref())
            {
                continue;
            }

            // Store for Phase 3
            tree_entries.push((parent_path.clone(), unresolved_dep.clone(), info.clone()));

            // Queue transitive deps (using a placeholder path for now)
            let range_key = (
                unresolved_dep.name.clone(),
                unresolved_dep.requested.clone(),
            );
            if !seen_ranges.contains(&range_key) {
                seen_ranges.insert(range_key);
                if !info.transitive_deps.is_empty() {
                    // Use placeholder path - will be computed in Phase 3
                    next_pending.push((
                        format!("__placeholder__/{}", info.resolved_dep.name),
                        info.transitive_deps.to_vec(),
                    ));
                }
            }
        }

        pending = next_pending;
    }

    // ========== PHASE 2: Assign install paths and build result ==========
    // Traverse again to determine install paths

    // If optimize is enabled, pre-compute optimal versions to hoist
    let mut hoisted: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();

    if options.optimize {
        // Use optimal hoisting: pick version that satisfies the most requirements
        let optimal = compute_optimal_hoisting(&all_requirements, &resolved_versions);
        for (name, version) in optimal {
            let path = format!("node_modules/{}", name);
            hoisted.insert(name, (version, path));
        }
    }
    // Otherwise use "first encountered wins" (npm default) - hoisted stays empty initially

    let mut installed: std::collections::BTreeMap<InstallKey, (Dependency, ResolvedInfo)> =
        std::collections::BTreeMap::new();

    // Re-traverse the tree with correct parent paths
    let mut pending: Vec<(String, Vec<Dependency>)> = vec![("".to_string(), deps.to_vec())];

    while !pending.is_empty() {
        let mut to_resolve: Vec<(String, Dependency)> = Vec::new();

        for (parent_path, parent_deps) in &pending {
            for dep in parent_deps {
                to_resolve.push((parent_path.clone(), dep.clone()));
            }
        }

        if to_resolve.is_empty() {
            break;
        }

        let mut next_pending: Vec<(String, Vec<Dependency>)> = Vec::new();

        for (parent_path, unresolved_dep) in &to_resolve {
            let metadata_key = (
                unresolved_dep.name.clone(),
                unresolved_dep.requested.clone(),
            );

            let Some(info) = resolved_metadata.get(&metadata_key) else {
                continue;
            };

            // Skip optional deps that don't match the current platform
            if info.resolved_dep.is_optional
                && !platform_matches(info.platform.os.as_deref(), info.platform.cpu.as_deref())
            {
                continue;
            }

            let resolved_dep = &info.resolved_dep;

            // Call the callback
            if resolved_dep.requested.starts_with("git://") {
                on_resolve(&resolved_dep.name, "git");
            } else {
                on_resolve(&resolved_dep.name, &resolved_dep.resolved);
            }

            // Determine install path using optimal hoisting
            // Force nest platform-specific binaries under their parent package
            let install_path = if is_platform_binary(&resolved_dep.name) && !parent_path.is_empty()
            {
                format!("{}/node_modules/{}", parent_path, resolved_dep.name)
            } else if let Some((hoisted_version, hoisted_path)) = hoisted.get(&resolved_dep.name) {
                // Package already hoisted - check if we can reuse
                let can_reuse = if let Ok(range) = unresolved_dep.requested.parse::<Range>() {
                    if let Ok(version) = hoisted_version.parse::<Version>() {
                        version.satisfies(&range)
                    } else {
                        hoisted_version == &resolved_dep.resolved
                    }
                } else {
                    hoisted_version == &resolved_dep.resolved
                };

                if can_reuse {
                    hoisted_path.clone()
                } else {
                    // Version conflict - nest under parent
                    if parent_path.is_empty() {
                        format!("node_modules/{}", resolved_dep.name)
                    } else {
                        format!("{}/node_modules/{}", parent_path, resolved_dep.name)
                    }
                }
            } else {
                // Not hoisted yet - hoist this version (first encountered wins, like npm)
                let path = format!("node_modules/{}", resolved_dep.name);
                hoisted.insert(
                    resolved_dep.name.clone(),
                    (resolved_dep.resolved.clone(), path.clone()),
                );
                path
            };

            let install_key = InstallKey {
                name: resolved_dep.name.clone(),
                version: resolved_dep.resolved.clone(),
                install_path: install_path.clone(),
            };

            if installed.contains_key(&install_key) {
                continue;
            }

            let deps_for_lockfile: Vec<(String, String)> = info
                .transitive_deps
                .iter()
                .map(|d| (d.name.clone(), d.requested.clone()))
                .collect();

            let resolved_info = ResolvedInfo {
                tarball_url: info.tarball_url.clone(),
                integrity: info.integrity.clone(),
                dependencies: deps_for_lockfile,
                install_path: install_path.clone(),
                deprecated: info.deprecated.clone(),
                maturity_fallback: info.maturity_fallback.clone(),
            };

            installed.insert(install_key, (resolved_dep.clone(), resolved_info));

            all_peer_deps.extend(info.peer_deps.to_vec());

            if !info.transitive_deps.is_empty() {
                next_pending.push((install_path, info.transitive_deps.to_vec()));
            }
        }

        pending = next_pending;
    }

    // Convert to IndexMap for return
    let mut ordered_dependencies: IndexMap<Dependency, ResolvedInfo> = IndexMap::new();
    for (_, (dep, info)) in installed {
        ordered_dependencies.insert(dep, info);
    }

    // Check peer dependencies and print warnings
    check_peer_dependencies(&all_peer_deps, &ordered_dependencies);

    Ok(ordered_dependencies)
}

pub fn path_to_root_dependency(file: &Path) -> Result<Dependency> {
    let mut package = file.to_path_buf();

    if !package.ends_with("package.json") {
        package.push("package.json");
    }

    let package_json = File::open(&package).context(FileReadSnafu {
        path: package.clone(),
    })?;
    let root: Value = serde_json::from_reader(package_json).context(JsonParseSnafu {
        source_desc: package.display().to_string(),
    })?;

    let version = root["version"].as_str().unwrap_or("0.0.0").to_string();
    Ok(Dependency {
        name: root["name"].as_str().unwrap_or("root").to_string(),
        requested: version.clone(),
        resolved: version,
        is_optional: false,
        alias: None,
    })
}

pub fn path_to_dependencies(file: &Path, include_dev: bool) -> Result<Vec<Dependency>> {
    let mut package = file.to_path_buf();

    if !package.ends_with("package.json") {
        package.push("package.json");
    }

    let package_json = File::open(&package).context(FileReadSnafu {
        path: package.clone(),
    })?;

    json_to_dependencies(&package_json, include_dev, &package.display().to_string())
}

pub fn json_to_dependencies(
    mut reader: impl io::Read,
    include_dev: bool,
    source_desc: &str,
) -> Result<Vec<Dependency>> {
    let mut buffer = String::new();
    reader.read_to_string(&mut buffer).context(FileReadSnafu {
        path: std::path::PathBuf::from(source_desc),
    })?;

    let root: Value = serde_json::from_str(&buffer).context(JsonParseSnafu {
        source_desc: source_desc.to_string(),
    })?;
    let mut deps = serde_json_value_to_dependencies(&root["dependencies"], false);

    if include_dev {
        let dev_deps = serde_json_value_to_dependencies(&root["devDependencies"], false);
        deps.extend(dev_deps);
    }

    // Include optionalDependencies (marked as optional)
    let optional_deps = serde_json_value_to_dependencies(&root["optionalDependencies"], true);
    deps.extend(optional_deps);

    Ok(deps)
}

/// Parse peer dependencies from package JSON, checking peerDependenciesMeta for optional flags
fn parse_peer_dependencies(package: &str, pkg_json: &serde_json::Value) -> Vec<PeerDependency> {
    let mut vec = Vec::new();
    let peers = &pkg_json["peerDependencies"];
    let peer_meta = &pkg_json["peerDependenciesMeta"];

    if let Some(peers) = peers.as_object() {
        for (name, range) in peers {
            // Check if this peer is marked as optional in peerDependenciesMeta
            let is_optional = peer_meta
                .get(name)
                .and_then(|m| m.get("optional"))
                .and_then(|o| o.as_bool())
                .unwrap_or(false);

            vec.push(PeerDependency {
                package: package.to_string(),
                peer_name: name.clone(),
                peer_range: range.as_str().unwrap_or("*").to_string(),
                optional: is_optional,
            });
        }
    }
    vec
}

/// Check peer dependencies against resolved dependencies and print warnings
fn check_peer_dependencies(
    peer_deps: &[PeerDependency],
    resolved: &IndexMap<Dependency, ResolvedInfo>,
) {
    use node_semver::{Range, Version};

    for peer in peer_deps {
        // Find if we have the peer package in our resolved deps
        let found = resolved.keys().find(|dep| dep.name == peer.peer_name);

        match found {
            Some(dep) => {
                // Check if version satisfies range
                if let Ok(range) = peer.peer_range.parse::<Range>() {
                    if let Ok(version) = dep.resolved.parse::<Version>() {
                        if !version.satisfies(&range) {
                            eprintln!(
                                "warn: {} requires peer {} {} but found {}",
                                peer.package, peer.peer_name, peer.peer_range, dep.resolved
                            );
                        }
                    }
                }
            }
            None => {
                // Skip warning for optional peers that aren't installed
                if !peer.optional {
                    eprintln!(
                        "warn: {} requires peer {} {} which is not installed",
                        peer.package, peer.peer_name, peer.peer_range
                    );
                }
            }
        }
    }
}

/// Parse dependencies from a JSON object, optionally marking them as optional
pub fn serde_json_value_to_dependencies(
    root: &serde_json::Value,
    is_optional: bool,
) -> Vec<Dependency> {
    let mut vec = Vec::new();

    if let Some(dependencies) = root.as_object() {
        for dependency in dependencies.iter() {
            if !dependency.0.starts_with("_") {
                let raw_value = dependency.1.as_str().unwrap_or("*").to_string();
                let alias_name = dependency.0.to_string();
                let is_npm_alias = raw_value.starts_with("npm:");

                // Handle npm: alias syntax like "npm:package-name@^1.0.0"
                let (name, requested) = if let Some(rest) = raw_value.strip_prefix("npm:") {
                    // Find the last @ to split package name from version
                    if let Some(at_pos) = rest.rfind('@') {
                        let pkg_name = &rest[..at_pos];
                        let version = &rest[at_pos + 1..];
                        (pkg_name.to_string(), version.to_string())
                    } else {
                        // No version specified, use "*"
                        (rest.to_string(), "*".to_string())
                    }
                } else {
                    (alias_name.clone(), raw_value)
                };

                vec.push(Dependency {
                    name,
                    requested,
                    resolved: String::new(), // Will be resolved later
                    is_optional,
                    alias: if is_npm_alias { Some(alias_name) } else { None },
                });
            }
        }
    };

    // Collapse ESM/CJS dual-mode pairs like npm does
    // Pattern: "foo" + "foo-cjs: npm:foo@..." -> keep only the CJS version as "foo"
    collapse_esm_cjs_pairs(&mut vec);

    vec
}

/// Collapse ESM/CJS dual-mode package pairs to match npm's behavior.
/// When a package has both "foo@^X" and "foo-cjs: npm:foo@^Y", npm only installs
/// the CJS version (foo@^Y) under the name "foo", skipping the ESM version.
fn collapse_esm_cjs_pairs(deps: &mut Vec<Dependency>) {
    // Find all CJS aliases (packages with alias ending in "-cjs" that point to the same package)
    let cjs_aliases: std::collections::HashSet<String> = deps
        .iter()
        .filter_map(|d| {
            if let Some(alias) = &d.alias {
                if alias.ends_with("-cjs") {
                    // Check if the base name (without -cjs) matches the package name
                    let base_name = alias.strip_suffix("-cjs").unwrap();
                    if base_name == d.name {
                        return Some(d.name.clone());
                    }
                }
            }
            None
        })
        .collect();

    // Remove ESM versions (non-aliased entries for packages that have CJS aliases)
    deps.retain(|d| {
        if d.alias.is_none() && cjs_aliases.contains(&d.name) {
            // This is the ESM version of a dual-mode package - remove it
            false
        } else {
            true
        }
    });

    // Update CJS aliases to install as the base name (remove -cjs suffix from alias)
    for dep in deps.iter_mut() {
        if let Some(alias) = &dep.alias {
            if alias.ends_with("-cjs") && alias.strip_suffix("-cjs") == Some(&dep.name) {
                // Install as the base name, not the -cjs alias
                dep.alias = None;
            }
        }
    }
}

/// Parse a package specifier like "lodash", "express@^4.0.0", or "@scope/pkg@1.0.0"
/// into a (name, optional_version) tuple.
///
/// # Examples
/// ```
/// use nary_lib::parse_package_spec;
///
/// let (name, version) = parse_package_spec("lodash");
/// assert_eq!(name, "lodash");
/// assert_eq!(version, None);
///
/// let (name, version) = parse_package_spec("lodash@4.17.21");
/// assert_eq!(name, "lodash");
/// assert_eq!(version, Some("4.17.21".to_string()));
///
/// let (name, version) = parse_package_spec("@babel/core@^7.0.0");
/// assert_eq!(name, "@babel/core");
/// assert_eq!(version, Some("^7.0.0".to_string()));
/// ```
pub fn parse_package_spec(spec: &str) -> (String, Option<String>) {
    // Handle scoped packages: @scope/pkg@version
    if spec.starts_with('@') {
        // Find the second @ (version separator)
        if let Some(slash_pos) = spec.find('/') {
            if let Some(at_pos) = spec[slash_pos..].find('@') {
                let version_start = slash_pos + at_pos;
                let name = &spec[..version_start];
                let version = &spec[version_start + 1..];
                return (name.to_string(), Some(version.to_string()));
            }
        }
        // No version specified for scoped package
        return (spec.to_string(), None);
    }

    // Non-scoped package: find last @
    if let Some(at_pos) = spec.rfind('@') {
        if at_pos > 0 {
            let name = &spec[..at_pos];
            let version = &spec[at_pos + 1..];
            return (name.to_string(), Some(version.to_string()));
        }
    }

    (spec.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_dep(name: &str, requested: &str, resolved: &str) -> Dependency {
        Dependency {
            name: name.to_string(),
            requested: requested.to_string(),
            resolved: resolved.to_string(),
            is_optional: false,
            alias: None,
        }
    }

    fn make_dep_with_alias(name: &str, requested: &str, resolved: &str, alias: &str) -> Dependency {
        Dependency {
            name: name.to_string(),
            requested: requested.to_string(),
            resolved: resolved.to_string(),
            is_optional: false,
            alias: Some(alias.to_string()),
        }
    }

    // ========== compute_optimal_hoisting tests ==========

    #[test]
    fn test_hoisting_single_version() {
        // When only one version exists, it should be hoisted
        let mut requirements = BTreeMap::new();
        requirements.insert("lodash".to_string(), vec!["^4.0.0".to_string()]);

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("lodash".to_string(), "^4.0.0".to_string()),
            "4.17.21".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        assert_eq!(optimal.get("lodash"), Some(&"4.17.21".to_string()));
    }

    #[test]
    fn test_hoisting_multiple_ranges_same_version() {
        // Multiple ranges that all resolve to the same version
        let mut requirements = BTreeMap::new();
        requirements.insert(
            "lodash".to_string(),
            vec!["^4.0.0".to_string(), "^4.17.0".to_string()],
        );

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("lodash".to_string(), "^4.0.0".to_string()),
            "4.17.21".to_string(),
        );
        resolved_versions.insert(
            ("lodash".to_string(), "^4.17.0".to_string()),
            "4.17.21".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        assert_eq!(optimal.get("lodash"), Some(&"4.17.21".to_string()));
    }

    #[test]
    fn test_hoisting_conflicting_versions_picks_most_compatible() {
        // When we have conflicting versions, pick the one that satisfies the most ranges
        let mut requirements = BTreeMap::new();
        requirements.insert(
            "semver".to_string(),
            vec![
                "^7.0.0".to_string(), // satisfied by 7.6.0
                "^7.5.0".to_string(), // satisfied by 7.6.0
                "^6.0.0".to_string(), // NOT satisfied by 7.6.0
            ],
        );

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("semver".to_string(), "^7.0.0".to_string()),
            "7.6.0".to_string(),
        );
        resolved_versions.insert(
            ("semver".to_string(), "^7.5.0".to_string()),
            "7.6.0".to_string(),
        );
        resolved_versions.insert(
            ("semver".to_string(), "^6.0.0".to_string()),
            "6.3.1".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        // 7.6.0 satisfies 2 ranges (^7.0.0 and ^7.5.0), 6.3.1 satisfies 1 range (^6.0.0)
        // So 7.6.0 should be hoisted
        assert_eq!(optimal.get("semver"), Some(&"7.6.0".to_string()));
    }

    #[test]
    fn test_hoisting_tie_breaks_with_newer_version() {
        // When versions satisfy equal number of ranges, newer version wins
        let mut requirements = BTreeMap::new();
        requirements.insert(
            "foo".to_string(),
            vec!["^1.0.0".to_string(), "^2.0.0".to_string()],
        );

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("foo".to_string(), "^1.0.0".to_string()),
            "1.5.0".to_string(),
        );
        resolved_versions.insert(
            ("foo".to_string(), "^2.0.0".to_string()),
            "2.0.0".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        // Both versions satisfy only 1 range each, but 2.0.0 is newer
        assert_eq!(optimal.get("foo"), Some(&"2.0.0".to_string()));
    }

    // ========== is_platform_binary tests ==========

    #[test]
    fn test_platform_binary_esbuild() {
        assert!(is_platform_binary("@esbuild/darwin-arm64"));
        assert!(is_platform_binary("@esbuild/linux-x64"));
        assert!(is_platform_binary("@esbuild/win32-x64"));
        assert!(!is_platform_binary("@esbuild/core"));
        assert!(!is_platform_binary("esbuild"));
    }

    #[test]
    fn test_platform_binary_rollup() {
        assert!(is_platform_binary("@rollup/rollup-darwin-arm64"));
        assert!(is_platform_binary("@rollup/rollup-linux-x64"));
        assert!(!is_platform_binary("@rollup/plugin-node-resolve"));
        assert!(!is_platform_binary("rollup"));
    }

    #[test]
    fn test_platform_binary_swc() {
        assert!(is_platform_binary("@swc/core-darwin-arm64"));
        assert!(is_platform_binary("@swc/core-linux-x64"));
        assert!(!is_platform_binary("@swc/core"));
    }

    #[test]
    fn test_platform_binary_regular_packages() {
        assert!(!is_platform_binary("lodash"));
        assert!(!is_platform_binary("express"));
        assert!(!is_platform_binary("@types/node"));
        assert!(!is_platform_binary("@babel/core"));
    }

    // ========== collapse_esm_cjs_pairs tests ==========

    #[test]
    fn test_collapse_esm_cjs_removes_esm_version() {
        let mut deps = vec![
            make_dep("string-width", "^5.0.0", "5.1.2"),
            make_dep_with_alias("string-width", "^4.2.3", "4.2.3", "string-width-cjs"),
        ];

        collapse_esm_cjs_pairs(&mut deps);

        // Should have only one entry (the CJS version)
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "string-width");
        assert_eq!(deps[0].requested, "^4.2.3");
        assert!(deps[0].alias.is_none()); // Alias should be removed
    }

    #[test]
    fn test_collapse_esm_cjs_keeps_unrelated_packages() {
        let mut deps = vec![
            make_dep("lodash", "^4.0.0", "4.17.21"),
            make_dep("string-width", "^5.0.0", "5.1.2"),
            make_dep_with_alias("string-width", "^4.2.3", "4.2.3", "string-width-cjs"),
            make_dep("express", "^4.0.0", "4.18.2"),
        ];

        collapse_esm_cjs_pairs(&mut deps);

        // Should have 3 entries: lodash, string-width (CJS), express
        assert_eq!(deps.len(), 3);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"lodash"));
        assert!(names.contains(&"string-width"));
        assert!(names.contains(&"express"));
    }

    #[test]
    fn test_collapse_esm_cjs_different_package_alias_not_collapsed() {
        // An alias that doesn't follow the "foo-cjs -> npm:foo" pattern should not collapse
        let mut deps = vec![
            make_dep("lodash", "^4.0.0", "4.17.21"),
            make_dep_with_alias("underscore", "^1.0.0", "1.13.6", "lodash-cjs"),
        ];

        collapse_esm_cjs_pairs(&mut deps);

        // Both should remain (underscore aliased as lodash-cjs is unrelated to lodash)
        assert_eq!(deps.len(), 2);
    }

    // ========== serde_json_value_to_dependencies tests ==========

    #[test]
    fn test_parse_simple_dependencies() {
        let json = serde_json::json!({
            "lodash": "^4.17.21",
            "express": "^4.18.0"
        });

        let deps = serde_json_value_to_dependencies(&json, false);

        assert_eq!(deps.len(), 2);
        let lodash = deps.iter().find(|d| d.name == "lodash").unwrap();
        assert_eq!(lodash.requested, "^4.17.21");
        assert!(lodash.alias.is_none());
        assert!(!lodash.is_optional);
    }

    #[test]
    fn test_parse_npm_alias() {
        let json = serde_json::json!({
            "my-lodash": "npm:lodash@^4.17.21"
        });

        let deps = serde_json_value_to_dependencies(&json, false);

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "lodash");
        assert_eq!(deps[0].requested, "^4.17.21");
        assert_eq!(deps[0].alias, Some("my-lodash".to_string()));
    }

    #[test]
    fn test_parse_optional_dependencies() {
        let json = serde_json::json!({
            "fsevents": "^2.3.0"
        });

        let deps = serde_json_value_to_dependencies(&json, true);

        assert_eq!(deps.len(), 1);
        assert!(deps[0].is_optional);
    }

    #[test]
    fn test_parse_skips_internal_fields() {
        let json = serde_json::json!({
            "lodash": "^4.17.21",
            "_internal": "should-be-skipped"
        });

        let deps = serde_json_value_to_dependencies(&json, false);

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "lodash");
    }

    #[test]
    fn test_parse_git_dependency() {
        let json = serde_json::json!({
            "my-pkg": "git://github.com/user/repo.git#v1.0.0"
        });

        let deps = serde_json_value_to_dependencies(&json, false);

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "my-pkg");
        assert_eq!(deps[0].requested, "git://github.com/user/repo.git#v1.0.0");
    }

    // ========== json_to_dependencies tests ==========

    #[test]
    fn test_json_to_dependencies_with_dev() {
        use indoc::indoc;

        let package_json = indoc! {r#"
            {
                "name": "test-package",
                "dependencies": {
                    "lodash": "^4.17.21"
                },
                "devDependencies": {
                    "jest": "^29.0.0"
                }
            }
        "#};

        let deps = json_to_dependencies(package_json.as_bytes(), true, "test").unwrap();

        assert_eq!(deps.len(), 2);
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"lodash"));
        assert!(names.contains(&"jest"));
    }

    #[test]
    fn test_json_to_dependencies_without_dev() {
        use indoc::indoc;

        let package_json = indoc! {r#"
            {
                "name": "test-package",
                "dependencies": {
                    "lodash": "^4.17.21"
                },
                "devDependencies": {
                    "jest": "^29.0.0"
                }
            }
        "#};

        let deps = json_to_dependencies(package_json.as_bytes(), false, "test").unwrap();

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "lodash");
    }

    #[test]
    fn test_json_to_dependencies_with_optional() {
        use indoc::indoc;

        let package_json = indoc! {r#"
            {
                "name": "test-package",
                "dependencies": {
                    "lodash": "^4.17.21"
                },
                "optionalDependencies": {
                    "fsevents": "^2.3.0"
                }
            }
        "#};

        let deps = json_to_dependencies(package_json.as_bytes(), false, "test").unwrap();

        assert_eq!(deps.len(), 2);
        let fsevents = deps.iter().find(|d| d.name == "fsevents").unwrap();
        assert!(fsevents.is_optional);
    }

    // ========== compute_optimal_hoisting edge case tests ==========

    #[test]
    fn test_hoisting_empty_requirements() {
        let requirements: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let resolved_versions: BTreeMap<(String, String), String> = BTreeMap::new();

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        assert!(optimal.is_empty());
    }

    #[test]
    fn test_hoisting_three_conflicting_versions() {
        // Three different major versions - pick the one satisfying the most ranges
        let mut requirements = BTreeMap::new();
        requirements.insert(
            "semver".to_string(),
            vec![
                "^5.0.0".to_string(), // satisfied by 5.x
                "^6.0.0".to_string(), // satisfied by 6.x
                "^6.3.0".to_string(), // satisfied by 6.x (and 6.3.1 specifically)
                "^7.0.0".to_string(), // satisfied by 7.x
            ],
        );

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("semver".to_string(), "^5.0.0".to_string()),
            "5.7.2".to_string(),
        );
        resolved_versions.insert(
            ("semver".to_string(), "^6.0.0".to_string()),
            "6.3.1".to_string(),
        );
        resolved_versions.insert(
            ("semver".to_string(), "^6.3.0".to_string()),
            "6.3.1".to_string(),
        );
        resolved_versions.insert(
            ("semver".to_string(), "^7.0.0".to_string()),
            "7.6.0".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        // 6.3.1 satisfies ^6.0.0 and ^6.3.0 (2 ranges), others satisfy 1 each
        assert_eq!(optimal.get("semver"), Some(&"6.3.1".to_string()));
    }

    #[test]
    fn test_hoisting_all_ranges_one_version() {
        // All ranges can be satisfied by the same version
        let mut requirements = BTreeMap::new();
        requirements.insert(
            "lodash".to_string(),
            vec![
                "^4.0.0".to_string(),
                "^4.10.0".to_string(),
                "^4.17.0".to_string(),
            ],
        );

        let mut resolved_versions = BTreeMap::new();
        // All resolve to 4.17.21
        resolved_versions.insert(
            ("lodash".to_string(), "^4.0.0".to_string()),
            "4.17.21".to_string(),
        );
        resolved_versions.insert(
            ("lodash".to_string(), "^4.10.0".to_string()),
            "4.17.21".to_string(),
        );
        resolved_versions.insert(
            ("lodash".to_string(), "^4.17.0".to_string()),
            "4.17.21".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        assert_eq!(optimal.get("lodash"), Some(&"4.17.21".to_string()));
    }

    #[test]
    fn test_hoisting_no_resolved_versions_for_package() {
        // Requirements exist but no resolved versions
        let mut requirements = BTreeMap::new();
        requirements.insert("ghost".to_string(), vec!["^1.0.0".to_string()]);

        let resolved_versions: BTreeMap<(String, String), String> = BTreeMap::new();

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        // Should have no entry for ghost
        assert!(optimal.get("ghost").is_none());
    }

    #[test]
    fn test_hoisting_multiple_packages() {
        // Multiple packages each with their own hoisting decision
        let mut requirements = BTreeMap::new();
        requirements.insert("lodash".to_string(), vec!["^4.0.0".to_string()]);
        requirements.insert(
            "express".to_string(),
            vec!["^4.0.0".to_string(), "^4.17.0".to_string()],
        );
        requirements.insert(
            "react".to_string(),
            vec!["^17.0.0".to_string(), "^18.0.0".to_string()],
        );

        let mut resolved_versions = BTreeMap::new();
        resolved_versions.insert(
            ("lodash".to_string(), "^4.0.0".to_string()),
            "4.17.21".to_string(),
        );
        resolved_versions.insert(
            ("express".to_string(), "^4.0.0".to_string()),
            "4.18.2".to_string(),
        );
        resolved_versions.insert(
            ("express".to_string(), "^4.17.0".to_string()),
            "4.18.2".to_string(),
        );
        resolved_versions.insert(
            ("react".to_string(), "^17.0.0".to_string()),
            "17.0.2".to_string(),
        );
        resolved_versions.insert(
            ("react".to_string(), "^18.0.0".to_string()),
            "18.2.0".to_string(),
        );

        let optimal = compute_optimal_hoisting(&requirements, &resolved_versions);

        assert_eq!(optimal.get("lodash"), Some(&"4.17.21".to_string()));
        assert_eq!(optimal.get("express"), Some(&"4.18.2".to_string()));
        // React: both versions satisfy 1 range each, 18.2.0 is newer so it wins
        assert_eq!(optimal.get("react"), Some(&"18.2.0".to_string()));
    }

    // ========== dedupe move detection tests ==========

    fn make_resolved_info(path: &str) -> ResolvedInfo {
        ResolvedInfo {
            tarball_url: None,
            integrity: None,
            dependencies: vec![],
            install_path: path.to_string(),
            deprecated: None,
            maturity_fallback: None,
        }
    }

    /// Compute moves needed for deduplication (extracted logic for testing)
    fn compute_dedupe_moves(
        old_deps: &IndexMap<Dependency, ResolvedInfo>,
        new_deps: &IndexMap<Dependency, ResolvedInfo>,
    ) -> Vec<(String, String, String)> {
        let mut moves = Vec::new();
        for (dep, new_info) in new_deps {
            if let Some(old_info) = old_deps.get(dep) {
                if old_info.install_path != new_info.install_path {
                    moves.push((
                        dep.name.clone(),
                        old_info.install_path.clone(),
                        new_info.install_path.clone(),
                    ));
                }
            }
        }
        moves
    }

    #[test]
    fn test_dedupe_no_moves_needed() {
        let mut old = IndexMap::new();
        let mut new = IndexMap::new();

        let lodash = make_dep("lodash", "^4.0.0", "4.17.21");
        old.insert(lodash.clone(), make_resolved_info("node_modules/lodash"));
        new.insert(lodash.clone(), make_resolved_info("node_modules/lodash"));

        let moves = compute_dedupe_moves(&old, &new);
        assert!(moves.is_empty());
    }

    #[test]
    fn test_dedupe_hoist_up() {
        // Package moves from nested to root (hoisted up)
        let mut old = IndexMap::new();
        let mut new = IndexMap::new();

        let qs = make_dep("qs", "^6.0.0", "6.11.0");
        old.insert(
            qs.clone(),
            make_resolved_info("node_modules/express/node_modules/qs"),
        );
        new.insert(qs.clone(), make_resolved_info("node_modules/qs"));

        let moves = compute_dedupe_moves(&old, &new);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "qs");
        assert_eq!(moves[0].1, "node_modules/express/node_modules/qs");
        assert_eq!(moves[0].2, "node_modules/qs");
    }

    #[test]
    fn test_dedupe_push_down() {
        // Package moves from root to nested (pushed down due to conflict)
        let mut old = IndexMap::new();
        let mut new = IndexMap::new();

        let qs = make_dep("qs", "^5.0.0", "5.2.1");
        old.insert(qs.clone(), make_resolved_info("node_modules/qs"));
        new.insert(
            qs.clone(),
            make_resolved_info("node_modules/body-parser/node_modules/qs"),
        );

        let moves = compute_dedupe_moves(&old, &new);
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].0, "qs");
        assert_eq!(moves[0].1, "node_modules/qs");
        assert_eq!(moves[0].2, "node_modules/body-parser/node_modules/qs");
    }

    #[test]
    fn test_dedupe_mixed_moves() {
        let mut old = IndexMap::new();
        let mut new = IndexMap::new();

        // Package stays in place
        let lodash = make_dep("lodash", "^4.0.0", "4.17.21");
        old.insert(lodash.clone(), make_resolved_info("node_modules/lodash"));
        new.insert(lodash.clone(), make_resolved_info("node_modules/lodash"));

        // Package hoists up
        let qs = make_dep("qs", "^6.0.0", "6.11.0");
        old.insert(
            qs.clone(),
            make_resolved_info("node_modules/express/node_modules/qs"),
        );
        new.insert(qs.clone(), make_resolved_info("node_modules/qs"));

        // Package pushes down
        let debug = make_dep("debug", "^2.0.0", "2.6.9");
        old.insert(debug.clone(), make_resolved_info("node_modules/debug"));
        new.insert(
            debug.clone(),
            make_resolved_info("node_modules/express/node_modules/debug"),
        );

        let moves = compute_dedupe_moves(&old, &new);
        assert_eq!(moves.len(), 2);

        let move_names: Vec<&str> = moves.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(move_names.contains(&"qs"));
        assert!(move_names.contains(&"debug"));
        assert!(!move_names.contains(&"lodash"));
    }

    #[test]
    fn test_dedupe_new_package_not_in_old() {
        // New package that wasn't in old deps - should not appear in moves
        let old = IndexMap::new();
        let mut new = IndexMap::new();

        let lodash = make_dep("lodash", "^4.0.0", "4.17.21");
        new.insert(lodash.clone(), make_resolved_info("node_modules/lodash"));

        let moves = compute_dedupe_moves(&old, &new);
        assert!(moves.is_empty());
    }

    // ========== find_dupes logic tests ==========

    /// Find duplicate packages from a lockfile packages map (extracted logic)
    fn find_duplicates(
        packages: &std::collections::HashMap<String, (String, String)>, // path -> (name, version)
    ) -> std::collections::HashMap<String, Vec<(String, String)>> {
        // Build map: package_name -> [(version, path), ...]
        let mut by_name: std::collections::HashMap<String, Vec<(String, String)>> =
            std::collections::HashMap::new();

        for (path, (name, version)) in packages {
            by_name
                .entry(name.clone())
                .or_default()
                .push((version.clone(), path.clone()));
        }

        // Filter to only packages with duplicates
        by_name
            .into_iter()
            .filter(|(_, locs)| locs.len() > 1)
            .collect()
    }

    #[test]
    fn test_find_dupes_none() {
        let mut packages = std::collections::HashMap::new();
        packages.insert(
            "node_modules/lodash".to_string(),
            ("lodash".to_string(), "4.17.21".to_string()),
        );
        packages.insert(
            "node_modules/express".to_string(),
            ("express".to_string(), "4.18.2".to_string()),
        );

        let dupes = find_duplicates(&packages);
        assert!(dupes.is_empty());
    }

    #[test]
    fn test_find_dupes_one_package() {
        let mut packages = std::collections::HashMap::new();
        packages.insert(
            "node_modules/qs".to_string(),
            ("qs".to_string(), "6.12.0".to_string()),
        );
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            ("qs".to_string(), "6.11.0".to_string()),
        );
        packages.insert(
            "node_modules/lodash".to_string(),
            ("lodash".to_string(), "4.17.21".to_string()),
        );

        let dupes = find_duplicates(&packages);
        assert_eq!(dupes.len(), 1);
        assert!(dupes.contains_key("qs"));
        assert_eq!(dupes.get("qs").unwrap().len(), 2);
    }

    #[test]
    fn test_find_dupes_multiple_packages() {
        let mut packages = std::collections::HashMap::new();
        // qs appears twice
        packages.insert(
            "node_modules/qs".to_string(),
            ("qs".to_string(), "6.12.0".to_string()),
        );
        packages.insert(
            "node_modules/express/node_modules/qs".to_string(),
            ("qs".to_string(), "6.11.0".to_string()),
        );
        // debug appears three times
        packages.insert(
            "node_modules/debug".to_string(),
            ("debug".to_string(), "4.3.4".to_string()),
        );
        packages.insert(
            "node_modules/express/node_modules/debug".to_string(),
            ("debug".to_string(), "2.6.9".to_string()),
        );
        packages.insert(
            "node_modules/morgan/node_modules/debug".to_string(),
            ("debug".to_string(), "3.1.0".to_string()),
        );
        // lodash appears once (not a dupe)
        packages.insert(
            "node_modules/lodash".to_string(),
            ("lodash".to_string(), "4.17.21".to_string()),
        );

        let dupes = find_duplicates(&packages);
        assert_eq!(dupes.len(), 2);
        assert!(dupes.contains_key("qs"));
        assert!(dupes.contains_key("debug"));
        assert!(!dupes.contains_key("lodash"));
        assert_eq!(dupes.get("debug").unwrap().len(), 3);
    }

    #[test]
    fn test_find_dupes_same_version_different_paths() {
        // Same version in multiple locations counts as duplicate
        let mut packages = std::collections::HashMap::new();
        packages.insert(
            "node_modules/ms".to_string(),
            ("ms".to_string(), "2.1.2".to_string()),
        );
        packages.insert(
            "node_modules/debug/node_modules/ms".to_string(),
            ("ms".to_string(), "2.1.2".to_string()),
        );

        let dupes = find_duplicates(&packages);
        assert_eq!(dupes.len(), 1);
        assert!(dupes.contains_key("ms"));
    }

    // ========== prune set logic tests ==========

    #[test]
    fn test_prune_no_extraneous() {
        use std::collections::HashSet;

        let expected: HashSet<String> = ["lodash", "express", "debug"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found: HashSet<String> = ["lodash", "express", "debug"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let extraneous: Vec<_> = found.difference(&expected).collect();
        assert!(extraneous.is_empty());
    }

    #[test]
    fn test_prune_some_extraneous() {
        use std::collections::HashSet;

        let expected: HashSet<String> = ["lodash", "express"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let found: HashSet<String> = ["lodash", "express", "leftover", "old-pkg"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut extraneous: Vec<_> = found.difference(&expected).cloned().collect();
        extraneous.sort();

        assert_eq!(extraneous.len(), 2);
        assert_eq!(extraneous, vec!["leftover", "old-pkg"]);
    }

    #[test]
    fn test_prune_all_extraneous() {
        use std::collections::HashSet;

        let expected: HashSet<String> = HashSet::new();
        let found: HashSet<String> = ["stale1", "stale2"].iter().map(|s| s.to_string()).collect();

        let extraneous: Vec<_> = found.difference(&expected).collect();
        assert_eq!(extraneous.len(), 2);
    }

    #[test]
    fn test_prune_nested_paths() {
        use std::collections::HashSet;

        // Test that nested paths work correctly
        let expected: HashSet<String> = ["express", "express/node_modules/qs", "lodash"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let found: HashSet<String> = [
            "express",
            "express/node_modules/qs",
            "express/node_modules/old-dep", // extraneous nested
            "lodash",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let extraneous: Vec<_> = found.difference(&expected).collect();
        assert_eq!(extraneous.len(), 1);
        assert_eq!(extraneous[0], "express/node_modules/old-dep");
    }

    // ========== peer dependency tests ==========

    #[test]
    fn test_parse_peer_deps_no_meta() {
        // peerDependencies without peerDependenciesMeta - all required
        let pkg_json = serde_json::json!({
            "peerDependencies": {
                "react": "^18.0.0",
                "react-dom": "^18.0.0"
            }
        });

        let peers = parse_peer_dependencies("my-component", &pkg_json);
        assert_eq!(peers.len(), 2);

        let react = peers.iter().find(|p| p.peer_name == "react").unwrap();
        assert_eq!(react.package, "my-component");
        assert_eq!(react.peer_range, "^18.0.0");
        assert!(!react.optional);

        let react_dom = peers.iter().find(|p| p.peer_name == "react-dom").unwrap();
        assert!(!react_dom.optional);
    }

    #[test]
    fn test_parse_peer_deps_with_optional_meta() {
        // peerDependencies with peerDependenciesMeta marking some as optional
        let pkg_json = serde_json::json!({
            "peerDependencies": {
                "react": "^18.0.0",
                "dayjs": "^1.0.0",
                "moment": "^2.0.0",
                "date-fns": "^2.0.0"
            },
            "peerDependenciesMeta": {
                "dayjs": { "optional": true },
                "moment": { "optional": true },
                "date-fns": { "optional": true }
            }
        });

        let peers = parse_peer_dependencies("@mui/x-date-pickers", &pkg_json);
        assert_eq!(peers.len(), 4);

        // react is required (not in meta)
        let react = peers.iter().find(|p| p.peer_name == "react").unwrap();
        assert!(!react.optional);

        // date libraries are optional
        let dayjs = peers.iter().find(|p| p.peer_name == "dayjs").unwrap();
        assert!(dayjs.optional);

        let moment = peers.iter().find(|p| p.peer_name == "moment").unwrap();
        assert!(moment.optional);

        let date_fns = peers.iter().find(|p| p.peer_name == "date-fns").unwrap();
        assert!(date_fns.optional);
    }

    #[test]
    fn test_parse_peer_deps_meta_false() {
        // peerDependenciesMeta with optional: false (explicit)
        let pkg_json = serde_json::json!({
            "peerDependencies": {
                "react": "^18.0.0"
            },
            "peerDependenciesMeta": {
                "react": { "optional": false }
            }
        });

        let peers = parse_peer_dependencies("test-pkg", &pkg_json);
        assert_eq!(peers.len(), 1);
        assert!(!peers[0].optional);
    }

    #[test]
    fn test_parse_peer_deps_empty() {
        // No peerDependencies field
        let pkg_json = serde_json::json!({
            "name": "test-pkg",
            "version": "1.0.0"
        });

        let peers = parse_peer_dependencies("test-pkg", &pkg_json);
        assert!(peers.is_empty());
    }

    #[test]
    fn test_check_peer_deps_all_satisfied() {
        // All peer deps installed with correct versions - no warnings
        let peer_deps = vec![PeerDependency {
            package: "my-lib".to_string(),
            peer_name: "react".to_string(),
            peer_range: "^18.0.0".to_string(),
            optional: false,
        }];

        let mut resolved = IndexMap::new();
        resolved.insert(
            make_dep("react", "^18.0.0", "18.2.0"),
            make_resolved_info("node_modules/react"),
        );

        // This should not print any warnings (we can't easily capture stderr in tests,
        // but at least verify it doesn't panic)
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_optional_missing_no_warn() {
        // Optional peer dep not installed - should NOT warn
        let peer_deps = vec![PeerDependency {
            package: "vite".to_string(),
            peer_name: "sass".to_string(),
            peer_range: "*".to_string(),
            optional: true,
        }];

        let resolved = IndexMap::new(); // Empty - sass not installed

        // This should not print any warnings for optional missing peer
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_required_missing_warns() {
        // Required peer dep not installed - should warn (can't capture stderr easily,
        // but this documents the expected behavior)
        let peer_deps = vec![PeerDependency {
            package: "react-router".to_string(),
            peer_name: "react".to_string(),
            peer_range: "^18.0.0".to_string(),
            optional: false,
        }];

        let resolved = IndexMap::new(); // Empty - react not installed

        // In real usage this prints: "warn: react-router requires peer react ^18.0.0 which is not installed"
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_version_mismatch_required() {
        // Required peer installed but wrong version - should warn
        let peer_deps = vec![PeerDependency {
            package: "react-router".to_string(),
            peer_name: "react".to_string(),
            peer_range: "^18.0.0".to_string(),
            optional: false,
        }];

        let mut resolved = IndexMap::new();
        resolved.insert(
            make_dep("react", "^17.0.0", "17.0.2"), // Wrong version
            make_resolved_info("node_modules/react"),
        );

        // Should print: "warn: react-router requires peer react ^18.0.0 but found 17.0.2"
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_version_mismatch_optional() {
        // Optional peer installed but wrong version - should STILL warn
        // (we only skip warning for MISSING optional peers, not mismatched ones)
        let peer_deps = vec![PeerDependency {
            package: "@mui/x-date-pickers".to_string(),
            peer_name: "dayjs".to_string(),
            peer_range: "^1.10.0".to_string(),
            optional: true,
        }];

        let mut resolved = IndexMap::new();
        resolved.insert(
            make_dep("dayjs", "^1.0.0", "1.8.0"), // Installed but doesn't satisfy ^1.10.0
            make_resolved_info("node_modules/dayjs"),
        );

        // Should print warning about version mismatch even though peer is optional
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_invalid_range_skips() {
        // Invalid semver range - should not panic, just skip
        let peer_deps = vec![PeerDependency {
            package: "weird-pkg".to_string(),
            peer_name: "react".to_string(),
            peer_range: "not-a-valid-range!!!".to_string(),
            optional: false,
        }];

        let mut resolved = IndexMap::new();
        resolved.insert(
            make_dep("react", "^18.0.0", "18.2.0"),
            make_resolved_info("node_modules/react"),
        );

        // Should not panic - invalid range is silently skipped
        check_peer_dependencies(&peer_deps, &resolved);
    }

    #[test]
    fn test_check_peer_deps_mixed_optional_required() {
        // Mix of optional and required peers
        let peer_deps = vec![
            PeerDependency {
                package: "my-lib".to_string(),
                peer_name: "react".to_string(),
                peer_range: "^18.0.0".to_string(),
                optional: false, // Required
            },
            PeerDependency {
                package: "my-lib".to_string(),
                peer_name: "typescript".to_string(),
                peer_range: "^5.0.0".to_string(),
                optional: true, // Optional
            },
        ];

        let mut resolved = IndexMap::new();
        resolved.insert(
            make_dep("react", "^18.0.0", "18.2.0"),
            make_resolved_info("node_modules/react"),
        );
        // typescript not installed - but it's optional so no warning

        check_peer_dependencies(&peer_deps, &resolved);
    }

    // === Offline Mode Tests ===

    use crate::cache::{save_root_metadata, save_version_metadata};
    use serial_test::serial;
    use tempfile::TempDir;

    #[tokio::test]
    #[serial]
    async fn test_metadata_cache_offline_uses_filesystem_cache() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        // Pre-populate the cache with metadata
        let metadata = serde_json::json!({
            "name": "offline-test-pkg",
            "versions": {
                "1.0.0": {
                    "name": "offline-test-pkg",
                    "version": "1.0.0"
                }
            }
        });
        save_root_metadata("offline-test-pkg", &metadata, Some("etag123")).unwrap();

        // Create offline cache
        let client = reqwest::Client::new();
        let cache = MetadataCache::with_options(client, RegistryConfig::default(), true);

        // Should successfully get metadata from cache
        let dep = Dependency {
            name: "offline-test-pkg".to_string(),
            requested: "^1.0.0".to_string(),
            resolved: String::new(),
            is_optional: false,
            alias: None,
        };

        let result = cache.get_root_metadata(&dep).await;
        assert!(result.is_ok());
        let metadata = result.unwrap();
        assert_eq!(metadata["name"], "offline-test-pkg");
    }

    #[tokio::test]
    #[serial]
    async fn test_metadata_cache_offline_fails_when_not_cached() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        // Create offline cache (no pre-populated data)
        let client = reqwest::Client::new();
        let cache = MetadataCache::with_options(client, RegistryConfig::default(), true);

        let dep = Dependency {
            name: "not-in-cache-pkg".to_string(),
            requested: "^1.0.0".to_string(),
            resolved: String::new(),
            is_optional: false,
            alias: None,
        };

        let result = cache.get_root_metadata(&dep).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found in cache"));
        assert!(err.contains("offline"));
    }

    #[tokio::test]
    #[serial]
    async fn test_metadata_cache_offline_version_from_cache() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        // Pre-populate version metadata
        let metadata = serde_json::json!({
            "name": "offline-ver-pkg",
            "version": "2.0.0",
            "dist": {
                "tarball": "https://example.com/pkg.tgz"
            }
        });
        save_version_metadata("offline-ver-pkg", "2.0.0", &metadata).unwrap();

        // Create offline cache
        let client = reqwest::Client::new();
        let cache = MetadataCache::with_options(client, RegistryConfig::default(), true);

        let dep = Dependency {
            name: "offline-ver-pkg".to_string(),
            requested: "^2.0.0".to_string(),
            resolved: "2.0.0".to_string(),
            is_optional: false,
            alias: None,
        };

        let result = cache.get_version_metadata(&dep).await;
        assert!(result.is_ok());
        let metadata = result.unwrap();
        assert_eq!(metadata["version"], "2.0.0");
    }

    #[tokio::test]
    #[serial]
    async fn test_metadata_cache_offline_version_fails_when_not_cached() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        // Create offline cache (no version metadata)
        let client = reqwest::Client::new();
        let cache = MetadataCache::with_options(client, RegistryConfig::default(), true);

        let dep = Dependency {
            name: "missing-ver-pkg".to_string(),
            requested: "^1.0.0".to_string(),
            resolved: "1.0.0".to_string(),
            is_optional: false,
            alias: None,
        };

        let result = cache.get_version_metadata(&dep).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found in cache"));
        assert!(err.contains("offline"));
    }
}
