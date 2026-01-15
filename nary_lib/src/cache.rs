use base64::{engine::general_purpose::STANDARD, Engine};
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};
use snafu::ResultExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use std::{
    fs,
    fs::create_dir_all,
    path::{Path, PathBuf},
};

use crate::error::{
    CacheDirSnafu, DirCreateSnafu, FileReadSnafu, FileWriteSnafu, HttpRequestSnafu,
    HttpResponseSnafu, IntegrityMismatchSnafu, InvalidIntegritySnafu, JsonSerializeSnafu, Result,
};

/// Cached metadata with optional ETag for conditional requests
pub struct CachedMetadata {
    pub data: Value,
    pub etag: Option<String>,
}

pub fn get_cache_dir() -> Result<PathBuf> {
    let mut cache_dir = dirs::home_dir().ok_or_else(|| CacheDirSnafu.build())?;

    cache_dir.push(".nary_cache");
    create_dir_all(&cache_dir).context(DirCreateSnafu {
        path: cache_dir.clone(),
    })?;

    Ok(cache_dir)
}

/// Clear the entire cache directory, returning the number of bytes freed
pub fn clear_cache() -> Result<u64> {
    let cache_dir = get_cache_dir()?;
    let size = dir_size(&cache_dir);
    fs::remove_dir_all(&cache_dir).ok(); // Ignore error if doesn't exist
    create_dir_all(&cache_dir).context(DirCreateSnafu {
        path: cache_dir.clone(),
    })?;
    Ok(size)
}

/// Calculate total size of a directory recursively (in bytes).
pub fn dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_dir() {
                size += dir_size(&entry_path);
            } else if let Ok(meta) = entry.metadata() {
                size += meta.len();
            }
        }
    }
    size
}

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// https://url.spec.whatwg.org/#path-percent-encode-set
pub const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'#')
    .add(b'?')
    .add(b'{')
    .add(b'}');

/// Cache the given package tarball, returning the (gzipped) bytes.
/// Uses local cache if available, otherwise downloads from tarball_url.
/// If integrity is provided, verifies the downloaded tarball's SHA-512 hash.
pub async fn cache_tarball(
    client: &Client,
    key: &str,
    version: &str,
    tarball_url: &str,
    integrity: Option<&str>,
) -> Result<Vec<u8>> {
    let mut path = get_cache_dir()?;
    // Handle scoped packages like @socket.io/adapter - they have / in the name
    path.push(utf8_percent_encode(key, PATH_SEGMENT_ENCODE_SET).to_string());
    path.push(version);
    // Safe to ignore: dir may already exist, and we'll error on file write if there's a real problem
    let _ = tokio::fs::create_dir_all(&path).await;
    path.push("package.tgz");

    // Check if already cached (assume already verified)
    if let Ok(mut cache_file) = tokio::fs::File::open(&path).await {
        let mut tarball_res = Vec::new();
        cache_file
            .read_to_end(&mut tarball_res)
            .await
            .context(FileReadSnafu { path: path.clone() })?;
        return Ok(tarball_res);
    }

    // Download from network with streaming hash computation
    let response = client
        .get(tarball_url)
        .send()
        .await
        .context(HttpRequestSnafu {
            url: tarball_url.to_string(),
        })?;

    // Stream download while computing hash incrementally
    let mut stream = response.bytes_stream();
    let mut hasher = Sha512::new();
    let mut tarball_res = Vec::with_capacity(1024 * 1024); // Pre-allocate 1MB

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.context(HttpResponseSnafu {
            url: tarball_url.to_string(),
        })?;
        hasher.update(&chunk);
        tarball_res.extend_from_slice(&chunk);
    }

    // Verify integrity if provided (using pre-computed hash)
    if let Some(integrity_str) = integrity {
        let hash_part = integrity_str
            .split_whitespace()
            .find(|h| h.starts_with("sha512-"))
            .ok_or_else(|| {
                InvalidIntegritySnafu {
                    integrity: integrity_str.to_string(),
                }
                .build()
            })?;

        let actual = format!("sha512-{}", STANDARD.encode(hasher.finalize()));
        if actual != hash_part {
            return IntegrityMismatchSnafu {
                package: key.to_string(),
                version: version.to_string(),
                expected: hash_part.to_string(),
                actual,
            }
            .fail();
        }
    }

    // Save to cache (only after integrity check passes)
    let mut cache_file = tokio::fs::File::create(&path)
        .await
        .context(FileWriteSnafu { path: path.clone() })?;
    cache_file
        .write_all(&tarball_res)
        .await
        .context(FileWriteSnafu { path: path.clone() })?;

    Ok(tarball_res)
}

