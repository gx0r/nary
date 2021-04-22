use anyhow::{Context, Result, anyhow};

use petgraph;
use petgraph::graphmap::DiGraphMap;

use hyper::{net::HttpsConnector, Client, Url};
use hyper_native_tls::NativeTlsClient;
use semver_rs::{Range, Version};
use serde_json::Value;
use std::{cmp::Ordering, collections::{HashMap, HashSet}, fs, fs::File, io::{Read, Write}, path::{Path, PathBuf}};
use tar::Archive;
// use indicatif::ProgressBar;

mod pack;
use crate::pack::{gunzip, unpack_archive};

mod cache;
pub use crate::cache::{cache, get_cache_dir, PATH_SEGMENT_ENCODE_SET};

use percent_encoding::utf8_percent_encode;

// lazy_static! {
//     static ref PROGRESS_BAR: Arc<ProgressBar> = {
//         let m = Arc::new(ProgressBar::new(100));
//         m
//     };
// }
use structopt::StructOpt;

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

fn install(root_path: &Path, install_dev_dependencies: bool) -> Result<()> {
    let _ = fs::create_dir("node_modules");
    let installed_deps: HashMap<String, Version> = HashMap::new();

    return install_helper(root_path, false, &installed_deps);
}

fn install_helper(
    root_path: &Path,
    install_dev_dependencies: bool,
    installed_deps: &HashMap<String, Version>,
) -> Result<()> {
    let mut package = root_path.to_path_buf();
    package.push("package.json");

    let mut package_json = File::open(package).context(format!(
        "Failed to open package.json of: {}",
        root_path.to_string_lossy()
    ))?;

    let mut contents = String::new();
    package_json.read_to_string(&mut contents).context(format!(
        "Failed to read package.json of: {}",
        root_path.to_string_lossy()
    ))?;

    let v: Value = serde_json::from_str(&contents).context(format!(
        "Failed to deserialize package.json of: {}",
        root_path.to_string_lossy()
    ))?;

    if let Some(deps) = v["dependencies"].as_object() {
        install_deps(root_path, deps, &installed_deps).context(format!(
            "Failed to install a dependency of: {}",
            root_path.to_string_lossy()
        ))?;
    }

    if install_dev_dependencies {
        if let Some(dev_deps) = v["devDependencies"].as_object() {
            install_deps(root_path, dev_deps, &installed_deps).context(format!(
                "Failed to install a dev dependency of: {}",
                root_path.to_string_lossy()
            ))?;
        }
    }

    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Package<'a> {
    name: &'a str,
    // version: Version,
}

impl <'a> Ord for Package<'a> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl <'a> PartialOrd for Package<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.name.cmp(&other.name))
    }
}

fn calculate_depends<'a>(deps: &'a serde_json::Map<String, serde_json::Value>) -> DiGraphMap<Package<'a>, String> {
    let mut graph: DiGraphMap<Package, String> = DiGraphMap::new();

    let root = Package {
        name: "this",
    };

    graph.add_node(root);

    for (key, vers) in deps.iter() {
        let package = Package{
            name: key,
            // version: vers,
        };
        graph.add_node(package);
        graph.add_edge(root, package, vers.to_string());
    }

    graph
}

