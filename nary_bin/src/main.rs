use anyhow::{Result};
use std::{fs};

use std::{
    path::{Path},
};

use structopt::StructOpt;

use nary_lib::{calculate_depends, path_to_dependencies, path_to_root_dependency, install_dep};

/// nary
#[derive(StructOpt, Debug)]
#[structopt(name = "basic")]
struct Opt {
    /// Verbose mode (-v, -vv, -vvv, etc.)
    #[structopt(short, long, parse(from_occurrences))]
    verbose: u8,

    /// Don't install any dev dependencies
    #[structopt(long = "prod")]
    production: bool,
}

fn main() -> Result<()> {
    let opt = Opt::from_args();
    // println!("{:#?}", opt);
    let install_dev_dependencies = !opt.production;

    install(&Path::new("."), !install_dev_dependencies)
}

fn install(root_path: &Path, _install_dev_dependencies: bool) -> Result<()> {
    let _ = fs::create_dir("node_modules");
    let dependencies = path_to_dependencies(&root_path)?;
    let root = path_to_root_dependency(&root_path)?;
    let depends = calculate_depends(&root, &dependencies)?;

    for dep in depends {
        install_dep(&Path::new(&"./node_modules".to_string()), &dep.0)?;
    }

    Ok(())
}