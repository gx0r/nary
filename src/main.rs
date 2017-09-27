#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;
// #[macro_use] extern crate lazy_static;
extern crate serde_json;
extern crate hyper;
extern crate hyper_native_tls;
extern crate tar;
extern crate flate2;
extern crate semver;
extern crate indicatif;
extern crate percent_encoding;

use serde_json::Value;
use std::fs::File;
use tar::Archive;
use std::fs;
use hyper::Client;
use hyper::net::HttpsConnector;
use hyper_native_tls::NativeTlsClient;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::path::Path;
use std::sync::Arc;
use semver::{Version, VersionReq};
use indicatif::ProgressBar;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::create_dir_all;
use std::env::home_dir;
use hyper::header::{Headers, UserAgent, Encoding, AcceptEncoding, qitem};
use hyper::Url;
use percent_encoding::utf8_percent_encode;

mod errors {
    // Create the Error, ErrorKind, ResultExt, and Result types
    error_chain!{}
}
use errors::*;

// lazy_static! {
//     static ref PROGRESS_BAR: Arc<ProgressBar> = {
//         let m = Arc::new(ProgressBar::new(100));
//         m
//     };
// }

fn main() {
    if let Err(ref e) = install(&Path::new("."), false) {
        use std::io::Write;
        use error_chain::ChainedError; // trait which holds `display`
        let stderr = &mut ::std::io::stderr();
        let errmsg = "Error writing to stderr";

        writeln!(stderr, "{}", e.display()).expect(errmsg);
        ::std::process::exit(1);
    }
}

fn install(root_path: &Path, install_dev_dependencies: bool) -> Result<()> {
    let _ = fs::create_dir("node_modules");
    let installed_deps: HashMap<String, semver::Version> = HashMap::new();

    return install_helper(root_path, install_dev_dependencies, &installed_deps);
}

fn install_helper(root_path: &Path,
                  install_dev_dependencies: bool,
                  installed_deps: &HashMap<String, semver::Version>)
                  -> Result<()> {

    let mut package = root_path.to_path_buf();
    package.push("package.json");

    let mut package_json = File::open(package).chain_err(|| "Failed to open package.json.")?;

    let mut contents = String::new();
    package_json.read_to_string(&mut contents).chain_err(|| "Failed to read package.json.")?;

    let v: Value =
        serde_json::from_str(&contents).chain_err(|| "Failed to deserialize package.json.")?;

    if let Some(deps) = v["dependencies"].as_object() {
        install_deps(root_path, deps, &installed_deps).chain_err(|| "Failed to install a dependency.")?;
    }

    if install_dev_dependencies {
        if let Some(dev_deps) = v["devDependencies"].as_object() {
            install_deps(root_path, dev_deps, &installed_deps).chain_err(|| "Failed to install a dev dependency.")?;
        }
    }

    Ok(())
}

