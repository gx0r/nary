//! Security audit utilities for checking package vulnerabilities.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::lockfile::{read_package_lock, PackageLock};

/// Raw advisory data from the npm bulk audit API response.
/// This matches the structure returned by the API.
#[derive(Debug, Clone, Deserialize)]
pub struct RawAdvisory {
    pub id: u64,
    pub severity: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub vulnerable_versions: Option<String>,
    pub patched_versions: Option<String>,
}

/// Response type from the npm bulk audit API.
/// Maps package names to arrays of advisories.
pub type AuditResponse = HashMap<String, Vec<RawAdvisory>>;

/// Summary of vulnerabilities found during an audit
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditSummary {
    pub total: usize,
    pub critical: usize,
    pub high: usize,
    pub moderate: usize,
    pub low: usize,
}

impl AuditSummary {
    /// Create an empty audit summary with no vulnerabilities
    pub fn empty() -> Self {
        Self::default()
    }

    /// Check if there are any vulnerabilities
    pub fn has_vulnerabilities(&self) -> bool {
        self.total > 0
    }

    /// Check if there are any high-severity or above vulnerabilities
    pub fn has_high_severity(&self) -> bool {
        self.critical > 0 || self.high > 0
    }
}

/// Build the payload for the npm audit bulk API from a package lock.
///
/// Returns a JSON object mapping package names to arrays of versions.
pub fn build_audit_payload(lock: &PackageLock) -> Value {
    let mut packages: HashMap<String, HashSet<String>> = HashMap::new();

    for (path, entry) in &lock.packages {
        if path.is_empty() {
            continue;
        }
        let name = path.rsplit("node_modules/").next().unwrap_or(path);
        if let Some(version) = &entry.version {
            packages
                .entry(name.to_string())
                .or_default()
                .insert(version.clone());
        }
    }

    packages
        .iter()
        .map(|(name, versions)| {
            (
                name.clone(),
                serde_json::json!(versions.iter().collect::<Vec<_>>()),
            )
        })
        .collect()
}

/// Detailed vulnerability advisory information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Advisory {
    pub id: u64,
    pub package: String,
    pub severity: String,
    pub title: String,
    pub url: Option<String>,
    pub vulnerable_versions: Option<String>,
    pub patched_versions: Option<String>,
}

/// Full audit result with detailed advisory information
#[derive(Debug, Clone, Default)]
pub struct AuditResult {
    pub summary: AuditSummary,
    pub advisories: Vec<Advisory>,
}

/// Fetch vulnerability information from the npm registry.
///
/// # Arguments
/// * `client` - HTTP client to use for the request
/// * `lockfile_path` - Path to the package-lock.json file
///
/// # Returns
/// An AuditSummary with counts of vulnerabilities by severity
pub async fn get_audit_summary(
    client: &Client,
    lockfile_path: &Path,
) -> Result<AuditSummary, Box<dyn std::error::Error + Send + Sync>> {
    let lock = match read_package_lock(lockfile_path) {
        Some(l) => l,
        None => return Ok(AuditSummary::empty()),
    };

    get_audit_summary_from_lock(client, &lock).await
}

/// Fetch vulnerability information from the npm registry using a pre-loaded lock.
///
/// # Arguments
/// * `client` - HTTP client to use for the request
/// * `lock` - The parsed package lock
///
/// # Returns
/// An AuditSummary with counts of vulnerabilities by severity
pub async fn get_audit_summary_from_lock(
    client: &Client,
    lock: &PackageLock,
) -> Result<AuditSummary, Box<dyn std::error::Error + Send + Sync>> {
    let audit_payload = build_audit_payload(lock);

    if audit_payload
        .as_object()
        .map(|o| o.is_empty())
        .unwrap_or(true)
    {
        return Ok(AuditSummary::empty());
    }

    let resp = client
        .post("https://registry.npmjs.org/-/npm/v1/security/advisories/bulk")
        .header("Content-Type", "application/json")
        .body(audit_payload.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        return Ok(AuditSummary::empty());
    }

    let audit_result: AuditResponse = resp.json().await?;

    Ok(parse_audit_response(&audit_result))
}

/// Parse the npm audit bulk API response into an AuditSummary.
pub fn parse_audit_response(audit_result: &AuditResponse) -> AuditSummary {
    let mut seen_ids: HashSet<u64> = HashSet::new();
    let mut critical: usize = 0;
    let mut high: usize = 0;
    let mut moderate: usize = 0;
    let mut low: usize = 0;

    for advisories in audit_result.values() {
        for advisory in advisories {
            // Skip if we've already counted this advisory
            if !seen_ids.insert(advisory.id) {
                continue;
            }

            let severity = advisory.severity.as_deref().unwrap_or("unknown");

            match severity {
                "critical" => critical += 1,
                "high" => high += 1,
                "moderate" => moderate += 1,
                "low" => low += 1,
                _ => {}
            }
        }
    }

    let total = critical + high + moderate + low;

    AuditSummary {
        total,
        critical,
        high,
        moderate,
        low,
    }
}

