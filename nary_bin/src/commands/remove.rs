use std::fs;
use std::path::Path;

use crate::error::Result;
use crate::RemoveArgs;

pub async fn run_remove(args: &RemoveArgs) -> Result<()> {
    let root_path = Path::new(".");
    let pkg_json_path = root_path.join("package.json");

    let content = fs::read_to_string(&pkg_json_path)?;
    let mut pkg: serde_json::Value = serde_json::from_str(&content)?;

    for name in &args.packages {
        let mut found = false;

        for section in ["dependencies", "devDependencies", "optionalDependencies"] {
            if let Some(deps) = pkg.get_mut(section).and_then(|d| d.as_object_mut()) {
                if deps.remove(name).is_some() {
                    eprintln!("Removed {} from {}", name, section);
                    found = true;
                }
            }
        }

        if !found {
            eprintln!("warn: {} not found in package.json", name);
        }

        // Remove from node_modules
        let pkg_path = root_path.join("node_modules").join(name);
        if pkg_path.exists() {
            fs::remove_dir_all(&pkg_path)?;
            eprintln!("Removed {}", pkg_path.display());
        }
    }

    // Write back package.json
    let output = serde_json::to_string_pretty(&pkg)?;
    fs::write(&pkg_json_path, output + "\n")?;

    Ok(())
}
