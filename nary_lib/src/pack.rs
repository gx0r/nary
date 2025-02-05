use snafu::ResultExt;
use std::{
    collections::HashSet,
    fs::create_dir_all,
    io::Read,
    path::{Path, PathBuf},
};
use tar::Archive;

use crate::error::{
    DirCreateSnafu, GunzipSnafu, Result, TarballAbsolutePathSnafu, TarballEmptySnafu,
    TarballEntryPathSnafu, TarballPathTraversalSnafu, TarballUnpackSnafu,
    TarballUnsupportedEntrySnafu,
};

pub fn gunzip(tarball: Vec<u8>, tarball_url: &str) -> Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    let mut vec = Vec::new();
    let mut d = GzDecoder::new(tarball.as_slice());
    d.read_to_end(&mut vec).context(GunzipSnafu {
        url: tarball_url.to_string(),
    })?;

    Ok(vec)
}

pub fn unpack_archive(
    archive: &mut Archive<&[u8]>,
    destination_path: &Path,
    tarball_url: &str,
) -> Result<()> {
    // Cache created directories to avoid redundant syscalls
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();

    for (index, file) in archive
        .entries()
        .context(TarballEmptySnafu {
            url: tarball_url.to_string(),
        })?
        .enumerate()
    {
        if let Ok(mut entry) = file {
            let mut entry_header = entry
                .header()
                .path()
                .context(TarballEntryPathSnafu {
                    url: tarball_url.to_string(),
                    index,
                })?
                .into_owned();

            if entry_header.is_absolute() {
                return TarballAbsolutePathSnafu {
                    url: tarball_url.to_string(),
                }
                .fail();
            }

            // Check for path traversal (../ components)
            if entry_header
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return TarballPathTraversalSnafu {
                    url: tarball_url.to_string(),
                }
                .fail();
            }

            // Only allow regular files and directories (reject symlinks, hardlinks, devices, fifos)
            let entry_type = entry.header().entry_type();
            if !entry_type.is_file() && !entry_type.is_dir() {
                return TarballUnsupportedEntrySnafu {
                    url: tarball_url.to_string(),
                }
                .fail();
            }

            if entry_header.strip_prefix("package/").is_ok() {
                entry_header = entry_header.strip_prefix("package/").unwrap().to_path_buf();
            }

            let mut file_path = destination_path.to_path_buf();
            file_path.push(entry_header);

            let mut dir_path = file_path.clone();
            dir_path.pop();

            // Only create directory if not already created
            if !created_dirs.contains(&dir_path) {
                create_dir_all(&dir_path).context(DirCreateSnafu {
                    path: dir_path.clone(),
                })?;
                created_dirs.insert(dir_path);
            }

            entry.unpack(&file_path).context(TarballUnpackSnafu {
                url: tarball_url.to_string(),
                index,
            })?;

            // Normalize directory permissions: ensure execute bits where read bits exist.
            // Some packages (e.g. char-regex) have tarballs with directories missing +x, which
            // prevents traversal and makes rm -rf fail. This mirrors npm's node-tar behavior.
            #[cfg(unix)]
            if entry_type.is_dir() {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&file_path) {
                    let mode = meta.permissions().mode();
                    let normalized = mode | ((mode >> 2) & 0o111);
                    if normalized != mode {
                        let _ = std::fs::set_permissions(
                            &file_path,
                            std::fs::Permissions::from_mode(normalized),
                        );
                    }
                }
            }
        } else {
            eprintln!("Tarball {} had a bad entry {}", tarball_url, index);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    use tar::{Builder, Header};
    use tempfile::TempDir;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn create_raw_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, *path, &content[..])
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn test_gunzip_valid_data() {
        let original = b"hello world";
        let compressed = gzip(original);
        let result = gunzip(compressed, "test://url").unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_gunzip_invalid_data() {
        let invalid = vec![0, 1, 2, 3, 4, 5]; // Not valid gzip
        let result = gunzip(invalid, "test://url");
        assert!(result.is_err());
    }

    #[test]
    fn test_unpack_simple_archive() {
        let tar_data = create_raw_tarball(&[("file.txt", b"content")]);
        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        let content = std::fs::read_to_string(dest.join("file.txt")).unwrap();
        assert_eq!(content, "content");
    }

    #[test]
    fn test_unpack_with_package_prefix() {
        // npm tarballs have a package/ prefix that should be stripped
        let tar_data = create_raw_tarball(&[("package/index.js", b"module.exports = {}")]);
        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        // Should be at dest/index.js, not dest/package/index.js
        let content = std::fs::read_to_string(dest.join("index.js")).unwrap();
        assert_eq!(content, "module.exports = {}");
    }

    #[test]
    fn test_unpack_nested_directories() {
        let tar_data = create_raw_tarball(&[
            ("package/src/lib/utils.js", b"export const util = 1;"),
            ("package/src/index.js", b"import './lib/utils';"),
        ]);
        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        assert!(dest.join("src/lib/utils.js").exists());
        assert!(dest.join("src/index.js").exists());
    }

    #[test]
    fn test_unpack_absolute_path_rejected() {
        // Create tarball with absolute path by manually crafting the tar header
        // The tar crate's builder prevents this, so we construct raw bytes
        let mut tar_data = Vec::new();

        // Tar header is 512 bytes
        let mut header = [0u8; 512];

        // Name field (0-99): absolute path "/etc/passwd"
        let name = b"/etc/passwd";
        header[..name.len()].copy_from_slice(name);

        // Mode field (100-107): "0000644\0"
        header[100..108].copy_from_slice(b"0000644\0");

        // UID (108-115): "0000000\0"
        header[108..116].copy_from_slice(b"0000000\0");

        // GID (116-123): "0000000\0"
        header[116..124].copy_from_slice(b"0000000\0");

        // Size (124-135): "00000000004\0" (4 bytes)
        header[124..136].copy_from_slice(b"00000000004\0");

        // Mtime (136-147): "00000000000\0"
        header[136..148].copy_from_slice(b"00000000000\0");

        // Checksum placeholder (148-155): 8 spaces initially
        header[148..156].copy_from_slice(b"        ");

        // Type flag (156): '0' for regular file
        header[156] = b'0';

        // Calculate checksum (sum of all bytes treating checksum field as spaces)
        let checksum: u32 = header.iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_str.as_bytes());

        tar_data.extend_from_slice(&header);

        // File content (padded to 512 bytes)
        let mut content_block = [0u8; 512];
        content_block[..4].copy_from_slice(b"evil");
        tar_data.extend_from_slice(&content_block);

        // End of archive (two zero blocks)
        tar_data.extend_from_slice(&[0u8; 1024]);

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, &dest, "test://url");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("absolute"));
    }

    #[test]
    fn test_unpack_path_traversal_rejected() {
        // Create tarball with path traversal attempt
        let mut tar_data = Vec::new();

        // Tar header is 512 bytes
        let mut header = [0u8; 512];

        // Name field (0-99): path traversal "../../../etc/passwd"
        let name = b"package/../../../etc/passwd";
        header[..name.len()].copy_from_slice(name);

        // Mode field (100-107): "0000644\0"
        header[100..108].copy_from_slice(b"0000644\0");

        // UID (108-115): "0000000\0"
        header[108..116].copy_from_slice(b"0000000\0");

        // GID (116-123): "0000000\0"
        header[116..124].copy_from_slice(b"0000000\0");

        // Size (124-135): "00000000004\0" (4 bytes)
        header[124..136].copy_from_slice(b"00000000004\0");

        // Mtime (136-147): "00000000000\0"
        header[136..148].copy_from_slice(b"00000000000\0");

        // Checksum placeholder (148-155): 8 spaces initially
        header[148..156].copy_from_slice(b"        ");

        // Type flag (156): '0' for regular file
        header[156] = b'0';

        // Calculate checksum (sum of all bytes treating checksum field as spaces)
        let checksum: u32 = header.iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_str.as_bytes());

        tar_data.extend_from_slice(&header);

        // File content (padded to 512 bytes)
        let mut content_block = [0u8; 512];
        content_block[..4].copy_from_slice(b"evil");
        tar_data.extend_from_slice(&content_block);

        // End of archive (two zero blocks)
        tar_data.extend_from_slice(&[0u8; 1024]);

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, &dest, "test://url");

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("traversal"));
    }

    /// Helper to create a tarball with a specific entry type using tar crate's Builder
    fn create_tarball_with_entry_type(path: &str, entry_type: tar::EntryType) -> Vec<u8> {
        use tar::{Builder, Header};

        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();

        header.set_path(path).unwrap();
        header.set_size(0);
        header.set_entry_type(entry_type);
        header.set_mode(0o644);
        header.set_mtime(0);

        // For symlinks/hardlinks, set link target
        if entry_type == tar::EntryType::Symlink || entry_type == tar::EntryType::Link {
            header.set_link_name("/etc/passwd").unwrap();
        }

        header.set_cksum();
        builder.append(&header, &[] as &[u8]).unwrap();

        builder.into_inner().unwrap()
    }

    #[test]
    fn test_unpack_symlink_rejected() {
        let tar_data = create_tarball_with_entry_type("package/evil-link", tar::EntryType::Symlink);
        let temp = TempDir::new().unwrap();
        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, temp.path(), "test://url");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported entry type"));
    }

    #[test]
    fn test_unpack_hardlink_rejected() {
        let tar_data =
            create_tarball_with_entry_type("package/evil-hardlink", tar::EntryType::Link);
        let temp = TempDir::new().unwrap();
        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, temp.path(), "test://url");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported entry type"));
    }

    #[test]
    fn test_unpack_char_device_rejected() {
        let tar_data = create_tarball_with_entry_type("package/dev-null", tar::EntryType::Char);
        let temp = TempDir::new().unwrap();
        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, temp.path(), "test://url");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported entry type"));
    }

    #[test]
    fn test_unpack_block_device_rejected() {
        let tar_data = create_tarball_with_entry_type("package/sda", tar::EntryType::Block);
        let temp = TempDir::new().unwrap();
        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, temp.path(), "test://url");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported entry type"));
    }

    #[test]
    fn test_unpack_fifo_rejected() {
        let tar_data = create_tarball_with_entry_type("package/my-fifo", tar::EntryType::Fifo);
        let temp = TempDir::new().unwrap();
        let mut archive = Archive::new(tar_data.as_slice());
        let result = unpack_archive(&mut archive, temp.path(), "test://url");

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported entry type"));
    }

    #[test]
    fn test_unpack_preserves_file_contents() {
        let content1 = b"const x = 42;\nexport default x;";
        let content2 = b"{\"name\": \"test\", \"version\": \"1.0.0\"}";

        let tar_data = create_raw_tarball(&[
            ("package/index.js", content1),
            ("package/package.json", content2),
        ]);

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        let read1 = std::fs::read(dest.join("index.js")).unwrap();
        let read2 = std::fs::read(dest.join("package.json")).unwrap();

        assert_eq!(read1, content1);
        assert_eq!(read2, content2);
    }

    #[test]
    fn test_unpack_multiple_files() {
        let tar_data =
            create_raw_tarball(&[("a.txt", b"aaa"), ("b.txt", b"bbb"), ("c.txt", b"ccc")]);

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "aaa");
        assert_eq!(std::fs::read_to_string(dest.join("b.txt")).unwrap(), "bbb");
        assert_eq!(std::fs::read_to_string(dest.join("c.txt")).unwrap(), "ccc");
    }

    #[test]
    fn test_unpack_directory_caching() {
        // Multiple files in same directory should only create dir once
        let tar_data = create_raw_tarball(&[
            ("package/src/a.js", b"a"),
            ("package/src/b.js", b"b"),
            ("package/src/c.js", b"c"),
        ]);

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        // This test mainly verifies no errors occur; caching is internal
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        assert!(dest.join("src/a.js").exists());
        assert!(dest.join("src/b.js").exists());
        assert!(dest.join("src/c.js").exists());
    }

    #[test]
    fn test_gunzip_then_unpack_integration() {
        let tar_data = create_raw_tarball(&[("package/lib.js", b"export const lib = true;")]);
        let gzipped = gzip(&tar_data);

        let decompressed = gunzip(gzipped, "test://url").unwrap();

        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(decompressed.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        let content = std::fs::read_to_string(dest.join("lib.js")).unwrap();
        assert_eq!(content, "export const lib = true;");
    }

    #[cfg(unix)]
    #[test]
    fn test_unpack_normalizes_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        // Create a tarball with a directory that has mode 0644 (no execute bit)
        // This reproduces issues like char-regex where rm -rf fails
        let mut builder = Builder::new(Vec::new());

        // Add directory entry with restrictive permissions (no execute)
        let mut dir_header = Header::new_gnu();
        dir_header.set_path("package/subdir").unwrap();
        dir_header.set_size(0);
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_mode(0o644); // rw-r--r-- (missing +x)
        dir_header.set_mtime(0);
        dir_header.set_cksum();
        builder.append(&dir_header, &[] as &[u8]).unwrap();

        // Add a file in that directory
        let mut file_header = Header::new_gnu();
        file_header.set_path("package/subdir/file.txt").unwrap();
        file_header.set_size(5);
        file_header.set_entry_type(tar::EntryType::Regular);
        file_header.set_mode(0o644);
        file_header.set_mtime(0);
        file_header.set_cksum();
        builder.append(&file_header, &b"hello"[..]).unwrap();

        let tar_data = builder.into_inner().unwrap();
        let temp = TempDir::new().unwrap();
        let dest = temp.path().to_path_buf();

        let mut archive = Archive::new(tar_data.as_slice());
        unpack_archive(&mut archive, &dest, "test://url").unwrap();

        // Verify the directory has execute permission (can be traversed)
        let dir_meta = std::fs::metadata(dest.join("subdir")).unwrap();
        let mode = dir_meta.permissions().mode();

        // Should have at least user execute bit since we had user read
        assert!(
            mode & 0o100 != 0,
            "directory should have user execute bit, got mode {:o}",
            mode
        );

        // Verify the file is accessible (proves directory is traversable)
        let content = std::fs::read_to_string(dest.join("subdir/file.txt")).unwrap();
        assert_eq!(content, "hello");

        // Verify rm -rf would work (the original symptom)
        std::fs::remove_dir_all(dest.join("subdir")).unwrap();
    }
}
