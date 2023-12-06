use anyhow::Result;
use std::fs;

use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use structopt::StructOpt;

use nary_lib::{calculate_depends, install_dep, path_to_dependencies, path_to_root_dependency};

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
    let install_dev_dependencies = !opt.production;

    install(&Path::new("."), !install_dev_dependencies)
}

fn install(root_path: &Path, _install_dev_dependencies: bool) -> Result<()> {
    let _ = fs::create_dir("node_modules");
    let dependencies = path_to_dependencies(&root_path)?;
    let root = path_to_root_dependency(&root_path)?;
    let depends = calculate_depends(&root, &dependencies)?;

    let pb = ProgressBar::new(depends.iter().len() as u64);

    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
            .unwrap(),
    );

    for dep in depends.iter() {
        pb.inc(1);

        let name = dep.0.name.to_string();
        let ver = dep.0.version.to_string();
        pb.set_message(format!("{}@{}", name, ver));

        install_dep(&Path::new(&"./node_modules".to_string()), &dep.0)?;
    }
    pb.finish_and_clear();

    Ok(())
}
