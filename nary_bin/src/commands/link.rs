use nary_lib::get_global_dir;
use snafu::OptionExt;
use std::fs;
use std::path::Path;

#[cfg(not(unix))]
use crate::error::SymlinksNotSupportedSnafu;
use crate::error::{
    GlobalPackageNotFoundSnafu, MissingPackageNameSnafu, NoHomeDirectorySnafu, Result,
};
use crate::{LinkArgs, UnlinkArgs};

pub fn run_link(args: &LinkArgs) -> Result<()> {
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_modules = global_dir.join("lib/node_modules");
    let global_bin = global_dir.join("bin");

    fs::create_dir_all(&global_modules)?;
    fs::create_dir_all(&global_bin)?;

    match &args.package {
        None => {
            // Link current package globally
            let root_path = Path::new(".");
            let pkg_json_path = root_path.join("package.json");
            let content = fs::read_to_string(&pkg_json_path)?;
            let pkg: serde_json::Value = serde_json::from_str(&content)?;

            let name = pkg
                .get("name")
                .and_then(|n| n.as_str())
                .context(MissingPackageNameSnafu)?;

            let abs_path = fs::canonicalize(root_path)?;
            let link_path = global_modules.join(name);

            // Create parent for scoped packages
            if name.contains('/') {
                if let Some(parent) = link_path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }

            // Remove existing link/dir
            let _ = fs::remove_file(&link_path);
            let _ = fs::remove_dir_all(&link_path);

            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(&abs_path, &link_path)?;
            }
            #[cfg(not(unix))]
            {
                return SymlinksNotSupportedSnafu.fail();
            }

            eprintln!("Linked {} -> {}", name, abs_path.display());

            // Link binaries
            if let Some(bin) = pkg.get("bin") {
                link_binaries(name, &abs_path, bin, &global_bin)?;
            }

            Ok(())
        }
        Some(pkg_name) => {
            // Link global package to local node_modules
            let global_pkg = global_modules.join(pkg_name);
            if !global_pkg.exists() {
                return GlobalPackageNotFoundSnafu {
                    package: pkg_name.to_string(),
                }
                .fail();
            }

            let local_modules = Path::new("node_modules");
            fs::create_dir_all(local_modules)?;

            let link_path = local_modules.join(pkg_name);

            // Create parent for scoped packages
            if pkg_name.contains('/') {
                if let Some(parent) = link_path.parent() {
                    fs::create_dir_all(parent)?;
                }
            }

            let _ = fs::remove_file(&link_path);
            let _ = fs::remove_dir_all(&link_path);

            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                // Get relative path from local node_modules to global
                let target =
                    pathdiff::diff_paths(&global_pkg, local_modules).unwrap_or(global_pkg.clone());
                symlink(&target, &link_path)?;
            }
            #[cfg(not(unix))]
            {
                return SymlinksNotSupportedSnafu.fail();
            }

            eprintln!("Linked {} -> {}", pkg_name, global_pkg.display());
            Ok(())
        }
    }
}

fn link_binaries(
    pkg_name: &str,
    pkg_path: &Path,
    bin: &serde_json::Value,
    bin_dir: &Path,
) -> Result<()> {
    match bin {
        serde_json::Value::String(script) => {
            // Single binary with package name
            let bin_name = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
            create_bin_link(bin_name, pkg_path, script, bin_dir)?;
        }
        serde_json::Value::Object(map) => {
            // Multiple binaries
            for (bin_name, script_val) in map {
                if let Some(script) = script_val.as_str() {
                    create_bin_link(bin_name, pkg_path, script, bin_dir)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn create_bin_link(bin_name: &str, pkg_path: &Path, script: &str, bin_dir: &Path) -> Result<()> {
    let target = pkg_path.join(script);
    let link_path = bin_dir.join(bin_name);

    let _ = fs::remove_file(&link_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&target, &link_path)?;
        // Make executable
        use std::os::unix::fs::PermissionsExt;
        if target.exists() {
            let mut perms = fs::metadata(&target)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&target, perms)?;
        }
    }

    eprintln!("  Linked binary: {}", bin_name);
    Ok(())
}

pub fn run_unlink(args: &UnlinkArgs) -> Result<()> {
    let global_dir = get_global_dir().context(NoHomeDirectorySnafu)?;
    let global_modules = global_dir.join("lib/node_modules");
    let global_bin = global_dir.join("bin");

    match &args.package {
        None => {
            // Unlink current package from global
            let root_path = Path::new(".");
            let pkg_json_path = root_path.join("package.json");
            let content = fs::read_to_string(&pkg_json_path)?;
            let pkg: serde_json::Value = serde_json::from_str(&content)?;

            let name = pkg
                .get("name")
                .and_then(|n| n.as_str())
                .context(MissingPackageNameSnafu)?;

            let link_path = global_modules.join(name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path).or_else(|_| fs::remove_dir_all(&link_path))?;
                eprintln!("Unlinked {} from global", name);
            } else {
                eprintln!("Package {} not linked globally", name);
            }

            // Remove binaries
            if let Some(bin) = pkg.get("bin") {
                unlink_binaries(name, bin, &global_bin)?;
            }

            Ok(())
        }
        Some(pkg_name) => {
            // Unlink package from local node_modules
            let link_path = Path::new("node_modules").join(pkg_name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path).or_else(|_| fs::remove_dir_all(&link_path))?;
                eprintln!("Unlinked {} from local node_modules", pkg_name);
            } else {
                eprintln!("Package {} not found in node_modules", pkg_name);
            }
            Ok(())
        }
    }
}

fn unlink_binaries(pkg_name: &str, bin: &serde_json::Value, bin_dir: &Path) -> Result<()> {
    match bin {
        serde_json::Value::String(_) => {
            let bin_name = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
            let link_path = bin_dir.join(bin_name);
            if link_path.exists() || link_path.is_symlink() {
                fs::remove_file(&link_path)?;
                eprintln!("  Unlinked binary: {}", bin_name);
            }
        }
        serde_json::Value::Object(map) => {
            for bin_name in map.keys() {
                let link_path = bin_dir.join(bin_name);
                if link_path.exists() || link_path.is_symlink() {
                    fs::remove_file(&link_path)?;
                    eprintln!("  Unlinked binary: {}", bin_name);
                }
            }
        }
        _ => {}
    }
    Ok(())
}
