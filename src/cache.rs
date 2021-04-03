use failure::{Error, ResultExt};
use hyper::{net::HttpsConnector, Client, Url};
use hyper_native_tls::NativeTlsClient;
use std::{
    fs,
    fs::{create_dir_all, File},
    io::{Read, Write},
    path::PathBuf,
};
// use indicatif::ProgressBar;

use crate::error::NeedHomeDir;

pub fn get_cache_dir() -> Result<PathBuf, Error> {
    let mut cache_dir = dirs::home_dir().ok_or(NeedHomeDir)?;

    cache_dir.push(".nary_cache");
    create_dir_all(&cache_dir).context("Couldn't create cache")?;

    Ok(cache_dir)
}

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

// https://url.spec.whatwg.org/#path-percent-encode-set
pub const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'#')
    .add(b'?')
    .add(b'{')
    .add(b'}');

/**
 * Cache the given package (key) at version from the given url, returning the (gzipped) tarball.
 */
pub fn cache(key: &str, version: &str, tarball_url: &Url) -> Result<Vec<u8>, Error> {
    let mut tarball_res = Vec::<u8>::new();
    let mut path = get_cache_dir()?;
    path.push(&utf8_percent_encode(key, PATH_SEGMENT_ENCODE_SET).to_string());
    let _ = fs::create_dir(&path);
    path.push(&version);
    let _ = fs::create_dir(&path);
    path.push("package.tgz");

    let cache_file = File::open(&path);

    match cache_file {
        Ok(mut cache_file) => {
            cache_file
                .read_to_end(&mut tarball_res)
                .context("Couldn't cache file")?;
            println!("Read {} from cache", path.to_string_lossy());
            Ok(tarball_res)
        }
        Err(_) => {
            let ssl = NativeTlsClient::new().context("Unable to create a NativeTlsClient")?;
            let connector = HttpsConnector::new(ssl);
            let client = Client::with_connector(connector);

            client
                .get(tarball_url.clone())
                // .header(AcceptEncoding(vec![qitem(Encoding::Gzip)]))
                .send()
                .with_context(|_| format!("Couldn't get tarball: {:?}", &tarball_url))?
                .read_to_end(&mut tarball_res)
                .with_context(|_| format!("Couldn't read to string tarball: {:?}", &tarball_url))?;

            // client.get(&*url).send().context(format!("Couldn't GET URL: {}", url))?.read_to_string(&mut body)
            // .context(format!("Couldn't ready body of: {}", url))?;

            let mut cache_file = File::create(&path).context("Couldn't cache file")?;
            println!("Caching {}", path.to_string_lossy());
            cache_file
                .write(tarball_res.as_slice())
                .context("Couldn't write to cache file")?;
            Ok(tarball_res)
        }
    }
}