/// Get the cache path for a package (creating directories if needed)
fn get_package_cache_path(package: &str) -> Result<PathBuf> {
    let mut path = get_cache_dir()?;
    // Handle scoped packages like @socket.io/adapter - they have / in the name
    path.push(utf8_percent_encode(package, PATH_SEGMENT_ENCODE_SET).to_string());
    // Safe to ignore: dir may already exist, and we'll error on file write if there's a real problem
    let _ = fs::create_dir_all(&path);
    Ok(path)
}

/// Save content with its SHA-256 hash to a companion file
fn save_with_hash(json_path: &PathBuf, hash_path: &PathBuf, content: &str) -> Result<()> {
    let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
    fs::write(json_path, content).context(FileWriteSnafu {
        path: json_path.clone(),
    })?;
    fs::write(hash_path, &hash).context(FileWriteSnafu {
        path: hash_path.clone(),
    })?;
    Ok(())
}

/// Load content and verify against SHA-256 hash file.
/// Returns None if hash mismatch or file not found.
fn load_with_hash_verification(json_path: &PathBuf, hash_path: &PathBuf) -> Option<String> {
    let content = fs::read_to_string(json_path).ok()?;
    if let Ok(expected_hash) = fs::read_to_string(hash_path) {
        let actual_hash = format!("{:x}", Sha256::digest(content.as_bytes()));
        if actual_hash != expected_hash.trim() {
            return None;
        }
    }
    Some(content)
}

/// Save root metadata with its ETag and integrity hash
pub fn save_root_metadata(package: &str, metadata: &Value, etag: Option<&str>) -> Result<()> {
    let path = get_package_cache_path(package)?;
    let json_content = serde_json::to_string(metadata).context(JsonSerializeSnafu)?;

    save_with_hash(
        &path.join("root.json"),
        &path.join("root.sha256"),
        &json_content,
    )?;

    if let Some(etag) = etag {
        let etag_path = path.join("root.etag");
        fs::write(&etag_path, etag).context(FileWriteSnafu { path: etag_path })?;
    }

    Ok(())
}

/// Load cached root metadata and ETag (if exists), verifying integrity
pub fn load_root_metadata(package: &str) -> Option<CachedMetadata> {
    let path = get_package_cache_path(package).ok()?;
    let content = load_with_hash_verification(&path.join("root.json"), &path.join("root.sha256"))?;
    let data: Value = serde_json::from_str(&content).ok()?;
    let etag = fs::read_to_string(path.join("root.etag")).ok();

    Some(CachedMetadata { data, etag })
}

/// Save version metadata with integrity hash (immutable - no ETag needed)
pub fn save_version_metadata(package: &str, version: &str, metadata: &Value) -> Result<()> {
    let mut path = get_package_cache_path(package)?;
    path.push(version);
    // Safe to ignore: dir may already exist, and we'll error on file write if there's a real problem
    let _ = fs::create_dir(&path);

    let json_content = serde_json::to_string(metadata).context(JsonSerializeSnafu)?;
    save_with_hash(
        &path.join("metadata.json"),
        &path.join("metadata.sha256"),
        &json_content,
    )
}

/// Load cached version metadata, verifying integrity
pub fn load_version_metadata(package: &str, version: &str) -> Option<Value> {
    let mut path = get_package_cache_path(package).ok()?;
    path.push(version);
    let content =
        load_with_hash_verification(&path.join("metadata.json"), &path.join("metadata.sha256"))?;
    serde_json::from_str(&content).ok()
}

// ============================================================================
// Async versions of cache functions (non-blocking for async runtimes)
// ============================================================================

/// Async version of load_root_metadata - runs blocking I/O on spawn_blocking
pub async fn load_root_metadata_async(package: &str) -> Option<CachedMetadata> {
    let package = package.to_string();
    tokio::task::spawn_blocking(move || load_root_metadata(&package))
        .await
        .ok()
        .flatten()
}

/// Async version of save_root_metadata - runs blocking I/O on spawn_blocking
pub async fn save_root_metadata_async(package: &str, metadata: &Value, etag: Option<&str>) {
    let package = package.to_string();
    let metadata = metadata.clone();
    let etag = etag.map(|s| s.to_string());
    let _ = tokio::task::spawn_blocking(move || {
        save_root_metadata(&package, &metadata, etag.as_deref())
    })
    .await;
}

