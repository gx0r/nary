//! npm `overrides` support for forcing specific versions of transitive dependencies.
//!
//! Supports the npm overrides syntax:
//! ```json
//! {
//!   "overrides": {
//!     "lodash": "4.17.21",                    // All lodash everywhere
//!     "express": { "qs": "6.11.0" },          // Only qs when under express
//!     "foo": { "bar": { "baz": "1.0.0" } },   // Deep nested path matching
//!     "react": "$react",                      // Use root's react version
//!     ".": { "lodash": "4.17.21" },           // Only direct deps of root
//!     "foo@^2.0.0": { "bar": "1.0.0" }        // Only when foo matches ^2.0.0
//!   }
//! }
//! ```

use node_semver::{Range, Version};
use serde_json::Value;
use std::collections::HashMap;

/// A package in the resolution path with its resolved version
#[derive(Clone, Debug)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: String,
}

/// An entry in the parent chain, which can include a version constraint
#[derive(Clone, Debug, PartialEq)]
pub struct ParentSpec {
    /// Package name (or "." for root)
    pub name: String,
    /// Optional version constraint (e.g., "^2.0.0")
    pub version_constraint: Option<String>,
}

impl ParentSpec {
    /// Parse a key like "express" or "express@^4.0.0" or "."
    fn parse(key: &str) -> Self {
        if key == "." {
            return ParentSpec {
                name: ".".to_string(),
                version_constraint: None,
            };
        }

        // Handle scoped packages: @scope/pkg@version
        if key.starts_with('@') {
            // Find the second @ (version separator)
            if let Some(slash_pos) = key.find('/') {
                if let Some(at_pos) = key[slash_pos..].find('@') {
                    let version_start = slash_pos + at_pos;
                    return ParentSpec {
                        name: key[..version_start].to_string(),
                        version_constraint: Some(key[version_start + 1..].to_string()),
                    };
                }
            }
            // No version specified for scoped package
            return ParentSpec {
                name: key.to_string(),
                version_constraint: None,
            };
        }

        // Non-scoped package: find @ for version
        if let Some(at_pos) = key.find('@') {
            ParentSpec {
                name: key[..at_pos].to_string(),
                version_constraint: Some(key[at_pos + 1..].to_string()),
            }
        } else {
            ParentSpec {
                name: key.to_string(),
                version_constraint: None,
            }
        }
    }

    /// Check if a resolved package matches this spec
    fn matches(&self, resolved: &ResolvedPackage) -> bool {
        if self.name != resolved.name {
            return false;
        }

        match &self.version_constraint {
            None => true, // No constraint, name match is enough
            Some(constraint) => {
                // Try to parse as semver range
                if let Ok(range) = constraint.parse::<Range>() {
                    if let Ok(version) = resolved.version.parse::<Version>() {
                        return version.satisfies(&range);
                    }
                }
                // Fall back to exact match
                constraint == &resolved.version
            }
        }
    }
}

/// A single override rule specifying what version to use for a package
#[derive(Clone, Debug, PartialEq)]
pub struct OverrideRule {
    /// Package name to override (e.g., "lodash")
    pub target_package: String,
    /// Parent path that must match (empty = global override)
    /// e.g., [express, body-parser] means "target under body-parser under express"
    pub parent_chain: Vec<ParentSpec>,
    /// Version to use (e.g., "4.17.21" or "^4.0.0")
    pub override_version: String,
    /// Whether this rule was applied during resolution (for warnings)
    pub applied: std::cell::Cell<bool>,
}

/// Configuration for npm overrides
#[derive(Clone, Debug, Default)]
pub struct OverridesConfig {
    /// Rules sorted by specificity (longest parent_chain first)
    pub rules: Vec<OverrideRule>,
}

