use serde_json::Value;
use snafu::ResultExt;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::{
    JsonParseSnafu, Result, ScriptFailedSnafu, ScriptSignaledSnafu, ScriptSpawnSnafu,
};

/// Scripts that npm runs during install
pub const LIFECYCLE_SCRIPTS: &[&str] = &["preinstall", "install", "postinstall"];

/// Info about a single script
#[derive(Debug)]
pub struct ScriptInfo {
    pub name: String,
    pub command: String,
}

/// Audit info for a package's lifecycle scripts
#[derive(Debug)]
pub struct ScriptAudit {
    pub package: String,
    pub scripts: Vec<ScriptInfo>,
}

/// Generate a sandbox profile for macOS sandbox-exec
/// Uses default-deny model: blocks everything then explicitly allows needed operations
/// Key restrictions:
/// - Reads: allowed everywhere except sensitive home directories (ssh, aws, gnupg, etc.)
/// - Writes: only allowed to project dir, caches, and temp directories
///
/// This is public so it can be used by tests and the `sandbox-profile` CLI command.
#[cfg(target_os = "macos")]
pub fn generate_sandbox_profile(project_root: &Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let project = project_root.display();
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/private/tmp".to_string());

    format!(
        r#"(version 1)
(deny default)

; Process operations - needed for sh, node, npm, etc.
(allow process*)
(allow signal)

; File reads - allow broadly then deny sensitive paths
(allow file-read*)

; Block reading sensitive credentials in home directory
(deny file-read-data
  (subpath "{home}/.ssh")
  (subpath "{home}/.aws")
  (subpath "{home}/.gnupg")
  (subpath "{home}/.gpg")
  (subpath "{home}/.config/gh")
  (subpath "{home}/.kube")
  (subpath "{home}/.docker")
  (subpath "{home}/.terraform.d")
  (subpath "{home}/.azure")
  (subpath "{home}/.config/gcloud")
  (subpath "{home}/.password-store")
  (literal "{home}/.netrc")
  (literal "{home}/.git-credentials")
)

; File writes - ONLY to project, caches, and temp directories
; Blocks writes to ~/.ssh, ~/.aws, ~/.gnupg, ~/.bashrc, etc.
(allow file-write*
  (subpath "{project}")
  (subpath "{home}/.npm")
  (subpath "{home}/.cache")
  (subpath "/private/tmp")
  (subpath "/private/var/folders")
  (subpath "/var/folders")
  (subpath "{tmpdir}")
  (regex #"^/dev/")
)

; Network - needed for downloading binaries
(allow network*)

; System operations - required for process execution
(allow mach*)
(allow ipc*)
(allow iokit*)
(allow sysctl*)
(allow system*)
(allow pseudo-tty)
(allow user-preference*)
(allow nvram*)
(allow lsopen)
(allow distributed-notification-post)
"#,
        home = home,
        project = project,
        tmpdir = tmpdir.trim_end_matches('/'),
    )
}

/// Execute a configured Command and map errors to script-specific errors
fn execute_script(
    mut cmd: Command,
    pkg_path: &Path,
    new_path: &str,
    pkg_name: &str,
    script_name: &str,
) -> Result<()> {
    cmd.current_dir(pkg_path)
        .env("PATH", new_path)
        .env("npm_lifecycle_event", script_name)
        .env("npm_package_name", pkg_name);

    let output = cmd.output().context(ScriptSpawnSnafu {
        package: pkg_name.to_string(),
        script: script_name.to_string(),
    })?;

    if !output.status.success() {
        if let Some(code) = output.status.code() {
            return ScriptFailedSnafu {
                package: pkg_name.to_string(),
                script: script_name.to_string(),
                exit_code: code,
            }
            .fail();
        } else {
            return ScriptSignaledSnafu {
                package: pkg_name.to_string(),
                script: script_name.to_string(),
            }
            .fail();
        }
    }

    Ok(())
}

/// Run lifecycle scripts for installed packages
pub struct LifecycleRunner<'a> {
    node_modules: &'a Path,
    sandbox: bool,
}

impl<'a> LifecycleRunner<'a> {
    pub fn new(node_modules: &'a Path) -> Self {
        Self {
            node_modules,
            sandbox: true,
        }
    }

    pub fn with_sandbox(node_modules: &'a Path, sandbox: bool) -> Self {
        Self {
            node_modules,
            sandbox,
        }
    }