fn install_deps(root_path: &Path,
                deps: &serde_json::Map<String, serde_json::Value>,
                installed_deps: &HashMap<String, semver::Version>)
                -> Result<()> {
    let ssl = NativeTlsClient::new().chain_err(|| "Unable to create a NativeTlsClient")?;
    let connector = HttpsConnector::new(ssl);

    let client = Client::with_connector(connector);

    let mut next_paths: HashSet<PathBuf> = HashSet::new();
    let mut installed_deps = installed_deps.clone();

    let mut cache_dir = home_dir().ok_or_else(|| "Needs a HOME directory.")?;
    cache_dir.push(".nary_cache");
    create_dir_all(&cache_dir).chain_err(|| "Couldn't create cache")?;

    // https://docs.serde.rs/serde_json/map/struct.Iter.html
    for (key, vers) in deps.iter() {
        // println!("key: {:?} version: {:?}", key, vers);

        if let Some(version) = vers.as_str() {
            let required_version = VersionReq::parse(version).chain_err(|| format!("Version {} of {} didn't parse", version, key))?;
            match installed_deps.get(key) {
                Some(installed_version) => {
                    if required_version.matches(installed_version) {
                        println!("Already have {} @ {}; don't need to install {}",
                                 key,
                                 installed_version,
                                 required_version);
                        continue;
                    }
                }
                None => (),
            }
            let url = format!("{}{}", "https://registry.npmjs.org/", utf8_percent_encode(key,
               percent_encoding::PATH_SEGMENT_ENCODE_SET));

            // println!("{}", &url);

            let mut body = String::new();

            client.get(&url)
                .send()
                .chain_err(|| format!("Couldn't GET URL: {}", url))?
                .read_to_string(&mut body)
                .chain_err(|| format!("Couldn't ready body of: {}", url))?;

            // println!("{}", &body);

            let metadata: Value = serde_json::from_str(&body).chain_err(|| format!("Couldn't JSON parse metadata from {}", url))?;
            let versions = &metadata["versions"].as_object()
                .ok_or_else(|| format!("Versions was not a JSON object. {}", url))?;

            for version in versions.iter().rev() {
                if required_version.matches(&Version::parse(version.0.as_str()).chain_err(|| format!("{} didn't parse", version.0))?) {
                    // let version = &versions[version];
                    // println!("Version: \n{:?}", version);

                    // PROGRESS_BAR.inc(1);

                    let dist = &version.1["dist"];
                    // let dis = version.1;

                    // println!("Dist: {}", dist);
                    let tarball_url = Url::parse(&dist["tarball"].as_str().ok_or_else(|| format!("tarball URL didn't convert to string"))?)
                        .chain_err(|| "Couldn't parse URL")?;
                    // let url = Url::parse(tarball_url);
                    // println!("Tarball URL: {:?}", &tarball_url);

                    let mut tarball_res = Vec::new();
                    { // cache

                        let mut path = cache_dir.clone();
                        path.push(&utf8_percent_encode(key,
                            percent_encoding::PATH_SEGMENT_ENCODE_SET).to_string());
                        let _ = fs::create_dir(&path);
                        path.push(&version.0);
                        let _ = fs::create_dir(&path);
                        path.push("package.tgz");

                        let cache_file = File::open(&path);

                        if cache_file.is_ok() {
                            cache_file.ok().unwrap().read_to_end(&mut tarball_res).chain_err(|| "Couldn't cache file")?;
                            println!("Read {} from cache", path.to_string_lossy());
                        } else {
                            client.get(tarball_url.clone())
                                // .header(AcceptEncoding(vec![qitem(Encoding::Gzip)]))
                                .send()
                                .chain_err(|| format!("Couldn't get tarball: {:?}", &tarball_url))?
                                .read_to_end(&mut tarball_res)
                                .chain_err(|| format!("Couldn't read to string tarball: {:?}", &tarball_url))?;

                            // client.get(&*url).send().chain_err(|| format!("Couldn't GET URL: {}", url))?.read_to_string(&mut body)
                            // .chain_err(|| format!("Couldn't ready body of: {}", url))?;

                            let mut cache_file = File::create(&path).chain_err(|| "Coudln't cache file")?;
                            println!("Caching {}", path.to_string_lossy());
                            cache_file.write(tarball_res.as_slice()).chain_err(|| "Couldn't write to cache file")?;
                        }
                    }

                    // let mut ball = Vec::<u8>::new();
                    // let _ = tarball_res.read_to_end(&mut ball).chain_err(|| format!("Couldn't read to end of {}", tarball_url))?;

                    use flate2::read::GzDecoder;
                    let mut d = GzDecoder::new(tarball_res.as_slice())
                        .chain_err(|| format!("Couldn't gunzip {}", tarball_url))?;
                    let mut vec = Vec::new();
                    let _ = d.read_to_end(&mut vec).chain_err(|| format!("Couldn't 2nd read to end of {}", tarball_url))?;

                    let mut a = Archive::new(vec.as_slice());

                    let mut path = root_path.to_path_buf();
                    path.push("node_modules");
                    path.push(key);

                    for (key, entry) in a.entries().chain_err(|| format!("{} didn't provide file entries", tarball_url))?.enumerate() {
                        // Make sure there wasn't an I/O error

                        if entry.is_ok() {
                            let mut entry = entry.ok().unwrap();
                            // Inspect metadata about the file
                            // println!("{:?}", entry.header().path().unwrap());
                            // println!("{}", entry.header().size().unwrap());

                            let mut entry_header = entry.header().path().chain_err(|| format!("Tarball {} had a bad entry path: {}", tarball_url, key))?.into_owned();

                            if entry_header.is_absolute() {
                                bail!("{:?} is absolute from {}", entry_header, tarball_url);
                            }

                            if entry_header.strip_prefix("package/").is_ok() {
                                entry_header = entry_header.strip_prefix("package/")
                                    .chain_err(|| format!("Tarball {} had no package/ prefix for {}", tarball_url, key))?
                                    .to_path_buf();
                            }

                            // println!("Entry header: {:?}", entry_header);

                            let mut file_path = path.clone();
                            file_path.push(entry_header);

                            // println!("Creating {:?}", file_path);

                            let mut dir_path = file_path.clone();
                            dir_path.pop();
                            create_dir_all(&dir_path).chain_err(|| format!("Couldn't create dir {} for {}", file_path.display(), key))?;
                            entry.unpack(&file_path).chain_err(|| format!("Couldn't unpack {} for {}", file_path.display(), key))?;
                        } else {
                            eprintln!("Tarball {} had a bad entry {}", tarball_url, key);
                            // let mut entry = entry.chain_err(|| format!("Tarball {} had a bad entry {}", tarball_url, key))?;
                        }

                    }

                    let version_to_insert = Version::parse(version.0.as_str()).chain_err(|| format!("{} didn't parse", version.0))?;
                    installed_deps.insert(key.to_string(), version_to_insert);

                    next_paths.insert(path);

                    break;
                }
            }
        } else {
            bail!(format!("A version of {} wasn't string parsable.", key));
        }
    }

    for path in next_paths {
        install_helper(&path, false, &installed_deps)?;
    }

    Ok(())
}