impl OverridesConfig {
    /// Parse overrides from package.json "overrides" field
    ///
    /// # Arguments
    /// * `value` - The "overrides" field from package.json
    /// * `root_deps` - Root dependencies for resolving $reference syntax (name -> version)
    pub fn parse(value: &Value, root_deps: &HashMap<String, String>) -> Self {
        let mut rules = Vec::new();

        if let Some(obj) = value.as_object() {
            Self::parse_recursive(obj, &[], root_deps, &mut rules);
        }

        // Sort by specificity: longest parent_chain first (most specific wins)
        // Secondary sort: rules with version constraints are more specific
        rules.sort_by(|a, b| {
            let len_cmp = b.parent_chain.len().cmp(&a.parent_chain.len());
            if len_cmp != std::cmp::Ordering::Equal {
                return len_cmp;
            }
            // Same length: count version constraints (more constraints = more specific)
            let a_constraints = a
                .parent_chain
                .iter()
                .filter(|p| p.version_constraint.is_some())
                .count();
            let b_constraints = b
                .parent_chain
                .iter()
                .filter(|p| p.version_constraint.is_some())
                .count();
            b_constraints.cmp(&a_constraints)
        });

        OverridesConfig { rules }
    }

    /// Recursively parse nested override objects
    fn parse_recursive(
        obj: &serde_json::Map<String, Value>,
        parent_chain: &[ParentSpec],
        root_deps: &HashMap<String, String>,
        rules: &mut Vec<OverrideRule>,
    ) {
        for (key, value) in obj {
            match value {
                Value::String(version) => {
                    // Leaf node: "package": "version"
                    let resolved_version = Self::resolve_reference(version, root_deps);

                    if key == "." {
                        // "." as a string value key means "override the parent package itself"
                        // e.g., "foo": { ".": "1.0.0" } -> override foo to 1.0.0
                        if let Some(parent_spec) = parent_chain.last() {
                            rules.push(OverrideRule {
                                target_package: parent_spec.name.clone(),
                                // Use all but the last element as the parent chain
                                parent_chain: parent_chain[..parent_chain.len() - 1].to_vec(),
                                override_version: resolved_version,
                                applied: std::cell::Cell::new(false),
                            });
                        }
                        // If parent_chain is empty, "." at top level with string value is invalid - skip
                    } else {
                        rules.push(OverrideRule {
                            target_package: key.clone(),
                            parent_chain: parent_chain.to_vec(),
                            override_version: resolved_version,
                            applied: std::cell::Cell::new(false),
                        });
                    }
                }
                Value::Object(nested) => {
                    // Nested: "parent": { "child": "version" }
                    let mut new_chain = parent_chain.to_vec();
                    new_chain.push(ParentSpec::parse(key));
                    Self::parse_recursive(nested, &new_chain, root_deps, rules);
                }
                _ => {
                    // Skip invalid values (null, array, number, bool)
                }
            }
        }
    }

    /// Resolve $reference syntax to actual version from root dependencies
    fn resolve_reference(version: &str, root_deps: &HashMap<String, String>) -> String {
        if let Some(ref_name) = version.strip_prefix('$') {
            // $react -> look up "react" in root dependencies
            root_deps
                .get(ref_name)
                .cloned()
                .unwrap_or_else(|| version.to_string())
        } else {
            version.to_string()
        }
    }

    /// Find an override for a package given the current resolution path
    ///
    /// # Arguments
    /// * `package` - The package name to look up
    /// * `resolution_path` - The chain of resolved packages leading to this dependency
    ///
    /// # Returns
    /// The override version if a matching rule exists, None otherwise
    pub fn find_override(
        &self,
        package: &str,
        resolution_path: &[ResolvedPackage],
    ) -> Option<&str> {
        // Rules are sorted by specificity (longest parent_chain first)
        // Find the first rule that matches
        for rule in &self.rules {
            if rule.target_package != package {
                continue;
            }

            if self.rule_matches(rule, resolution_path) {
                rule.applied.set(true);
                return Some(&rule.override_version);
            }
        }
        None
    }

