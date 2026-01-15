use std::collections::HashMap;
use std::fs;
use std::path::Path;

/// Represents a parsed .npmrc configuration
#[derive(Debug, Default, Clone)]
pub struct NpmrcConfig {
    /// Default registry URL
    pub registry: Option<String>,

    /// Scope-specific registries: @scope -> registry URL
    pub scoped_registries: HashMap<String, String>,

    /// Auth tokens: registry host (without protocol) -> token
    pub auth_tokens: HashMap<String, String>,

    /// Legacy _auth values: registry host -> base64 encoded user:pass
    pub legacy_auth: HashMap<String, String>,
}

impl NpmrcConfig {
    /// Parse .npmrc content from a string
    pub fn parse(content: &str) -> Self {
        let mut config = Self::default();

        for line in content.lines() {
            let line = line.trim();

            // Skip comments and empty lines
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }

            // Parse key=value
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = expand_env_vars(value.trim());

                if key == "registry" {
                    config.registry = Some(value);
                } else if key.starts_with('@') && key.ends_with(":registry") {
                    // @scope:registry=URL
                    let scope = key.trim_end_matches(":registry");
                    config.scoped_registries.insert(scope.to_string(), value);
                } else if key.starts_with("//") && key.ends_with(":_authToken") {
                    // //registry.host/:_authToken=TOKEN
                    let host = key
                        .trim_start_matches("//")
                        .trim_end_matches(":_authToken")
                        .trim_end_matches('/');
                    config.auth_tokens.insert(host.to_string(), value);
                } else if key.starts_with("//") && key.ends_with(":_auth") {
                    // //registry.host/:_auth=BASE64
                    let host = key
                        .trim_start_matches("//")
                        .trim_end_matches(":_auth")
                        .trim_end_matches('/');
                    config.legacy_auth.insert(host.to_string(), value);
                }
            }
        }

        config
    }

    /// Load .npmrc from a file path
    pub fn load_from_file(path: &Path) -> Option<Self> {
        fs::read_to_string(path)
            .ok()
            .map(|content| Self::parse(&content))
    }

    /// Load and merge .npmrc files from standard locations
    /// Order: global (~/.npmrc) -> project (./.npmrc)
    /// Later files override earlier ones
    pub fn load() -> Self {
        let mut config = Self::default();

        // Global: ~/.npmrc
        if let Some(home) = dirs::home_dir() {
            if let Some(global) = Self::load_from_file(&home.join(".npmrc")) {
                config.merge(global);
            }
        }

        // Project: ./.npmrc
        if let Some(project) = Self::load_from_file(Path::new(".npmrc")) {
            config.merge(project);
        }

        config
    }

    /// Merge another config into this one (other takes precedence)
    fn merge(&mut self, other: Self) {
        if other.registry.is_some() {
            self.registry = other.registry;
        }
        self.scoped_registries.extend(other.scoped_registries);
        self.auth_tokens.extend(other.auth_tokens);
        self.legacy_auth.extend(other.legacy_auth);
    }
}

/// Expand environment variables in a string
/// Supports ${VAR} and $VAR syntax
fn expand_env_vars(value: &str) -> String {
    shellexpand::env(value)
        .unwrap_or_else(|_| value.into())
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_registry() {
        let content = "registry=https://custom.registry.com/";
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.registry,
            Some("https://custom.registry.com/".to_string())
        );
    }

    #[test]
    fn test_parse_scoped_registry() {
        let content = "@mycompany:registry=https://npm.mycompany.com/";
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.scoped_registries.get("@mycompany"),
            Some(&"https://npm.mycompany.com/".to_string())
        );
    }

    #[test]
    fn test_parse_auth_token() {
        let content = "//npm.mycompany.com/:_authToken=secret123";
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.auth_tokens.get("npm.mycompany.com"),
            Some(&"secret123".to_string())
        );
    }

    #[test]
    fn test_parse_multiple() {
        let content = r#"
registry=https://registry.npmjs.org/
@mycompany:registry=https://npm.mycompany.com/
//npm.mycompany.com/:_authToken=secret123
//registry.npmjs.org/:_authToken=public456
"#;
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.registry,
            Some("https://registry.npmjs.org/".to_string())
        );
        assert_eq!(config.scoped_registries.len(), 1);
        assert_eq!(config.auth_tokens.len(), 2);
    }

    #[test]
    fn test_env_var_expansion() {
        std::env::set_var("TEST_TOKEN", "expanded_value");
        let content = "//registry.npmjs.org/:_authToken=${TEST_TOKEN}";
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.auth_tokens.get("registry.npmjs.org"),
            Some(&"expanded_value".to_string())
        );
        std::env::remove_var("TEST_TOKEN");
    }

    #[test]
    fn test_comments_ignored() {
        let content = r#"
# This is a comment
; This is also a comment
registry=https://registry.npmjs.org/
"#;
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.registry,
            Some("https://registry.npmjs.org/".to_string())
        );
    }
}