    /// Run a specific script from a package's package.json
    pub fn run_script(&self, pkg_name: &str, script_name: &str) -> Result<()> {
        let pkg_path = self.node_modules.join(pkg_name);
        let package_json_path = pkg_path.join("package.json");

        // Read package.json
        let content = match fs::read_to_string(&package_json_path) {
            Ok(c) => c,
            Err(_) => return Ok(()), // No package.json, nothing to run
        };

        let pkg: Value = serde_json::from_str(&content).context(JsonParseSnafu {
            source_desc: package_json_path.display().to_string(),
        })?;

        // Get scripts object
        let scripts = match pkg.get("scripts") {
            Some(Value::Object(s)) => s,
            _ => return Ok(()), // No scripts section
        };

        // Get the specific script
        let script_content = match scripts.get(script_name) {
            Some(Value::String(s)) => s,
            _ => return Ok(()), // Script not defined
        };

        // Build PATH with node_modules/.bin prepended (must be absolute)
        let bin_path = self.node_modules.join(".bin");
        let bin_path = std::fs::canonicalize(&bin_path).unwrap_or(bin_path);
        let current_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", bin_path.display(), current_path);

        // Run the script
        let pkg_path = std::fs::canonicalize(&pkg_path).unwrap_or(pkg_path);
        eprintln!("  {} {}: {}", pkg_name, script_name, script_content);

        #[cfg(unix)]
        {
            #[cfg(target_os = "macos")]
            let cmd = if self.sandbox {
                let project_root =
                    std::fs::canonicalize(self.node_modules.parent().unwrap_or(self.node_modules))
                        .unwrap_or_else(|_| {
                            self.node_modules
                                .parent()
                                .unwrap_or(self.node_modules)
                                .to_path_buf()
                        });
                let profile = generate_sandbox_profile(&project_root);
                let mut cmd = Command::new("sandbox-exec");
                cmd.arg("-p")
                    .arg(&profile)
                    .arg("sh")
                    .arg("-c")
                    .arg(script_content);
                cmd
            } else {
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(script_content);
                cmd
            };

            #[cfg(not(target_os = "macos"))]
            let cmd = {
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(script_content);
                cmd
            };

            execute_script(cmd, &pkg_path, &new_path, pkg_name, script_name)?;
        }

        #[cfg(not(unix))]
        {
            // On non-Unix, use cmd.exe
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(script_content);
            execute_script(cmd, &pkg_path, &new_path, pkg_name, script_name)?;
        }

        Ok(())
    }

    /// Run all lifecycle scripts for a package (preinstall, install, postinstall)
    pub fn run_lifecycle_scripts(&self, pkg_name: &str) -> Result<()> {
        for script in LIFECYCLE_SCRIPTS {
            self.run_script(pkg_name, script)?;
        }
        Ok(())
    }

    /// Check if a package has any lifecycle scripts
    pub fn has_lifecycle_scripts(&self, pkg_name: &str) -> bool {
        !self.get_lifecycle_scripts(pkg_name).is_empty()
    }

    /// Get all lifecycle scripts for a package
    pub fn get_lifecycle_scripts(&self, pkg_name: &str) -> Vec<ScriptInfo> {
        let pkg_path = self.node_modules.join(pkg_name);
        let package_json_path = pkg_path.join("package.json");

        let content = match fs::read_to_string(&package_json_path) {
            Ok(c) => c,
            Err(_) => return vec![],
        };

        let pkg: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return vec![],
        };

        let scripts = match pkg.get("scripts") {
            Some(Value::Object(s)) => s,
            _ => return vec![],
        };

