//! Package maturity checking for supply chain security.
//!
//! Prevents installation of packages published within a configurable time window.
//! This helps protect against supply chain attacks by allowing time for the
//! community to detect malicious packages.

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Default maturity period in minutes (7 days)
pub const DEFAULT_MATURITY_MINUTES: u64 = 10080;

/// Configuration for maturity age filtering
#[derive(Clone, Debug)]
pub struct MaturityConfig {
    /// Minimum release age in minutes (0 = disabled)
    pub minimum_age_minutes: u64,

    /// Packages excluded from maturity check (exact name matches)
    pub excluded_packages: Vec<String>,

    /// Whether to bypass maturity checks entirely (--allow-new-packages)
    pub allow_new_packages: bool,
}

impl Default for MaturityConfig {
    fn default() -> Self {
        Self {
            minimum_age_minutes: DEFAULT_MATURITY_MINUTES,
            excluded_packages: Vec::new(),
            allow_new_packages: false,
        }
    }
}

impl MaturityConfig {
    /// Create a disabled maturity config (no checks)
    pub fn disabled() -> Self {
        Self {
            minimum_age_minutes: 0,
            excluded_packages: Vec::new(),
            allow_new_packages: true,
        }
    }

    /// Check if a package is excluded from maturity checks
    pub fn is_excluded(&self, package_name: &str) -> bool {
        self.excluded_packages
            .iter()
            .any(|p| p == package_name || package_name.starts_with(&format!("{}/", p)))
    }

    /// Check if maturity checking is enabled
    pub fn is_enabled(&self) -> bool {
        !self.allow_new_packages && self.minimum_age_minutes > 0
    }

    /// Check if maturity should be applied for a specific package
    pub fn should_check(&self, package_name: &str) -> bool {
        self.is_enabled() && !self.is_excluded(package_name)
    }
}

/// Result of a maturity check for a specific version
#[derive(Clone, Debug)]
pub enum MaturityCheckResult {
    /// Version is mature enough
    Mature,
    /// Version is too new
    TooNew {
        published_at: DateTime<Utc>,
        age_minutes: u64,
    },
    /// No time data available for version
    NoTimeData,
}

/// Information about a version that was skipped due to maturity requirements
#[derive(Clone, Debug)]
pub struct MaturityFallbackInfo {
    /// The version that was skipped
    pub skipped_version: String,
    /// When the skipped version was published
    pub skipped_published_at: DateTime<Utc>,
    /// How old the skipped version is (in minutes)
    pub skipped_age_minutes: u64,
    /// The required age (in minutes)
    pub required_age_minutes: u64,
}

impl MaturityFallbackInfo {
    /// Format the age in a human-readable way
    pub fn format_age(&self) -> String {
        format_duration_minutes(self.skipped_age_minutes)
    }

    /// Format the required age in a human-readable way
    pub fn format_required(&self) -> String {
        format_duration_minutes(self.required_age_minutes)
    }
}

/// Format a duration in minutes to a human-readable string
fn format_duration_minutes(minutes: u64) -> String {
    if minutes < 60 {
        format!("{}m", minutes)
    } else if minutes < 1440 {
        let hours = minutes / 60;
        format!("{}h", hours)
    } else {
        let days = minutes / 1440;
        format!("{}d", days)
    }
}

/// Parse the `time` field from npm registry metadata to get publish timestamp for a version.
///
/// npm registry metadata includes a `time` object like:
/// ```json
/// {
///   "time": {
///     "created": "2011-08-26T...",
///     "modified": "2024-01-15T...",
///     "1.0.0": "2011-08-26T...",
///     "1.0.1": "2011-09-01T..."
///   }
/// }
/// ```
pub fn get_version_publish_time(root_metadata: &Value, version: &str) -> Option<DateTime<Utc>> {
    root_metadata
        .get("time")?
        .get(version)?
        .as_str()?
        .parse::<DateTime<Utc>>()
        .ok()
}

