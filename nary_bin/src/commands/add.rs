use snafu::OptionExt;
use std::fs;
use std::path::Path;

use nary_lib::{create_client, parse_package_spec};

use crate::error::{NoLatestVersionSnafu, Result};
use crate::{commands, AddArgs, InstallArgs};

pub async fn run_add(args: &AddArgs) -> Result<()> {
    let client = create_client()?;
    let root_path = Path::new(".");

    for pkg_spec in &args.packages {
        // Parse package@version
        let (name, version) = parse_package_spec(pkg_spec);

        // Resolve version if not specified
        let resolved_version = match version {
            Some(v) => v,
            None => {
                // Fetch latest version from registry
                let dep = nary_lib::Dependency {
                    name: name.clone(),
                    requested: "*".to_string(),
                    resolved: String::new(),
                    is_optional: false,
                    alias: None,
                };
                let metadata = nary_lib::fetch_package_root_metadata(&client, &dep).await?;
                let latest =
                    metadata["dist-tags"]["latest"]
                        .as_str()
                        .context(NoLatestVersionSnafu {
                            package: name.clone(),
                        })?;
                if args.exact {
                    latest.to_string()
                } else {
                    format!("^{}", latest)
                }
            }
        };

        // Read and update package.json
        let pkg_json_path = root_path.join("package.json");
        let content = fs::read_to_string(&pkg_json_path)?;
        let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

        let section = if args.dev {
            "devDependencies"
        } else {
            "dependencies"
        };

        if pkg.get(section).is_none() {
            pkg[section] = serde_json::json!({});
        }
        pkg[section][&name] = serde_json::Value::String(resolved_version.clone());

        // Write back with 2-space indent
        let output = serde_json::to_string_pretty(&pkg)?;
        fs::write(&pkg_json_path, output + "\n")?;

        eprintln!("Added {}@{} to {}", name, resolved_version, section);
    }

    // Remove package-lock.json so install re-resolves (safe to ignore if doesn't exist)
    let _ = fs::remove_file(root_path.join("package-lock.json"));

    // Run install
    commands::run_install(&InstallArgs::default()).await
}