    /// Convenience method for when you only have package names (no versions)
    /// This is used when version info isn't available (legacy compatibility)
    pub fn find_override_by_names(
        &self,
        package: &str,
        resolution_path: &[String],
    ) -> Option<&str> {
        let resolved: Vec<ResolvedPackage> = resolution_path
            .iter()
            .map(|name| ResolvedPackage {
                name: name.clone(),
                version: String::new(), // Empty version won't match version constraints
            })
            .collect();
        self.find_override(package, &resolved)
    }

    /// Check if a rule matches the given resolution path
    fn rule_matches(&self, rule: &OverrideRule, resolution_path: &[ResolvedPackage]) -> bool {
        if rule.parent_chain.is_empty() {
            return true; // Global override matches everything
        }

        // Check for "." at the start of parent_chain (root-only constraint)
        if rule.parent_chain.first().map(|p| p.name.as_str()) == Some(".") {
            // "." means only direct dependencies of root
            if !resolution_path.is_empty() {
                return false; // Not a direct dep
            }
            // Check remaining chain (should be empty for just ".")
            if rule.parent_chain.len() == 1 {
                return true;
            }
            // Shouldn't normally have more after ".", but handle it
            return false;
        }

        // Check if resolution_path ends with parent_chain
        if resolution_path.len() < rule.parent_chain.len() {
            return false;
        }

        let start = resolution_path.len() - rule.parent_chain.len();
        for (i, spec) in rule.parent_chain.iter().enumerate() {
            if !spec.matches(&resolution_path[start + i]) {
                return false;
            }
        }
        true
    }

    /// Returns true if there are any override rules configured
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Get warnings for overrides that were never applied
    pub fn get_unapplied_warnings(&self) -> Vec<String> {
        self.rules
            .iter()
            .filter(|r| !r.applied.get())
            .map(|r| {
                if r.parent_chain.is_empty() {
                    format!(
                        "Override for '{}' was not applied (package not found in dependency tree)",
                        r.target_package
                    )
                } else {
                    let path: Vec<&str> = r.parent_chain.iter().map(|p| p.name.as_str()).collect();
                    format!(
                        "Override for '{}' under [{}] was not applied",
                        r.target_package,
                        path.join(" > ")
                    )
                }
            })
            .collect()
    }

