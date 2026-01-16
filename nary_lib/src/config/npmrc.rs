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

    /// Minimum release age in minutes for package maturity check (nary-specific)
    pub minimum_release_age: Option<u64>,

    /// Packages excluded from maturity age check (nary-specific)
    pub maturity_exclude: Vec<String>,
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
                } else if key == "nary-minimum-release-age" {
                    // nary-minimum-release-age=<minutes>
                    if let Ok(minutes) = value.parse::<u64>() {
                        config.minimum_release_age = Some(minutes);
                    }
                } else if key == "nary-maturity-exclude[]" {
                    // nary-maturity-exclude[]=<package-name>
                    config.maturity_exclude.push(value);
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
        if other.minimum_release_age.is_some() {
            self.minimum_release_age = other.minimum_release_age;
        }
        self.maturity_exclude.extend(other.maturity_exclude);
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

    #[test]
    fn test_parse_maturity_release_age() {
        let content = "nary-minimum-release-age=4320";
        let config = NpmrcConfig::parse(content);
        assert_eq!(config.minimum_release_age, Some(4320));
    }

    #[test]
    fn test_parse_maturity_release_age_invalid() {
        let content = "nary-minimum-release-age=not-a-number";
        let config = NpmrcConfig::parse(content);
        assert_eq!(config.minimum_release_age, None);
    }

    #[test]
    fn test_parse_maturity_exclude() {
        let content = r#"
nary-maturity-exclude[]=lodash
nary-maturity-exclude[]=express
nary-maturity-exclude[]=@types
"#;
        let config = NpmrcConfig::parse(content);
        assert_eq!(config.maturity_exclude.len(), 3);
        assert!(config.maturity_exclude.contains(&"lodash".to_string()));
        assert!(config.maturity_exclude.contains(&"express".to_string()));
        assert!(config.maturity_exclude.contains(&"@types".to_string()));
    }

    #[test]
    fn test_parse_maturity_all_options() {
        let content = r#"
registry=https://registry.npmjs.org/
nary-minimum-release-age=1440
nary-maturity-exclude[]=lodash
nary-maturity-exclude[]=react
"#;
        let config = NpmrcConfig::parse(content);
        assert_eq!(
            config.registry,
            Some("https://registry.npmjs.org/".to_string())
        );
        assert_eq!(config.minimum_release_age, Some(1440));
        assert_eq!(config.maturity_exclude.len(), 2);
    }

    #[test]
    fn test_merge_maturity_settings() {
        // Simulate global config with default maturity settings
        let global_content = r#"
registry=https://registry.npmjs.org/
nary-minimum-release-age=4320
nary-maturity-exclude[]=lodash
"#;
        let mut global = NpmrcConfig::parse(global_content);

        // Project config overrides minimum age and adds more exclusions
        let project_content = r#"
nary-minimum-release-age=1440
nary-maturity-exclude[]=express
nary-maturity-exclude[]=react
"#;
        let project = NpmrcConfig::parse(project_content);

        // Merge: project takes precedence
        global.merge(project);

        // minimum_release_age should be overridden by project
        assert_eq!(global.minimum_release_age, Some(1440));

        // maturity_exclude should be merged (global + project)
        assert_eq!(global.maturity_exclude.len(), 3);
        assert!(global.maturity_exclude.contains(&"lodash".to_string()));
        assert!(global.maturity_exclude.contains(&"express".to_string()));
        assert!(global.maturity_exclude.contains(&"react".to_string()));

        // registry from global should remain (project didn't override)
        assert_eq!(
            global.registry,
            Some("https://registry.npmjs.org/".to_string())
        );
    }

    #[test]
    fn test_merge_maturity_no_override() {
        // Global has maturity settings
        let global_content = r#"
nary-minimum-release-age=4320
nary-maturity-exclude[]=lodash
"#;
        let mut global = NpmrcConfig::parse(global_content);

        // Project has no maturity settings
        let project_content = r#"
registry=https://custom.registry.com/
"#;
        let project = NpmrcConfig::parse(project_content);

        global.merge(project);

        // Global maturity settings should remain unchanged
        assert_eq!(global.minimum_release_age, Some(4320));
        assert_eq!(global.maturity_exclude.len(), 1);
        assert!(global.maturity_exclude.contains(&"lodash".to_string()));

        // Project registry should be applied
        assert_eq!(
            global.registry,
            Some("https://custom.registry.com/".to_string())
        );
    }
}