/// Check if a version meets the maturity requirements.
pub fn check_version_maturity(
    root_metadata: &Value,
    version: &str,
    config: &MaturityConfig,
) -> MaturityCheckResult {
    if !config.is_enabled() {
        return MaturityCheckResult::Mature;
    }

    let Some(published_at) = get_version_publish_time(root_metadata, version) else {
        return MaturityCheckResult::NoTimeData;
    };

    let age = Utc::now().signed_duration_since(published_at);
    let age_minutes = age.num_minutes().max(0) as u64;

    if age_minutes >= config.minimum_age_minutes {
        MaturityCheckResult::Mature
    } else {
        MaturityCheckResult::TooNew {
            published_at,
            age_minutes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_maturity_config_default() {
        let config = MaturityConfig::default();
        assert_eq!(config.minimum_age_minutes, DEFAULT_MATURITY_MINUTES);
        assert!(config.excluded_packages.is_empty());
        assert!(!config.allow_new_packages);
        assert!(config.is_enabled());
    }

    #[test]
    fn test_maturity_config_disabled() {
        let config = MaturityConfig::disabled();
        assert!(!config.is_enabled());
    }

    #[test]
    fn test_is_excluded() {
        let config = MaturityConfig {
            minimum_age_minutes: 4320,
            excluded_packages: vec!["lodash".to_string(), "@types".to_string()],
            allow_new_packages: false,
        };

        assert!(config.is_excluded("lodash"));
        assert!(!config.is_excluded("express"));
        // Test scoped package prefix matching
        assert!(config.is_excluded("@types/node"));
        assert!(config.is_excluded("@types/react"));
    }

    #[test]
    fn test_should_check() {
        let config = MaturityConfig {
            minimum_age_minutes: 4320,
            excluded_packages: vec!["lodash".to_string()],
            allow_new_packages: false,
        };

        assert!(!config.should_check("lodash"));
        assert!(config.should_check("express"));
    }

    #[test]
    fn test_get_version_publish_time() {
        let metadata = json!({
            "time": {
                "1.0.0": "2024-01-01T00:00:00.000Z",
                "1.0.1": "2024-06-15T12:30:00.000Z"
            }
        });

        let time = get_version_publish_time(&metadata, "1.0.0");
        assert!(time.is_some());

        let time = get_version_publish_time(&metadata, "9.9.9");
        assert!(time.is_none());
    }

    #[test]
    fn test_check_version_maturity_disabled() {
        let config = MaturityConfig::disabled();
        let metadata = json!({});

        let result = check_version_maturity(&metadata, "1.0.0", &config);
        assert!(matches!(result, MaturityCheckResult::Mature));
    }

    #[test]
    fn test_check_version_maturity_no_time_data() {
        let config = MaturityConfig::default();
        let metadata = json!({
            "versions": { "1.0.0": {} }
        });

        let result = check_version_maturity(&metadata, "1.0.0", &config);
        assert!(matches!(result, MaturityCheckResult::NoTimeData));
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration_minutes(30), "30m");
        assert_eq!(format_duration_minutes(60), "1h");
        assert_eq!(format_duration_minutes(120), "2h");
        assert_eq!(format_duration_minutes(1440), "1d");
        assert_eq!(format_duration_minutes(4320), "3d");
    }

    #[test]
    fn test_check_version_maturity_mature() {
        use chrono::Duration;

        let config = MaturityConfig {
            minimum_age_minutes: 60, // 1 hour
            excluded_packages: vec![],
            allow_new_packages: false,
        };

        // Version published 2 hours ago should be mature
        let old_time = Utc::now() - Duration::hours(2);
        let metadata = json!({
            "time": {
                "1.0.0": old_time.to_rfc3339()
            }
        });

        let result = check_version_maturity(&metadata, "1.0.0", &config);
        assert!(matches!(result, MaturityCheckResult::Mature));
    }

    #[test]
    fn test_check_version_maturity_too_new() {
        use chrono::Duration;

        let config = MaturityConfig {
            minimum_age_minutes: 4320, // 3 days
            excluded_packages: vec![],
            allow_new_packages: false,
        };

        // Version published 1 hour ago should be too new
        let recent_time = Utc::now() - Duration::hours(1);
        let metadata = json!({
            "time": {
                "1.0.0": recent_time.to_rfc3339()
            }
        });

        let result = check_version_maturity(&metadata, "1.0.0", &config);
        match result {
            MaturityCheckResult::TooNew { age_minutes, .. } => {
                assert!(age_minutes < 120); // Should be around 60 minutes
            }
            _ => panic!("Expected TooNew result"),
        }
    }

    #[test]
    fn test_maturity_fallback_info_format() {
        use chrono::Duration;

        let fallback = MaturityFallbackInfo {
            skipped_version: "2.0.0".to_string(),
            skipped_published_at: Utc::now() - Duration::hours(2),
            skipped_age_minutes: 120,
            required_age_minutes: 4320,
        };

        assert_eq!(fallback.format_age(), "2h");
        assert_eq!(fallback.format_required(), "3d");
    }
}