    /// Reset applied flags (useful for re-resolution)
    pub fn reset_applied(&self) {
        for rule in &self.rules {
            rule.applied.set(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_deps() -> HashMap<String, String> {
        HashMap::new()
    }

    fn make_root_deps(deps: &[(&str, &str)]) -> HashMap<String, String> {
        deps.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn pkg(name: &str, version: &str) -> ResolvedPackage {
        ResolvedPackage {
            name: name.to_string(),
            version: version.to_string(),
        }
    }

    // ========== ParentSpec parsing tests ==========

    #[test]
    fn test_parent_spec_parse_simple() {
        let spec = ParentSpec::parse("express");
        assert_eq!(spec.name, "express");
        assert_eq!(spec.version_constraint, None);
    }

    #[test]
    fn test_parent_spec_parse_with_version() {
        let spec = ParentSpec::parse("express@^4.0.0");
        assert_eq!(spec.name, "express");
        assert_eq!(spec.version_constraint, Some("^4.0.0".to_string()));
    }

    #[test]
    fn test_parent_spec_parse_scoped() {
        let spec = ParentSpec::parse("@babel/core");
        assert_eq!(spec.name, "@babel/core");
        assert_eq!(spec.version_constraint, None);
    }

    #[test]
    fn test_parent_spec_parse_scoped_with_version() {
        let spec = ParentSpec::parse("@babel/core@^7.0.0");
        assert_eq!(spec.name, "@babel/core");
        assert_eq!(spec.version_constraint, Some("^7.0.0".to_string()));
    }

    #[test]
    fn test_parent_spec_parse_dot() {
        let spec = ParentSpec::parse(".");
        assert_eq!(spec.name, ".");
        assert_eq!(spec.version_constraint, None);
    }

    // ========== ParentSpec matching tests ==========

    #[test]
    fn test_parent_spec_matches_name_only() {
        let spec = ParentSpec::parse("express");
        assert!(spec.matches(&pkg("express", "4.18.2")));
        assert!(spec.matches(&pkg("express", "5.0.0")));
        assert!(!spec.matches(&pkg("koa", "2.0.0")));
    }

    #[test]
    fn test_parent_spec_matches_with_version_constraint() {
        let spec = ParentSpec::parse("express@^4.0.0");
        assert!(spec.matches(&pkg("express", "4.18.2")));
        assert!(spec.matches(&pkg("express", "4.0.0")));
        assert!(!spec.matches(&pkg("express", "5.0.0"))); // ^4 doesn't match 5
        assert!(!spec.matches(&pkg("express", "3.0.0"))); // ^4 doesn't match 3
    }

    #[test]
    fn test_parent_spec_matches_exact_version() {
        let spec = ParentSpec::parse("lodash@4.17.21");
        assert!(spec.matches(&pkg("lodash", "4.17.21")));
        assert!(!spec.matches(&pkg("lodash", "4.17.20")));
    }

    // ========== Parsing tests ==========

    #[test]
    fn test_parse_flat_override() {
        let overrides = json!({
            "lodash": "4.17.21"
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "lodash");
        assert_eq!(config.rules[0].parent_chain, Vec::<ParentSpec>::new());
        assert_eq!(config.rules[0].override_version, "4.17.21");
    }

    #[test]
    fn test_parse_multiple_flat_overrides() {
        let overrides = json!({
            "lodash": "4.17.21",
            "qs": "6.11.0"
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 2);
        let names: Vec<&str> = config
            .rules
            .iter()
            .map(|r| r.target_package.as_str())
            .collect();
        assert!(names.contains(&"lodash"));
        assert!(names.contains(&"qs"));
    }

    #[test]
    fn test_parse_nested_override_single_level() {
        let overrides = json!({
            "express": {
                "qs": "6.11.0"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "qs");
        assert_eq!(config.rules[0].parent_chain.len(), 1);
        assert_eq!(config.rules[0].parent_chain[0].name, "express");
        assert_eq!(config.rules[0].override_version, "6.11.0");
    }

    #[test]
    fn test_parse_version_scoped_key() {
        let overrides = json!({
            "express@^4.0.0": {
                "qs": "6.11.0"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "qs");
        assert_eq!(config.rules[0].parent_chain[0].name, "express");
        assert_eq!(
            config.rules[0].parent_chain[0].version_constraint,
            Some("^4.0.0".to_string())
        );
    }

    #[test]
    fn test_parse_dot_self_reference() {
        let overrides = json!({
            ".": {
                "lodash": "4.17.21"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "lodash");
        assert_eq!(config.rules[0].parent_chain[0].name, ".");
    }

    #[test]
    fn test_parse_dot_string_value_overrides_parent() {
        // "foo": { ".": "1.0.0" } -> override foo itself to 1.0.0
        let overrides = json!({
            "foo": {
                ".": "1.0.0"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "foo");
        assert!(config.rules[0].parent_chain.is_empty()); // Global override for foo
        assert_eq!(config.rules[0].override_version, "1.0.0");
    }

    #[test]
    fn test_parse_dot_string_value_with_sibling() {
        // "foo": { ".": "1.0.0", "bar": "2.0.0" }
        // -> override foo itself to 1.0.0 AND bar under foo to 2.0.0
        let overrides = json!({
            "foo": {
                ".": "1.0.0",
                "bar": "2.0.0"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 2);

        // Find the foo rule (global override)
        let foo_rule = config
            .rules
            .iter()
            .find(|r| r.target_package == "foo")
            .unwrap();
        assert!(foo_rule.parent_chain.is_empty());
        assert_eq!(foo_rule.override_version, "1.0.0");

        // Find the bar rule (under foo)
        let bar_rule = config
            .rules
            .iter()
            .find(|r| r.target_package == "bar")
            .unwrap();
        assert_eq!(bar_rule.parent_chain.len(), 1);
        assert_eq!(bar_rule.parent_chain[0].name, "foo");
        assert_eq!(bar_rule.override_version, "2.0.0");
    }

    #[test]
    fn test_parse_dot_string_value_nested() {
        // "foo": { "bar": { ".": "1.0.0" } } -> override bar under foo to 1.0.0
        let overrides = json!({
            "foo": {
                "bar": {
                    ".": "1.0.0"
                }
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "bar");
        assert_eq!(config.rules[0].parent_chain.len(), 1);
        assert_eq!(config.rules[0].parent_chain[0].name, "foo");
        assert_eq!(config.rules[0].override_version, "1.0.0");
    }

    #[test]
    fn test_parse_dot_string_value_at_top_level_ignored() {
        // ".": "1.0.0" at top level doesn't make sense - should be skipped
        let overrides = json!({
            ".": "1.0.0"
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert!(config.rules.is_empty());
    }

    #[test]
    fn test_parse_deep_nested_override() {
        let overrides = json!({
            "foo": {
                "bar": {
                    "baz": "1.0.0"
                }
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "baz");
        assert_eq!(config.rules[0].parent_chain.len(), 2);
        assert_eq!(config.rules[0].parent_chain[0].name, "foo");
        assert_eq!(config.rules[0].parent_chain[1].name, "bar");
        assert_eq!(config.rules[0].override_version, "1.0.0");
    }

    #[test]
    fn test_parse_reference_syntax() {
        let overrides = json!({
            "react": "$react"
        });

        let root_deps = make_root_deps(&[("react", "^18.2.0")]);
        let config = OverridesConfig::parse(&overrides, &root_deps);

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].override_version, "^18.2.0");
    }

    #[test]
    fn test_parse_reference_missing_keeps_literal() {
        let overrides = json!({
            "react": "$react"
        });

        // No react in root deps
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].override_version, "$react"); // Kept as-is
    }

    #[test]
    fn test_parse_mixed_flat_and_nested() {
        let overrides = json!({
            "lodash": "4.17.21",
            "express": {
                "qs": "6.11.0"
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 2);

        // Nested should come first (more specific)
        assert_eq!(config.rules[0].target_package, "qs");
        assert_eq!(config.rules[0].parent_chain.len(), 1);

        assert_eq!(config.rules[1].target_package, "lodash");
        assert_eq!(config.rules[1].parent_chain.len(), 0);
    }

    #[test]
    fn test_parse_sorts_by_specificity() {
        let overrides = json!({
            "qs": "6.10.0",                    // global (least specific)
            "express": {
                "qs": "6.11.0"                // under express (more specific)
            },
            "express": {
                "body-parser": {
                    "qs": "6.12.0"            // under express/body-parser (most specific)
                }
            }
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Rules should be sorted: deep nested first, then single nested, then global
        // Note: JSON object key collision means only the last "express" key survives
        // In a real scenario, nested paths would be different packages
        let depths: Vec<usize> = config.rules.iter().map(|r| r.parent_chain.len()).collect();

        // Verify descending order (most specific first)
        for i in 1..depths.len() {
            assert!(
                depths[i - 1] >= depths[i],
                "Rules should be sorted by specificity"
            );
        }
    }

    #[test]
    fn test_parse_version_constraint_more_specific() {
        let overrides = json!({
            "express": { "qs": "6.10.0" },           // no version constraint
            "express@^4.0.0": { "qs": "6.11.0" }     // with version constraint
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 2);
        // Rule with version constraint should come first
        assert!(config.rules[0].parent_chain[0].version_constraint.is_some());
    }

    #[test]
    fn test_parse_skips_invalid_values() {
        let overrides = json!({
            "valid": "1.0.0",
            "invalid_null": null,
            "invalid_number": 123,
            "invalid_bool": true,
            "invalid_array": ["1.0.0"]
        });

        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].target_package, "valid");
    }

    #[test]
    fn test_parse_empty_object() {
        let overrides = json!({});
        let config = OverridesConfig::parse(&overrides, &empty_deps());
        assert!(config.rules.is_empty());
    }

    #[test]
    fn test_parse_non_object_value() {
        let overrides = json!("not an object");
        let config = OverridesConfig::parse(&overrides, &empty_deps());
        assert!(config.rules.is_empty());
    }

    // ========== Matching tests ==========

    #[test]
    fn test_find_global_override() {
        let overrides = json!({
            "lodash": "4.17.21"
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should match anywhere
        assert_eq!(config.find_override("lodash", &[]), Some("4.17.21"));
        assert_eq!(
            config.find_override("lodash", &[pkg("express", "4.18.2")]),
            Some("4.17.21")
        );
        assert_eq!(
            config.find_override(
                "lodash",
                &[pkg("express", "4.18.2"), pkg("body-parser", "1.20.0")]
            ),
            Some("4.17.21")
        );
    }

    #[test]
    fn test_find_no_match() {
        let overrides = json!({
            "lodash": "4.17.21"
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.find_override("express", &[]), None);
        assert_eq!(
            config.find_override("qs", &[pkg("express", "4.18.2")]),
            None
        );
    }

    #[test]
    fn test_find_nested_override_matches_path() {
        let overrides = json!({
            "express": {
                "qs": "6.11.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should match qs under express
        assert_eq!(
            config.find_override("qs", &[pkg("express", "4.18.2")]),
            Some("6.11.0")
        );

        // Should NOT match qs at root
        assert_eq!(config.find_override("qs", &[]), None);

        // Should NOT match qs under different package
        assert_eq!(
            config.find_override("qs", &[pkg("body-parser", "1.20.0")]),
            None
        );
    }

    #[test]
    fn test_find_version_scoped_override() {
        let overrides = json!({
            "express@^4.0.0": {
                "qs": "6.11.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should match qs under express@4.x
        assert_eq!(
            config.find_override("qs", &[pkg("express", "4.18.2")]),
            Some("6.11.0")
        );

        // Should NOT match qs under express@5.x
        assert_eq!(config.find_override("qs", &[pkg("express", "5.0.0")]), None);

        // Should NOT match qs under express@3.x
        assert_eq!(config.find_override("qs", &[pkg("express", "3.0.0")]), None);
    }

    #[test]
    fn test_find_dot_root_only_override() {
        let overrides = json!({
            ".": {
                "lodash": "4.17.21"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should match lodash as direct dep (empty resolution path)
        assert_eq!(config.find_override("lodash", &[]), Some("4.17.21"));

        // Should NOT match lodash as transitive dep
        assert_eq!(
            config.find_override("lodash", &[pkg("express", "4.18.2")]),
            None
        );
    }

    #[test]
    fn test_find_dot_self_reference_override() {
        // "foo": { ".": "1.0.0" } -> override foo itself everywhere
        let overrides = json!({
            "foo": {
                ".": "1.0.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should match foo as direct dep
        assert_eq!(config.find_override("foo", &[]), Some("1.0.0"));

        // Should also match foo as transitive dep
        assert_eq!(
            config.find_override("foo", &[pkg("express", "4.18.2")]),
            Some("1.0.0")
        );
    }

    #[test]
    fn test_find_dot_self_reference_with_child() {
        // "foo": { ".": "1.0.0", "bar": "2.0.0" }
        let overrides = json!({
            "foo": {
                ".": "1.0.0",
                "bar": "2.0.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // foo itself should be overridden everywhere
        assert_eq!(config.find_override("foo", &[]), Some("1.0.0"));
        assert_eq!(
            config.find_override("foo", &[pkg("some-pkg", "1.0.0")]),
            Some("1.0.0")
        );

        // bar under foo should be overridden
        assert_eq!(
            config.find_override("bar", &[pkg("foo", "1.0.0")]),
            Some("2.0.0")
        );

        // bar NOT under foo should NOT be overridden
        assert_eq!(config.find_override("bar", &[]), None);
        assert_eq!(config.find_override("bar", &[pkg("other", "1.0.0")]), None);
    }

    #[test]
    fn test_find_deep_nested_override() {
        let overrides = json!({
            "foo": {
                "bar": {
                    "baz": "1.0.0"
                }
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Exact path match
        assert_eq!(
            config.find_override("baz", &[pkg("foo", "1.0.0"), pkg("bar", "2.0.0")]),
            Some("1.0.0")
        );

        // Path ends with the chain (deeper in tree)
        assert_eq!(
            config.find_override(
                "baz",
                &[
                    pkg("root", "0.0.0"),
                    pkg("foo", "1.0.0"),
                    pkg("bar", "2.0.0")
                ]
            ),
            Some("1.0.0")
        );

        // Should NOT match with partial chain
        assert_eq!(config.find_override("baz", &[pkg("bar", "2.0.0")]), None);
        assert_eq!(config.find_override("baz", &[pkg("foo", "1.0.0")]), None);
    }

    #[test]
    fn test_find_most_specific_wins() {
        let overrides = json!({
            "qs": "6.10.0",
            "express": {
                "qs": "6.11.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Under express: should use nested (more specific) rule
        assert_eq!(
            config.find_override("qs", &[pkg("express", "4.18.2")]),
            Some("6.11.0")
        );

        // At root: should use global rule
        assert_eq!(config.find_override("qs", &[]), Some("6.10.0"));

        // Under different package: should use global rule
        assert_eq!(
            config.find_override("qs", &[pkg("body-parser", "1.20.0")]),
            Some("6.10.0")
        );
    }

    #[test]
    fn test_find_version_constraint_more_specific_than_none() {
        let overrides = json!({
            "express": { "qs": "6.10.0" },
            "express@^4.0.0": { "qs": "6.11.0" }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // express@4.x should match the version-constrained rule
        assert_eq!(
            config.find_override("qs", &[pkg("express", "4.18.2")]),
            Some("6.11.0")
        );

        // express@5.x should fall back to non-constrained rule
        assert_eq!(
            config.find_override("qs", &[pkg("express", "5.0.0")]),
            Some("6.10.0")
        );
    }

    #[test]
    fn test_find_scoped_package() {
        let overrides = json!({
            "@types/node": "^18.0.0"
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        assert_eq!(config.find_override("@types/node", &[]), Some("^18.0.0"));
    }

    #[test]
    fn test_is_empty() {
        let empty = OverridesConfig::default();
        assert!(empty.is_empty());

        let non_empty = OverridesConfig::parse(&json!({"foo": "1.0.0"}), &empty_deps());
        assert!(!non_empty.is_empty());
    }

    // ========== Applied tracking tests ==========

    #[test]
    fn test_applied_tracking() {
        let overrides = json!({
            "lodash": "4.17.21",
            "nonexistent": "1.0.0"
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Use lodash override
        config.find_override("lodash", &[]);

        // Check warnings
        let warnings = config.get_unapplied_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonexistent"));
    }

    #[test]
    fn test_reset_applied() {
        let overrides = json!({
            "lodash": "4.17.21"
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        config.find_override("lodash", &[]);
        assert!(config.get_unapplied_warnings().is_empty());

        config.reset_applied();
        assert_eq!(config.get_unapplied_warnings().len(), 1);
    }

    // ========== Legacy compatibility ==========

    #[test]
    fn test_find_override_by_names() {
        let overrides = json!({
            "express": {
                "qs": "6.11.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Should work with just names
        assert_eq!(
            config.find_override_by_names("qs", &["express".to_string()]),
            Some("6.11.0")
        );
    }

    #[test]
    fn test_find_override_by_names_no_version_constraint_match() {
        let overrides = json!({
            "express@^4.0.0": {
                "qs": "6.11.0"
            }
        });
        let config = OverridesConfig::parse(&overrides, &empty_deps());

        // Without version info, version-constrained rules won't match
        // (empty version won't satisfy ^4.0.0)
        assert_eq!(
            config.find_override_by_names("qs", &["express".to_string()]),
            None
        );
    }
}
