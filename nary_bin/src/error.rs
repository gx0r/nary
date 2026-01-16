use snafu::prelude::*;

/// Error type for nary CLI
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    // === Library Errors ===
    #[snafu(transparent)]
    Lib {
        #[snafu(source(from(nary_lib::Error, Box::new)))]
        source: Box<nary_lib::Error>,
        backtrace: snafu::Backtrace,
    },

    #[snafu(transparent)]
    Version {
        #[snafu(source(from(nary_lib::VersionError, Box::new)))]
        source: Box<nary_lib::VersionError>,
        backtrace: snafu::Backtrace,
    },

    // === Standard Error Types ===
    #[snafu(transparent)]
    Io {
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(transparent)]
    Json {
        source: serde_json::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(transparent)]
    Http {
        source: reqwest::Error,
        backtrace: snafu::Backtrace,
    },

    // === Missing Value Errors ===
    #[snafu(display("Could not find latest version for {package}"))]
    NoLatestVersion { package: String },

    #[snafu(display("Could not find version for {package}"))]
    NoVersion { package: String },

    #[snafu(display("Script '{script}' not found in package.json"))]
    ScriptNotFound { script: String },

    #[snafu(display("package.json missing 'name' field"))]
    MissingPackageName,

    #[snafu(display("Could not find tarball URL for {package}"))]
    NoTarballUrl { package: String },

    #[snafu(display("Package {package} does not have a 'bin' field"))]
    NoBinField { package: String },

    // === Missing File Errors ===
    #[snafu(display("No package-lock.json found. Run 'nary install' first."))]
    NoLockfile,

    // === Platform Errors ===
    #[snafu(display("Symlinks not supported on this platform"))]
    SymlinksNotSupported,

    #[snafu(display("Could not determine home directory"))]
    NoHomeDirectory,

    #[snafu(display(
        "Package {package} not found in global modules. Run 'nary link' in the package directory first."
    ))]
    GlobalPackageNotFound { package: String },

    // === HTTP Errors ===
    #[snafu(display("Audit request failed: {status}"))]
    AuditRequestFailed { status: reqwest::StatusCode },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