        LIFECYCLE_SCRIPTS
            .iter()
            .filter_map(|&script_name| {
                scripts
                    .get(script_name)
                    .and_then(|v| v.as_str())
                    .map(|cmd| ScriptInfo {
                        name: script_name.to_string(),
                        command: cmd.to_string(),
                    })
            })
            .collect()
    }

    /// Audit all packages and return those with lifecycle scripts
    pub fn audit_scripts(&self, packages: &[String]) -> Vec<ScriptAudit> {
        packages
            .iter()
            .filter_map(|pkg_name| {
                let scripts = self.get_lifecycle_scripts(pkg_name);
                if scripts.is_empty() {
                    None
                } else {
                    Some(ScriptAudit {
                        package: pkg_name.clone(),
                        scripts,
                    })
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create a fake package in node_modules with optional scripts
    fn setup_package(temp: &TempDir, name: &str, scripts: Option<serde_json::Value>) -> PathBuf {
        let node_modules = temp.path().join("node_modules");
        let pkg_dir = node_modules.join(name);
        fs::create_dir_all(&pkg_dir).unwrap();

        let mut pkg_json = serde_json::json!({
            "name": name,
            "version": "1.0.0"
        });

        if let Some(s) = scripts {
            pkg_json["scripts"] = s;
        }

        fs::write(pkg_dir.join("package.json"), pkg_json.to_string()).unwrap();
        node_modules
    }

    #[test]
    fn test_get_lifecycle_scripts_none() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(&temp, "no-scripts", None);

        let runner = LifecycleRunner::new(&node_modules);
        let scripts = runner.get_lifecycle_scripts("no-scripts");

        assert!(scripts.is_empty());
    }

    #[test]
    fn test_get_lifecycle_scripts_empty_scripts_object() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(&temp, "empty-scripts", Some(serde_json::json!({})));

        let runner = LifecycleRunner::new(&node_modules);
        let scripts = runner.get_lifecycle_scripts("empty-scripts");

        assert!(scripts.is_empty());
    }

    #[test]
    fn test_get_lifecycle_scripts_all_three() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(
            &temp,
            "all-scripts",
            Some(serde_json::json!({
                "preinstall": "echo pre",
                "install": "node-gyp rebuild",
                "postinstall": "echo post"
            })),
        );

        let runner = LifecycleRunner::new(&node_modules);
        let scripts = runner.get_lifecycle_scripts("all-scripts");

        assert_eq!(scripts.len(), 3);
        assert!(scripts.iter().any(|s| s.name == "preinstall"));
        assert!(scripts.iter().any(|s| s.name == "install"));
        assert!(scripts.iter().any(|s| s.name == "postinstall"));
    }

    #[test]
    fn test_get_lifecycle_scripts_partial() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(
            &temp,
            "partial",
            Some(serde_json::json!({
                "postinstall": "node scripts/postinstall.js",
                "test": "jest" // Not a lifecycle script
            })),
        );

        let runner = LifecycleRunner::new(&node_modules);
        let scripts = runner.get_lifecycle_scripts("partial");

        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].name, "postinstall");
        assert_eq!(scripts[0].command, "node scripts/postinstall.js");
    }

    #[test]
    fn test_has_lifecycle_scripts_true() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(
            &temp,
            "has-scripts",
            Some(serde_json::json!({"install": "make"})),
        );

        let runner = LifecycleRunner::new(&node_modules);
        assert!(runner.has_lifecycle_scripts("has-scripts"));
    }

    #[test]
    fn test_has_lifecycle_scripts_false() {
        let temp = TempDir::new().unwrap();
        let node_modules = setup_package(
            &temp,
            "no-lifecycle",
            Some(serde_json::json!({
                "test": "jest",
                "build": "tsc",
                "start": "node index.js"
            })),
        );

        let runner = LifecycleRunner::new(&node_modules);
        assert!(!runner.has_lifecycle_scripts("no-lifecycle"));
    }

    #[test]
    fn test_has_lifecycle_scripts_missing_package() {
        let temp = TempDir::new().unwrap();
        let node_modules = temp.path().join("node_modules");
        fs::create_dir_all(&node_modules).unwrap();

        let runner = LifecycleRunner::new(&node_modules);
        assert!(!runner.has_lifecycle_scripts("nonexistent"));
    }

    #[test]
    fn test_audit_scripts_multiple_packages() {
        let temp = TempDir::new().unwrap();

        // Package with scripts
        setup_package(
            &temp,
            "pkg-a",
            Some(serde_json::json!({"postinstall": "echo done"})),
        );

        // Package without lifecycle scripts
        setup_package(&temp, "pkg-b", Some(serde_json::json!({"test": "jest"})));

        // Another package with scripts
        setup_package(
            &temp,
            "pkg-c",
            Some(serde_json::json!({"install": "node-gyp rebuild"})),
        );

        let node_modules = temp.path().join("node_modules");
        let runner = LifecycleRunner::new(&node_modules);

        let packages = vec![
            "pkg-a".to_string(),
            "pkg-b".to_string(),
            "pkg-c".to_string(),
        ];
        let audits = runner.audit_scripts(&packages);

        // Should only include pkg-a and pkg-c
        assert_eq!(audits.len(), 2);
        assert!(audits.iter().any(|a| a.package == "pkg-a"));
        assert!(audits.iter().any(|a| a.package == "pkg-c"));
        assert!(!audits.iter().any(|a| a.package == "pkg-b"));
    }

    #[test]
    fn test_audit_scripts_empty_list() {
        let temp = TempDir::new().unwrap();
        let node_modules = temp.path().join("node_modules");
        fs::create_dir_all(&node_modules).unwrap();

        let runner = LifecycleRunner::new(&node_modules);
        let audits = runner.audit_scripts(&[]);

        assert!(audits.is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_generate_sandbox_profile_contains_project_path() {
        let temp = TempDir::new().unwrap();
        let profile = generate_sandbox_profile(temp.path());

        // Should contain the project path for write access
        assert!(profile.contains(&temp.path().display().to_string()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_generate_sandbox_profile_blocks_sensitive() {
        let temp = TempDir::new().unwrap();
        let profile = generate_sandbox_profile(temp.path());

        // Should block reading sensitive directories
        assert!(profile.contains(".ssh"));
        assert!(profile.contains(".aws"));
        assert!(profile.contains(".gnupg"));
        assert!(profile.contains(".kube"));
        assert!(profile.contains(".netrc"));
        assert!(profile.contains(".git-credentials"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_generate_sandbox_profile_allows_npm_cache() {
        let temp = TempDir::new().unwrap();
        let profile = generate_sandbox_profile(temp.path());

        // Should allow writing to npm cache
        assert!(profile.contains(".npm"));
        assert!(profile.contains(".cache"));
    }
}