/// Parse the npm audit bulk API response into detailed advisories.
pub fn parse_audit_advisories(audit_result: &AuditResponse) -> Vec<Advisory> {
    let mut advisories = Vec::new();
    let mut seen_ids: HashSet<u64> = HashSet::new();

    for (package_name, advisory_list) in audit_result {
        for raw in advisory_list {
            // Skip duplicates
            if !seen_ids.insert(raw.id) {
                continue;
            }

            advisories.push(Advisory {
                id: raw.id,
                package: package_name.clone(),
                severity: raw
                    .severity
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                title: raw.title.clone().unwrap_or_default(),
                url: raw.url.clone(),
                vulnerable_versions: raw.vulnerable_versions.clone(),
                patched_versions: raw.patched_versions.clone(),
            });
        }
    }

    advisories
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile::PackageEntry;
    use indexmap::IndexMap;
    use serde_json::json;

    fn make_lock(packages: Vec<(&str, &str)>) -> PackageLock {
        let mut pkg_map: IndexMap<String, PackageEntry> = IndexMap::new();

        // Root entry
        pkg_map.insert("".to_string(), PackageEntry::default());

        for (name, version) in packages {
            pkg_map.insert(
                format!("node_modules/{}", name),
                PackageEntry {
                    version: Some(version.to_string()),
                    ..Default::default()
                },
            );
        }

        PackageLock {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            lockfile_version: 3,
            requires: false,
            packages: pkg_map,
        }
    }

    #[test]
    fn test_build_audit_payload_empty() {
        let lock = make_lock(vec![]);
        let payload = build_audit_payload(&lock);
        assert!(payload.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_build_audit_payload_simple() {
        let lock = make_lock(vec![("lodash", "4.17.20"), ("express", "4.18.0")]);
        let payload = build_audit_payload(&lock);

        let obj = payload.as_object().unwrap();
        assert!(obj.contains_key("lodash"));
        assert!(obj.contains_key("express"));

        let lodash_versions = obj["lodash"].as_array().unwrap();
        assert!(lodash_versions.contains(&json!("4.17.20")));
    }

    #[test]
    fn test_build_audit_payload_multiple_versions() {
        let mut lock = make_lock(vec![("lodash", "4.17.20")]);
        lock.packages.insert(
            "node_modules/express/node_modules/lodash".to_string(),
            PackageEntry {
                version: Some("4.17.21".to_string()),
                ..Default::default()
            },
        );

        let payload = build_audit_payload(&lock);
        let obj = payload.as_object().unwrap();

        let lodash_versions = obj["lodash"].as_array().unwrap();
        assert_eq!(lodash_versions.len(), 2);
    }

    fn parse_response(json: serde_json::Value) -> AuditResponse {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn test_parse_audit_response_empty() {
        let response = parse_response(json!({}));
        let summary = parse_audit_response(&response);
        assert_eq!(summary.total, 0);
    }

    #[test]
    fn test_parse_audit_response_single() {
        let response = parse_response(json!({
            "lodash": [
                {"id": 1, "severity": "high", "title": "Test vuln"}
            ]
        }));
        let summary = parse_audit_response(&response);
        assert_eq!(summary.total, 1);
        assert_eq!(summary.high, 1);
    }

    #[test]
    fn test_parse_audit_response_multiple_severities() {
        let response = parse_response(json!({
            "pkg1": [{"id": 1, "severity": "critical"}],
            "pkg2": [{"id": 2, "severity": "high"}],
            "pkg3": [{"id": 3, "severity": "moderate"}],
            "pkg4": [{"id": 4, "severity": "low"}]
        }));
        let summary = parse_audit_response(&response);
        assert_eq!(summary.total, 4);
        assert_eq!(summary.critical, 1);
        assert_eq!(summary.high, 1);
        assert_eq!(summary.moderate, 1);
        assert_eq!(summary.low, 1);
    }

    #[test]
    fn test_parse_audit_response_dedupes_by_id() {
        let response = parse_response(json!({
            "pkg1": [{"id": 1, "severity": "high"}],
            "pkg2": [{"id": 1, "severity": "high"}]  // Same advisory ID
        }));
        let summary = parse_audit_response(&response);
        assert_eq!(summary.total, 1); // Should only count once
    }

    #[test]
    fn test_audit_summary_has_vulnerabilities() {
        let empty = AuditSummary::empty();
        assert!(!empty.has_vulnerabilities());

        let with_vulns = AuditSummary {
            total: 1,
            low: 1,
            ..Default::default()
        };
        assert!(with_vulns.has_vulnerabilities());
    }

    #[test]
    fn test_audit_summary_has_high_severity() {
        let low_only = AuditSummary {
            total: 1,
            low: 1,
            ..Default::default()
        };
        assert!(!low_only.has_high_severity());

        let with_high = AuditSummary {
            total: 1,
            high: 1,
            ..Default::default()
        };
        assert!(with_high.has_high_severity());

        let with_critical = AuditSummary {
            total: 1,
            critical: 1,
            ..Default::default()
        };
        assert!(with_critical.has_high_severity());
    }

    #[test]
    fn test_parse_audit_advisories() {
        let response = parse_response(json!({
            "lodash": [
                {
                    "id": 123,
                    "severity": "high",
                    "title": "Prototype Pollution",
                    "url": "https://npmjs.com/advisories/123",
                    "vulnerable_versions": "<4.17.21",
                    "patched_versions": ">=4.17.21"
                }
            ]
        }));

        let advisories = parse_audit_advisories(&response);
        assert_eq!(advisories.len(), 1);
        assert_eq!(advisories[0].id, 123);
        assert_eq!(advisories[0].package, "lodash");
        assert_eq!(advisories[0].severity, "high");
        assert_eq!(advisories[0].title, "Prototype Pollution");
    }
}
