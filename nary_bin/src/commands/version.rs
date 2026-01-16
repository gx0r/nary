use nary_lib::bump_version;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::Result;
use crate::{RunArgs, VersionArgs};

use super::run_script;

pub fn run_version(args: &VersionArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");
    let content = fs::read_to_string(&pkg_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    let current = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0");

    let new_version = bump_version(current, &args.bump, args.preid.as_deref())?;

    // Run preversion script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("preversion") {
            eprintln!("Running preversion script...");
            let _ = run_script(&RunArgs {
                script: "preversion".to_string(),
                args: vec![],
            });
        }
    }

    // Update package.json
    pkg["version"] = serde_json::Value::String(new_version.clone());
    let output = serde_json::to_string_pretty(&pkg)?;
    fs::write(&pkg_json_path, output + "\n")?;

    eprintln!("v{}", new_version);

    // Run version script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("version") {
            eprintln!("Running version script...");
            let _ = run_script(&RunArgs {
                script: "version".to_string(),
                args: vec![],
            });
        }
    }

    // Git operations (unless --no-git-tag-version)
    if !args.no_git_tag_version {
        // Check if we're in a git repo
        let git_check = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .output();

        if let Ok(output) = git_check {
            if output.status.success() {
                // Stage package.json
                let _ = Command::new("git").args(["add", "package.json"]).status();

                // Commit
                let commit_msg = format!("v{}", new_version);
                let _ = Command::new("git")
                    .args(["commit", "-m", &commit_msg])
                    .status();

                // Tag
                let tag = format!("v{}", new_version);
                let _ = Command::new("git").args(["tag", &tag]).status();

                eprintln!("Created git commit and tag: {}", tag);
            }
        }
    }

    // Run postversion script if exists
    if let Some(scripts) = pkg.get("scripts").and_then(|s| s.as_object()) {
        if scripts.contains_key("postversion") {
            eprintln!("Running postversion script...");
            let _ = run_script(&RunArgs {
                script: "postversion".to_string(),
                args: vec![],
            });
        }
    }

    Ok(())
}
