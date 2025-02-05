use super::{NpmrcConfig, DEFAULT_REGISTRY};
use percent_encoding::utf8_percent_encode;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, RequestBuilder};

/// Configuration for registry access
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    config: NpmrcConfig,
    default_registry: String,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            config: NpmrcConfig::default(),
            default_registry: DEFAULT_REGISTRY.to_string(),
        }
    }
}

impl RegistryConfig {
    /// Load configuration from .npmrc files
    pub fn load() -> Self {
        let config = NpmrcConfig::load();
        let default_registry = config
            .registry
            .clone()
            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string());

        Self {
            config,
            default_registry,
        }
    }

    /// Create a config with a specific registry override
    pub fn with_registry(registry: String) -> Self {
        Self {
            config: NpmrcConfig::load(),
            default_registry: registry,
        }
    }

    /// Create a config with a custom NpmrcConfig (for testing)
    pub fn with_config(config: NpmrcConfig, default_registry: String) -> Self {
        Self {
            config,
            default_registry,
        }
    }

    /// Set the default registry (for CLI override)
    pub fn set_default_registry(&mut self, registry: &str) {
        self.default_registry = registry.to_string();
    }

    /// Get the registry URL for a package name
    /// Handles scoped packages: @scope/pkg -> scoped registry if configured
    pub fn registry_for_package(&self, package_name: &str) -> &str {
        if package_name.starts_with('@') {
            if let Some(scope) = package_name.split('/').next() {
                if let Some(registry) = self.config.scoped_registries.get(scope) {
                    return registry;
                }
            }
        }
        &self.default_registry
    }

    /// Build the metadata URL for a package
    pub fn metadata_url(&self, package_name: &str) -> String {
        let registry = self.registry_for_package(package_name);
        let encoded_name =
            utf8_percent_encode(package_name, crate::PATH_SEGMENT_ENCODE_SET).to_string();

        let registry = registry.trim_end_matches('/');
        format!("{}/{}", registry, encoded_name)
    }

    /// Build the URL for a specific version's metadata
    pub fn version_url(&self, package_name: &str, version: &str) -> String {
        let registry = self.registry_for_package(package_name);
        let encoded_name =
            utf8_percent_encode(package_name, crate::PATH_SEGMENT_ENCODE_SET).to_string();
        let encoded_version =
            utf8_percent_encode(version, crate::PATH_SEGMENT_ENCODE_SET).to_string();

        let registry = registry.trim_end_matches('/');
        format!("{}/{}/{}", registry, encoded_name, encoded_version)
    }

    /// Get authentication headers for a URL
    pub fn auth_headers(&self, url: &str) -> Option<HeaderMap> {
        let url_key = extract_registry_key(url)?;

        // Check for bearer token first (try longest prefix match)
        if let Some(token) = find_longest_match(&self.config.auth_tokens, &url_key) {
            let mut headers = HeaderMap::new();
            let auth_value = format!("Bearer {}", token);
            if let Ok(value) = HeaderValue::from_str(&auth_value) {
                headers.insert(AUTHORIZATION, value);
                return Some(headers);
            }
        }

        // Check for legacy basic auth
        if let Some(auth) = find_longest_match(&self.config.legacy_auth, &url_key) {
            let mut headers = HeaderMap::new();
            let auth_value = format!("Basic {}", auth);
            if let Ok(value) = HeaderValue::from_str(&auth_value) {
                headers.insert(AUTHORIZATION, value);
                return Some(headers);
            }
        }

        None
    }

    /// Build an authenticated GET request for a URL
    pub fn authenticated_get(&self, client: &Client, url: &str) -> RequestBuilder {
        let mut request = client.get(url);
        if let Some(headers) = self.auth_headers(url) {
            for (key, value) in headers.iter() {
                request = request.header(key, value.clone());
            }
        }
        request
    }

    /// Check if authentication is configured for a registry
    pub fn has_auth_for(&self, url: &str) -> bool {
        if let Some(url_key) = extract_registry_key(url) {
            find_longest_match(&self.config.auth_tokens, &url_key).is_some()
                || find_longest_match(&self.config.legacy_auth, &url_key).is_some()
        } else {
            false
        }
    }
}

/// Extract registry key from URL for auth lookup (host + path prefix)
/// "https://npm.mycompany.com/api/npm/@scope/pkg" -> "npm.mycompany.com/api/npm/@scope/pkg"
fn extract_registry_key(url: &str) -> Option<String> {
    let url = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    Some(url.to_string())
}

/// Find the longest matching key that is a prefix of the URL
fn find_longest_match<'a>(
    map: &'a std::collections::HashMap<String, String>,
    url_key: &str,
) -> Option<&'a str> {
    map.iter()
        .filter(|(key, _)| url_key.starts_with(key.as_str()))
        .max_by_key(|(key, _)| key.len())
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_registry() {
        let config = RegistryConfig::default();
        assert_eq!(config.registry_for_package("lodash"), DEFAULT_REGISTRY);
    }

    #[test]
    fn test_scoped_package_registry() {
        let mut npmrc = NpmrcConfig::default();
        npmrc.scoped_registries.insert(
            "@mycompany".to_string(),
            "https://npm.mycompany.com/".to_string(),
        );

        let config = RegistryConfig::with_config(npmrc, DEFAULT_REGISTRY.to_string());

        assert_eq!(
            config.registry_for_package("@mycompany/utils"),
            "https://npm.mycompany.com/"
        );
        assert_eq!(config.registry_for_package("lodash"), DEFAULT_REGISTRY);
    }

    #[test]
    fn test_metadata_url() {
        let config = RegistryConfig::default();
        assert_eq!(
            config.metadata_url("lodash"),
            "https://registry.npmjs.org/lodash"
        );
        // Scoped packages: the @ and / are encoded
        assert_eq!(
            config.metadata_url("@types/node"),
            "https://registry.npmjs.org/@types/node"
        );
    }

    #[test]
    fn test_auth_headers() {
        let mut npmrc = NpmrcConfig::default();
        npmrc
            .auth_tokens
            .insert("npm.mycompany.com".to_string(), "secret123".to_string());

        let config = RegistryConfig::with_config(npmrc, DEFAULT_REGISTRY.to_string());

        let headers = config.auth_headers("https://npm.mycompany.com/package");
        assert!(headers.is_some());

        let headers = headers.unwrap();
        assert!(headers.contains_key(AUTHORIZATION));

        // No auth for public registry
        let public_headers = config.auth_headers("https://registry.npmjs.org/package");
        assert!(public_headers.is_none());
    }

    #[test]
    fn test_auth_headers_with_path() {
        let mut npmrc = NpmrcConfig::default();
        // Auth token with path prefix (e.g., //registry.example.com/npm/:_authToken=xxx)
        npmrc.auth_tokens.insert(
            "registry.example.com/npm".to_string(),
            "pathtoken".to_string(),
        );

        let config = RegistryConfig::with_config(npmrc, DEFAULT_REGISTRY.to_string());

        // Should match URLs under that path
        let headers = config.auth_headers("https://registry.example.com/npm/@scope/pkg");
        assert!(headers.is_some());

        // Should not match URLs outside that path
        let other_headers = config.auth_headers("https://registry.example.com/other/pkg");
        assert!(other_headers.is_none());
    }
}
