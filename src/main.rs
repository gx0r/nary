use failure::{
    Error,
    ResultExt,
    format_err,
};
use failure_derive::Fail;
use serde_json::Value;
use std::{
    collections::{
        HashMap,
        HashSet,
    },
    fs,
    fs::{
        create_dir_all,
        File,
    },
    io::{
        Read,
        Write,
    },
    path::{
        PathBuf,
        Path,
    }
};
use tar::Archive;
use hyper::{
    Client,
    net::HttpsConnector,
    Url,
};
use hyper_native_tls::NativeTlsClient;
use semver_rs::{Version, Range};
// use indicatif::ProgressBar;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
// https://url.spec.whatwg.org/#path-percent-encode-set
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'<').add(b'>').add(b'`')
    .add(b'#').add(b'?').add(b'{').add(b'}');

#[derive(Fail, Debug)]
#[fail(display = "Needs a Home Directory")]
pub struct NeedHomeDir;

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

fn main() {
    let opt = Opt::from_args();
    // println!("{:#?}", opt);
    let install_dev_dependencies = !opt.production;

    if let Err(err) = install(&Path::new("."), !install_dev_dependencies) {
        let stderr = &mut std::io::stderr();
        let errmsg = "Error writing to stderr";

        writeln!(stderr, "{}", err).expect(errmsg);

        let mut fail = err.as_fail();
            while let Some(cause) = fail.cause() {
            writeln!(stderr, "{}", cause).expect(errmsg);

            // Make `fail` the reference to the cause of the previous fail, making the
            // loop "dig deeper" into the cause chain.
            fail = cause;
        }

        // The backtrace is not always generated. `RUST_BACKTRACE=1`.
        writeln!(stderr, "{}", err.backtrace()).expect(errmsg);

        std::process::exit(1);
    }
}

fn install(root_path: &Path, install_dev_dependencies: bool) -> Result<(), Error> {
    let _ = fs::create_dir("node_modules");
    let installed_deps: HashMap<String, Version> = HashMap::new();

    return install_helper(root_path, false, &installed_deps);
}

fn install_helper(
    root_path: &Path,
    install_dev_dependencies: bool,
    installed_deps: &HashMap<String, Version>,
) -> Result<(), Error> {
    let mut package = root_path.to_path_buf();
    package.push("package.json");

    let mut package_json = File::open(package)
        .context(format!("Failed to open package.json of: {}", root_path.to_string_lossy()))?;

    let mut contents = String::new();
    package_json
        .read_to_string(&mut contents)
        .context(format!("Failed to read package.json of: {}", root_path.to_string_lossy()))?;

    let v: Value =
        serde_json::from_str(&contents)
        .context(format!("Failed to deserialize package.json of: {}", root_path.to_string_lossy()))?;

    if let Some(deps) = v["dependencies"].as_object() {
        install_deps(root_path, deps, &installed_deps)
            .context(format!("Failed to install a dependency of: {}", root_path.to_string_lossy()))?;
    }

    if install_dev_dependencies {
        if let Some(dev_deps) = v["devDependencies"].as_object() {
            install_deps(root_path, dev_deps, &installed_deps)
                .context(format!("Failed to install a dev dependency of: {}", root_path.to_string_lossy()))?;
        }
    }

    Ok(())
}

fn get_cache_dir() -> Result<PathBuf, Error> {
    let mut cache_dir = dirs::home_dir().ok_or(NeedHomeDir)?;

    cache_dir.push(".nary_cache");
    create_dir_all(&cache_dir).context("Couldn't create cache")?;

    Ok(cache_dir)
}

/**
 * Cache the given package (key) at version from the given url, returning the (gzipped) tarball.
 */
fn cache(key: &str, version: &str, tarball_url: &Url) -> Result<Vec<u8>, Error> {
    let mut tarball_res = Vec::<u8>::new();
    let mut path = get_cache_dir()?;
    path.push(&utf8_percent_encode(
        key,
        PATH_SEGMENT_ENCODE_SET,
    ).to_string());
    let _ = fs::create_dir(&path);
    path.push(&version);
    let _ = fs::create_dir(&path);
    path.push("package.tgz");

    let cache_file = File::open(&path);

    match cache_file {
        Ok(mut cache_file) => {            
            cache_file.read_to_end(&mut tarball_res)
                .context("Couldn't cache file")?;
            println!("Read {} from cache", path.to_string_lossy());
            Ok(tarball_res)
        },
        Err(_) => {
             let ssl = NativeTlsClient::new().context("Unable to create a NativeTlsClient")?;
            let connector = HttpsConnector::new(ssl);
            let client = Client::with_connector(connector);

            client.get(tarball_url.clone())
                // .header(AcceptEncoding(vec![qitem(Encoding::Gzip)]))
                .send()
                .with_context(|_| format!("Couldn't get tarball: {:?}", &tarball_url))?
                .read_to_end(&mut tarball_res)
                .with_context(|_| format!("Couldn't read to string tarball: {:?}", &tarball_url))?;

            // client.get(&*url).send().context(format!("Couldn't GET URL: {}", url))?.read_to_string(&mut body)
            // .context(format!("Couldn't ready body of: {}", url))?;

            let mut cache_file =
                File::create(&path).context("Couldn't cache file")?;
            println!("Caching {}", path.to_string_lossy());
            cache_file
                .write(tarball_res.as_slice())
                .context("Couldn't write to cache file")?;
            Ok(tarball_res)
        },
    }
}

