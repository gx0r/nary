use snafu::prelude::*;
use snafu::GenerateImplicitData;

/// Error type for nary CLI
#[derive(Debug, Snafu)]
#[snafu(visibility(pub))]
pub enum Error {
    // === Library Errors ===
    #[snafu(display("{source}"))]
    Lib {
        source: nary_lib::Error,
        #[snafu(backtrace)]
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("{source}"))]
    Version {
        source: nary_lib::VersionError,
        backtrace: snafu::Backtrace,
    },

    // === Standard Error Types ===
    #[snafu(display("{source}"))]
    Io {
        source: std::io::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("{source}"))]
    Json {
        source: serde_json::Error,
        backtrace: snafu::Backtrace,
    },

    #[snafu(display("{source}"))]
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

impl From<nary_lib::Error> for Error {
    fn from(source: nary_lib::Error) -> Self {
        Error::Lib {
            source,
            backtrace: snafu::Backtrace::generate(),
        }
    }
}

impl From<nary_lib::VersionError> for Error {
    fn from(source: nary_lib::VersionError) -> Self {
        Error::Version {
            source,
            backtrace: snafu::Backtrace::generate(),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io {
            source,
            backtrace: snafu::Backtrace::generate(),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(source: serde_json::Error) -> Self {
        Error::Json {
            source,
            backtrace: snafu::Backtrace::generate(),
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(source: reqwest::Error) -> Self {
        Error::Http {
            source,
            backtrace: snafu::Backtrace::generate(),
        }
    }
}
