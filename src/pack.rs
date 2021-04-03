use failure::{format_err, Error, ResultExt};
use hyper::Url;
use std::{fs::create_dir_all, io::Read, path::PathBuf};
use tar::Archive;
// use indicatif::ProgressBar;

pub fn gunzip(tarball: Vec<u8>, tarball_url: &Url) -> Result<Vec<u8>, Error> {
    use flate2::read::GzDecoder;
    let mut vec = Vec::new();
    let mut d = GzDecoder::new(tarball.as_slice());
    let _ = d
        .read_to_end(&mut vec)
        .with_context(|_| format!("Couldn't read to end of tarball: {}", tarball_url))?;

    Ok(vec)
}

pub fn unpack_archive(
    archive: &mut Archive<&[u8]>,
    destination_path: &PathBuf,
    tarball_url: &Url,
) -> Result<(), Error> {
    for (key, file) in archive
        .entries() // https://docs.rs/tar/0.4.26/tar/struct.Entries.html
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
                return Err(format_err!(
                    "{:?} is absolute from {}",
                    entry_header,
                    tarball_url
                ));
            }

            if entry_header.strip_prefix("package/").is_ok() {
                entry_header = entry_header
                    .strip_prefix("package/")
                    .with_context(|_| {
                        format!("Tarball {} had no package/ prefix for {}", tarball_url, key)
                    })?
                    .to_path_buf();
            }

            // println!("Entry header: {:?}", entry_header);

            let mut file_path = destination_path.clone();
            file_path.push(entry_header);

            // println!("Creating {:?}", file_path);

            let mut dir_path = file_path.clone();
            dir_path.pop();
            create_dir_all(&dir_path).with_context(|_| {
                format!("Couldn't create dir {} for {}", file_path.display(), key)
            })?;
            entry
                .unpack(&file_path)
                .with_context(|_| format!("Couldn't unpack {} for {}", file_path.display(), key))?;
        } else {
            eprintln!("Tarball {} had a bad entry {}", tarball_url, key);
            // let mut entry = entry.with_context(|_| format!("Tarball {} had a bad entry {}", tarball_url, key))?;
        }
    }

    Ok(())
}