fn gunzip(tarball: Vec<u8>, tarball_url: &Url) -> Result<Vec<u8>, Error> {
    use flate2::read::GzDecoder;
    let mut vec = Vec::new();
    let mut d = GzDecoder::new(tarball.as_slice());
    let _ = d.read_to_end(&mut vec)
        .with_context(|_| format!("Couldn't read to end of tarball: {}", tarball_url))?;

    Ok(vec)
}

fn unpack_archive(archive: &mut Archive<&[u8]>, destination_path: &PathBuf, tarball_url: &Url) -> Result<(), Error> {
    for (key, file) in archive.entries() // https://docs.rs/tar/0.4.26/tar/struct.Entries.html
        .with_context(|_| format!("{} didn't provide file entries", tarball_url))?
        .enumerate()
    {
        // Make sure there wasn't an I/O error

        if file.is_ok() {
            let mut entry = file.ok().unwrap();
            // Inspect metadata about the file
            // println!("{:?}", entry.header().path().unwrap());
            // println!("{}", entry.header().size().unwrap());

            let mut entry_header = entry
                .header()
                .path()
                .with_context(|_| format!("Tarball {} had a bad entry path: {}", tarball_url, key))?
                .into_owned();

            if entry_header.is_absolute() {
                return Err(format_err!("{:?} is absolute from {}", entry_header, tarball_url));
            }

            if entry_header.strip_prefix("package/").is_ok() {
                entry_header = entry_header
                    .strip_prefix("package/")
                    .with_context(|_| format!("Tarball {} had no package/ prefix for {}", tarball_url, key))?
                    .to_path_buf();
            }

            // println!("Entry header: {:?}", entry_header);

            let mut file_path = destination_path.clone();
            file_path.push(entry_header);

            // println!("Creating {:?}", file_path);

            let mut dir_path = file_path.clone();
            dir_path.pop();
            create_dir_all(&dir_path)
                .with_context(|_| format!("Couldn't create dir {} for {}", file_path.display(), key))?;
            entry.unpack(&file_path)
                .with_context(|_| format!("Couldn't unpack {} for {}", file_path.display(), key))?;
        } else {
            eprintln!("Tarball {} had a bad entry {}", tarball_url, key);
            // let mut entry = entry.with_context(|_| format!("Tarball {} had a bad entry {}", tarball_url, key))?;
        }
    }

    Ok(())
}

fn install_deps(
    root_path: &Path,
    deps: &serde_json::Map<String, serde_json::Value>,
    installed_deps: &HashMap<String, Version>,
) -> Result<(), Error> {
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


            let required_version = Range::new(version).parse()
                .with_context(|_| format!("Version {} of {} didn't parse", version, key))?;
            match installed_deps.get(key) {
                Some(installed_version) => if required_version.test(installed_version) {
                    println!(
                        "Already have {} @ {}; don't need to install {}",
                        key,
                        installed_version,
                        version
                    );
                    continue;
                },
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
                .with_context(|_| format!("Couldn't GET URL: {}", url))?
                .read_to_string(&mut body)
                .with_context(|_| format!("Couldn't ready body of: {}", url))?;

            // println!("{}", &body);

            let metadata: Value = serde_json::from_str(&body)
                .with_context(|_| format!("Couldn't JSON parse metadata from {}", url))?;
            let versions = &metadata["versions"]
                .as_object()
                .ok_or(format_err!("Versions was not a JSON object. {}", url))?;

            for version in versions.iter().rev() {
                if required_version.test(&Version::new(version.0.as_str()).parse()
                    .with_context(|_| format_err!("{} didn't parse", version.0))?)
                {
                    // let version = &versions[version];
                    // println!("Version: \n{:?}", version);

                    // PROGRESS_BAR.inc(1);

                    let dist = &version.1["dist"];
                    // let dis = version.1;

                    // println!("Dist: {}", dist);
                    let tarball_url = Url::parse(
                        &dist["tarball"]
                            .as_str()
                            .ok_or(format_err!("tarball URL didn't convert to string"))?,
                    ).context("Couldn't parse URL")?;
                    // let url = Url::parse(tarball_url);
                    // println!("Tarball URL: {:?}", &tarball_url);

                    let tarball = gunzip(cache(key, &version.0, &tarball_url)?, &tarball_url)?;
                    let mut archive = Archive::new(tarball.as_slice());

                    let mut path = root_path.to_path_buf();
                    path.push("node_modules");
                    path.push(key);

                    unpack_archive(&mut archive, &path, &tarball_url)?;

                    let version_to_insert = Version::new(version.0.as_str()).parse()
                        .with_context(|_| format!("{} didn't parse", version.0))?;
                    installed_deps.insert(key.to_string(), version_to_insert);

                    next_paths.insert(path);

                    break;
                }
            }
        } else {
            return Err(format_err!("A version of {} wasn't string parsable.", key));
        }
    }

    for path in next_paths {
        install_helper(&path, false, &installed_deps)?;
    }

    Ok(())
}
