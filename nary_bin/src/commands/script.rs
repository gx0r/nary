use snafu::OptionExt;
use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::{Result, ScriptNotFoundSnafu};
use crate::RunArgs;

pub fn run_script(args: &RunArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;

    let script_cmd = pkg
        .get("scripts")
        .and_then(|s| s.get(&args.script))
        .and_then(|v| v.as_str())
        .context(ScriptNotFoundSnafu {
            script: args.script.clone(),
        })?;

    // Build command with extra args
    let full_cmd = if args.args.is_empty() {
        script_cmd.to_string()
    } else {
        format!("{} {}", script_cmd, args.args.join(" "))
    };

    // Build PATH with node_modules/.bin prepended
    let bin_path =
        fs::canonicalize("node_modules/.bin").unwrap_or_else(|_| "node_modules/.bin".into());
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_path.display(), current_path);

    // Get package name for env vars
    let pkg_name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let pkg_version = pkg.get("version").and_then(|v| v.as_str()).unwrap_or("");

    eprintln!("> {}", full_cmd);

    let status = Command::new("sh")
        .arg("-c")
        .arg(&full_cmd)
        .env("PATH", &new_path)
        .env("npm_lifecycle_event", &args.script)
        .env("npm_package_name", pkg_name)
        .env("npm_package_version", pkg_version)
        .status()?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

pub fn run_binary(
    bin_path: &Path,
    args: &[String],
    sandbox: bool,
    project_root: &Path,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(bin_path, fs::Permissions::from_mode(0o755));
    }

    #[cfg(target_os = "macos")]
    let status = if sandbox {
        let profile = nary_lib::generate_sandbox_profile(project_root);
        Command::new("sandbox-exec")
            .arg("-p")
            .arg(&profile)
            .arg(bin_path)
            .args(args)
            .status()?
    } else {
        Command::new(bin_path).args(args).status()?
    };

    #[cfg(not(target_os = "macos"))]
    let status = {
        let _ = (sandbox, project_root); // Suppress unused warnings
        Command::new(bin_path).args(args).status()?
    };

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}