fn install_deps(
    root_path: &Path,
    deps: &serde_json::Map<String, serde_json::Value>,
    installed_deps: &HashMap<String, Version>,
) -> Result<()> {
    let ssl = NativeTlsClient::new().context("Unable to create a NativeTlsClient")?;
    let connector = HttpsConnector::new(ssl);
    let client = Client::with_connector(connector);

    let mut next_paths: HashSet<PathBuf> = HashSet::new();
    let mut installed_deps = installed_deps.clone();

    // https://docs.serde.rs/serde_json/map/struct.Iter.html
    for (key, vers) in deps.iter() {
        println!("Installing {:?} version: {:?}", key, vers);

        if let Some(version) = vers.as_str() {
            if version.starts_with("git://") {
                use git2::Repository;
                let mut path = root_path.clone().to_path_buf();
                path.push("node_modules");
                path.push(key);

                if let Some(x) = version.rfind('#') {
                    let (repo, hash) = version.clone().split_at(x);
                    let repo_cloned = Repository::clone(repo, &path)?;
                    let mut hash = hash.clone().to_string();
                    hash.remove(0);
                    println!("hash: {}", hash);
                    let obj = repo_cloned.revparse_single(&hash)?;
                    repo_cloned.checkout_tree(&obj, None)?;
                } else {
                    Repository::clone(version, &path)?;
                }
                continue;
            }

            let required_version = Range::new(version)
                .parse()
                .with_context(|| format!("Version {} of {} didn't parse", version, key))?;
            match installed_deps.get(key) {
                Some(installed_version) => {
                    if required_version.test(installed_version) {
                        println!(
                            "Already have {} @ {}; don't need to install {}",
                            key, installed_version, version
                        );
                        continue;
                    }
                }
                None => (),
            }
            let url = format!(
                "{}{}",
                "https://registry.npmjs.org/",
                utf8_percent_encode(key, PATH_SEGMENT_ENCODE_SET)
            );

            // println!("{}", &url);

            let mut body = String::new();

            client
                .get(&url)
                .send()
                .with_context(|| format!("Couldn't GET URL: {}", url))?
                .read_to_string(&mut body)
                .with_context(|| format!("Couldn't ready body of: {}", url))?;

            // println!("{}", &body);

            let metadata: Value = serde_json::from_str(&body)
                .with_context(|| format!("Couldn't JSON parse metadata from {}", url))?;
            let versions = &metadata["versions"]
                .as_object()
                .ok_or(anyhow!("Versions was not a JSON object. {}", url))?;

            for version in versions.iter().rev() {
                if required_version.test(
                    &Version::new(version.0.as_str())
                        .parse()
                        .with_context(|| format!("{} didn't parse", version.0))?,
                ) {
                    // let version = &versions[version];
                    // println!("Version: \n{:?}", version);

                    // PROGRESS_BAR.inc(1);

                    let dist = &version.1["dist"];
                    // let dis = version.1;

                    // println!("Dist: {}", dist);
                    let tarball_url = Url::parse(
                        &dist["tarball"]
                            .as_str()
                            .ok_or(anyhow!("tarball URL didn't convert to string"))?,
                    )
                    .context("Couldn't parse URL")?;
                    // let url = Url::parse(tarball_url);
                    // println!("Tarball URL: {:?}", &tarball_url);

                    let tarball = gunzip(cache(key, &version.0, &tarball_url)?, &tarball_url)?;
                    let mut archive = Archive::new(tarball.as_slice());

                    let mut path = root_path.to_path_buf();
                    path.push("node_modules");
                    path.push(key);

                    unpack_archive(&mut archive, &path, &tarball_url)?;

                    let version_to_insert = Version::new(version.0.as_str())
                        .parse()
                        .with_context(|| format!("{} didn't parse", version.0))?;
                    installed_deps.insert(key.to_string(), version_to_insert);

                    next_paths.insert(path);

                    break;
                }
            }
        } else {
            return Err(anyhow!("A version of {} wasn't string parsable.", key));
        }
    }

    for path in next_paths {
        install_helper(&path, false, &installed_deps)?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use indoc::indoc;

    use super::*;

    #[test]
    fn it_will_ejs() {
        let input = indoc! {r###"
            {
                "private": true,
                "name": "or",
                "version": "1.0.0",
                "description": "",
                "main": "index.js",
                "scripts": {
                    "test": "echo \"Error: no test specified\" && exit 1"
                },
                "author": "",
                "license": "ISC",
                "dependencies": {
                 "koa-ejs": "^4.1.0"
                }
            }
        "###};
    }
}