/// Async version of load_version_metadata - runs blocking I/O on spawn_blocking
pub async fn load_version_metadata_async(package: &str, version: &str) -> Option<Value> {
    let package = package.to_string();
    let version = version.to_string();
    tokio::task::spawn_blocking(move || load_version_metadata(&package, &version))
        .await
        .ok()
        .flatten()
}

/// Async version of save_version_metadata - runs blocking I/O on spawn_blocking
pub async fn save_version_metadata_async(package: &str, version: &str, metadata: &Value) {
    let package = package.to_string();
    let version = version.to_string();
    let metadata = metadata.clone();
    let _ =
        tokio::task::spawn_blocking(move || save_version_metadata(&package, &version, &metadata))
            .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serial_test::serial;
    use sha2::{Digest, Sha512};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn compute_sha512(data: &[u8]) -> String {
        let mut hasher = Sha512::new();
        hasher.update(data);
        format!("sha512-{}", STANDARD.encode(hasher.finalize()))
    }

    #[test]
    fn test_dir_size_empty() {
        let temp = TempDir::new().unwrap();
        let size = dir_size(&temp.path().to_path_buf());
        assert_eq!(size, 0);
    }

    #[test]
    fn test_dir_size_with_files() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "hello").unwrap(); // 5 bytes
        std::fs::write(temp.path().join("b.txt"), "world!").unwrap(); // 6 bytes

        let size = dir_size(&temp.path().to_path_buf());
        assert_eq!(size, 11);
    }

    #[test]
    fn test_dir_size_nested() {
        let temp = TempDir::new().unwrap();
        let sub = temp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(temp.path().join("a.txt"), "aaa").unwrap(); // 3 bytes
        std::fs::write(sub.join("b.txt"), "bbbbb").unwrap(); // 5 bytes

        let size = dir_size(&temp.path().to_path_buf());
        assert_eq!(size, 8);
    }

    #[test]
    #[serial]
    fn test_save_load_root_metadata_roundtrip() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({
            "name": "test-pkg",
            "versions": {"1.0.0": {}}
        });

        save_root_metadata("test-pkg", &metadata, Some("W/\"abc123\"")).unwrap();

        let loaded = load_root_metadata("test-pkg").unwrap();
        assert_eq!(loaded.data["name"], "test-pkg");
        assert_eq!(loaded.etag, Some("W/\"abc123\"".to_string()));
    }

    #[test]
    #[serial]
    fn test_save_load_root_metadata_no_etag() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({"name": "pkg"});
        save_root_metadata("pkg", &metadata, None).unwrap();

        let loaded = load_root_metadata("pkg").unwrap();
        assert!(loaded.etag.is_none());
    }

    #[test]
    #[serial]
    fn test_save_load_version_metadata_roundtrip() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({
            "name": "lodash",
            "version": "4.17.21",
            "dist": {"tarball": "https://example.com/lodash.tgz"}
        });

        save_version_metadata("lodash", "4.17.21", &metadata).unwrap();

        let loaded = load_version_metadata("lodash", "4.17.21").unwrap();
        assert_eq!(loaded["version"], "4.17.21");
    }

    #[test]
    #[serial]
    fn test_root_metadata_integrity_check_detects_corruption() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({
            "name": "test-pkg",
            "versions": {"1.0.0": {}}
        });

        save_root_metadata("test-pkg", &metadata, Some("etag123")).unwrap();

        // Corrupt the cached file
        let cache_path = temp.path().join(".nary_cache/test-pkg/root.json");
        std::fs::write(&cache_path, r#"{"name": "corrupted"}"#).unwrap();

        // Load should return None due to hash mismatch
        let result = load_root_metadata("test-pkg");
        assert!(result.is_none(), "corrupted cache should return None");
    }

    #[test]
    #[serial]
    fn test_version_metadata_integrity_check_detects_corruption() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({
            "name": "lodash",
            "version": "4.17.21"
        });

        save_version_metadata("lodash", "4.17.21", &metadata).unwrap();

        // Corrupt the cached file
        let cache_path = temp.path().join(".nary_cache/lodash/4.17.21/metadata.json");
        std::fs::write(&cache_path, r#"{"corrupted": true}"#).unwrap();

        // Load should return None due to hash mismatch
        let result = load_version_metadata("lodash", "4.17.21");
        assert!(result.is_none(), "corrupted cache should return None");
    }

    #[test]
    #[serial]
    fn test_load_nonexistent_returns_none() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let result = load_root_metadata("nonexistent-package");
        assert!(result.is_none());

        let result = load_version_metadata("nonexistent", "1.0.0");
        assert!(result.is_none());
    }

    #[test]
    #[serial]
    fn test_scoped_package_encoding() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        let metadata = serde_json::json!({"name": "@scope/pkg"});
        save_root_metadata("@scope/pkg", &metadata, None).unwrap();

        let loaded = load_root_metadata("@scope/pkg").unwrap();
        assert_eq!(loaded.data["name"], "@scope/pkg");
    }

    #[test]
    #[serial]
    fn test_clear_cache_returns_size() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());

        // Create cache with some data
        let metadata = serde_json::json!({"name": "pkg"});
        save_root_metadata("pkg1", &metadata, None).unwrap();
        save_root_metadata("pkg2", &metadata, None).unwrap();

        let freed = clear_cache().unwrap();
        // Should have freed some bytes (exact amount depends on JSON formatting)
        assert!(freed > 0);

        // Cache should be empty now
        assert!(load_root_metadata("pkg1").is_none());
    }

    #[tokio::test]
    #[serial]
    async fn test_cache_tarball_downloads_on_miss() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        // Pre-create the cache directory structure
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        let server = MockServer::start().await;
        let tarball_content = b"fake-tarball-content";

        Mock::given(method("GET"))
            .and(path("/pkg/-/pkg-1.0.0.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball_content.to_vec()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/pkg/-/pkg-1.0.0.tgz", server.uri());

        let result = cache_tarball(&client, "pkg", "1.0.0", &url, None)
            .await
            .unwrap();

        assert_eq!(result, tarball_content);
    }

    #[tokio::test]
    #[serial]
    async fn test_cache_tarball_returns_cached() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        let server = MockServer::start().await;
        let tarball_content = b"cached-content";

        Mock::given(method("GET"))
            .and(path("/pkg/-/pkg-1.0.0.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball_content.to_vec()))
            .expect(1) // Should only be called once
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/pkg/-/pkg-1.0.0.tgz", server.uri());

        // First call - downloads
        let result1 = cache_tarball(&client, "pkg", "1.0.0", &url, None)
            .await
            .unwrap();

        // Second call - should use cache
        let result2 = cache_tarball(&client, "pkg", "1.0.0", &url, None)
            .await
            .unwrap();

        assert_eq!(result1, result2);
    }

    #[tokio::test]
    #[serial]
    async fn test_cache_tarball_integrity_valid() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        let server = MockServer::start().await;
        let tarball_content = b"verified-content";
        let integrity = compute_sha512(tarball_content);

        Mock::given(method("GET"))
            .and(path("/pkg/-/pkg-1.0.0.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball_content.to_vec()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/pkg/-/pkg-1.0.0.tgz", server.uri());

        let result = cache_tarball(&client, "pkg", "1.0.0", &url, Some(&integrity))
            .await
            .unwrap();

        assert_eq!(result, tarball_content);
    }

    #[tokio::test]
    #[serial]
    async fn test_cache_tarball_integrity_mismatch() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        let server = MockServer::start().await;
        let tarball_content = b"actual-content";
        let wrong_integrity = compute_sha512(b"different-content");

        Mock::given(method("GET"))
            .and(path("/pkg/-/pkg-1.0.0.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball_content.to_vec()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/pkg/-/pkg-1.0.0.tgz", server.uri());

        let result = cache_tarball(&client, "pkg", "1.0.0", &url, Some(&wrong_integrity)).await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity") || err.contains("mismatch"));
    }

    #[tokio::test]
    #[serial]
    async fn test_cache_tarball_scoped_package_encoding() {
        let temp = TempDir::new().unwrap();
        std::env::set_var("HOME", temp.path());
        std::fs::create_dir_all(temp.path().join(".nary_cache")).unwrap();

        let server = MockServer::start().await;
        let tarball_content = b"scoped-pkg-content";

        Mock::given(method("GET"))
            .and(path("/@scope/pkg/-/pkg-1.0.0.tgz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball_content.to_vec()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/@scope/pkg/-/pkg-1.0.0.tgz", server.uri());

        let result = cache_tarball(&client, "@scope/pkg", "1.0.0", &url, None)
            .await
            .unwrap();

        assert_eq!(result, tarball_content);

        // Verify cache uses encoded path (@ and / in package name)
        let second = cache_tarball(&client, "@scope/pkg", "1.0.0", &url, None)
            .await
            .unwrap();
        assert_eq!(second, tarball_content);
    }
}
