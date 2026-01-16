use snafu::prelude::*;
use std::path::PathBuf;

/// Error type for nary_lib
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    // === Network Errors ===
    #[snafu(display("Failed to create HTTP client"))]
    HttpClientBuild {
        source: reqwest::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to fetch {url}"))]
    HttpRequest {
        url: String,
        source: reqwest::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to read response from {url}"))]
    HttpResponse {
        url: String,
        source: reqwest::Error,
        backtrace: snafu::Backtrace,
    },

    // === Parse Errors ===
    #[snafu(display("Failed to parse JSON from {source_desc}"))]
    JsonParse {
        source_desc: String,
        source: serde_json::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to serialize JSON"))]
    JsonSerialize {
        source: serde_json::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Invalid semver range '{range}' for {package}"))]
    SemverRangeParse { package: String, range: String },

    #[snafu(display("Invalid semver version '{version}'"))]
    SemverVersionParse { version: String },

    #[snafu(display("Missing field '{field}' for {package}"))]
    MissingField {
        package: String,
        field: &'static str,
    },

    // === IO Errors ===
    #[snafu(display("Failed to read {}", path.display()))]
    FileRead {
        path: PathBuf,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to write {}", path.display()))]
    FileWrite {
        path: PathBuf,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to create directory {}", path.display()))]
    DirCreate {
        path: PathBuf,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to symlink {} -> {}", link.display(), target.display()))]
    Symlink {
        link: PathBuf,
        target: PathBuf,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    // === Cache Errors ===
    #[snafu(display("Could not determine cache directory"))]
    CacheDir,

    #[snafu(display("Package {package}@{version} not found in cache (offline mode)"))]
    OfflineTarballNotCached { package: String, version: String },

    #[snafu(display("Metadata for {package} not found in cache (offline mode)"))]
    OfflineMetadataNotCached { package: String },

    // === Archive Errors ===
    #[snafu(display("Failed to decompress {url}"))]
    Gunzip {
        url: String,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Tarball {url} has no entries"))]
    TarballEmpty {
        url: String,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Tarball {url} entry {index} has invalid path"))]
    TarballEntryPath {
        url: String,
        index: usize,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Tarball {url} contains absolute path (security risk)"))]
    TarballAbsolutePath { url: String },

    #[snafu(display("Tarball {url} contains path traversal (security risk)"))]
    TarballPathTraversal { url: String },

    #[snafu(display(
        "Tarball {url} contains unsupported entry type (only files and directories allowed)"
    ))]
    TarballUnsupportedEntry { url: String },

    #[snafu(display("Failed to unpack entry {index} from {url}"))]
    TarballUnpack {
        url: String,
        index: usize,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    // === Version Resolution ===
    #[snafu(display("No matching version for {package} {requested}"))]
    NoMatchingVersion { package: String, requested: String },

    #[snafu(display(
        "No mature version for {package} {requested}: newest {newest_version} is only {age_minutes} minutes old (requires {required_minutes})"
    ))]
    NoMatureVersion {
        package: String,
        requested: String,
        newest_version: String,
        age_minutes: u64,
        required_minutes: u64,
    },

    #[snafu(display("Cyclic dependency: {package}"))]
    CyclicDependency { package: String },

    // === Git Errors ===
    #[snafu(display("Failed to clone {url}"))]
    GitClone {
        url: String,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("Failed to checkout {git_ref} in {url}"))]
    GitCheckout {
        url: String,
        git_ref: String,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    // === Integrity Errors ===
    #[snafu(display(
        "Integrity mismatch for {package}@{version}: expected {expected}, got {actual}"
    ))]
    IntegrityMismatch {
        package: String,
        version: String,
        expected: String,
        actual: String,
    },

    #[snafu(display("Invalid integrity format: {integrity}"))]
    InvalidIntegrity { integrity: String },

    // === Lifecycle Script Errors ===
    #[snafu(display("Script '{script}' failed for {package} (exit code {exit_code})"))]
    ScriptFailed {
        package: String,
        script: String,
        exit_code: i32,
    },

    #[snafu(display("Script '{script}' for {package} was killed by signal"))]
    ScriptSignaled { package: String, script: String },

    #[snafu(display("Failed to run script '{script}' for {package}"))]
    ScriptSpawn {
        package: String,
        script: String,
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    // === Peer Dependencies ===
    #[snafu(display("Peer dependency {peer}@{required} required by {package} not satisfied"))]
    PeerNotSatisfied {
        package: String,
        peer: String,
        required: String,
    },

    // === Task Errors ===
    #[snafu(display("Extraction task panicked: {message}"))]
    ExtractionTaskPanic { message: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
