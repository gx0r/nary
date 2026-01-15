use base64::{engine::general_purpose::STANDARD, Engine};
use sha2::{Digest, Sha512};

use crate::error::{IntegrityMismatchSnafu, InvalidIntegritySnafu, Result};

/// Compute SHA-512 integrity string in SRI format (sha512-base64hash)
pub fn compute_sha512_integrity(data: &[u8]) -> String {
    let mut hasher = Sha512::new();
    hasher.update(data);
    let hash = hasher.finalize();
    format!("sha512-{}", STANDARD.encode(hash))
}

/// Verify data matches the expected integrity hash (SRI format)
/// Returns Ok(()) if valid, Err if mismatch or invalid format
pub fn verify_integrity(data: &[u8], integrity: &str, package: &str, version: &str) -> Result<()> {
    // npm integrity can have multiple hashes separated by space, we check the first sha512
    let hash_part = integrity
        .split_whitespace()
        .find(|h| h.starts_with("sha512-"))
        .ok_or_else(|| {
            InvalidIntegritySnafu {
                integrity: integrity.to_string(),
            }
            .build()
        })?;

    let actual = compute_sha512_integrity(data);

    if actual != hash_part {
        return IntegrityMismatchSnafu {
            package: package.to_string(),
            version: version.to_string(),
            expected: hash_part.to_string(),
            actual,
        }
        .fail();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_integrity() {
        let data = b"hello world";
        let integrity = compute_sha512_integrity(data);
        assert!(integrity.starts_with("sha512-"));
    }

    #[test]
    fn test_verify_integrity_valid() {
        let data = b"hello world";
        let integrity = compute_sha512_integrity(data);
        assert!(verify_integrity(data, &integrity, "test", "1.0.0").is_ok());
    }

    #[test]
    fn test_verify_integrity_mismatch() {
        let data = b"hello world";
        let wrong_integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(verify_integrity(data, wrong_integrity, "test", "1.0.0").is_err());
    }

    #[test]
    fn test_verify_integrity_invalid_format() {
        let data = b"hello world";
        let invalid = "md5-abc123";
        assert!(verify_integrity(data, invalid, "test", "1.0.0").is_err());
    }

    #[test]
    fn test_verify_multiple_hashes() {
        // npm can have multiple hashes, we use the sha512 one
        let data = b"hello world";
        let sha512 = compute_sha512_integrity(data);
        let multi = format!("sha1-abc123 {}", sha512);
        assert!(verify_integrity(data, &multi, "test", "1.0.0").is_ok());
    }
}
