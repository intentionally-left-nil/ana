//! [`crate::Error`]: every way [`crate::Downloader::new`] and
//! [`crate::reconcile`] can fail.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// [`rattler_cache::ensure_cache_dir`] failed on the shared cache root.
    #[error("could not prepare the rattler cache directory at {path}")]
    Cache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Building the shared HTTP client ([`reqwest::Client::builder`])
    /// failed -- this is a configuration error (bad TLS backend, etc.),
    /// never a network failure (no request has been made yet).
    #[error("could not build the shared HTTP client")]
    BuildClient(#[source] reqwest::Error),

    /// Reading the environment's currently-installed packages (for
    /// [`crate::ReconcileMode::Inexact`]'s `ignored` set) failed. A
    /// missing `env_path`/`conda-meta` is not an error here -- rattler's
    /// own minimal-prefix-record reader already treats that as "nothing
    /// installed."
    #[error("could not read the environment's installed packages at {path}")]
    ReadPrefix {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// [`rattler::install::Installer::install`] itself failed (download,
    /// extraction, hash mismatch, linking, ...).
    #[error("failed to install the environment at {path}")]
    Install {
        path: PathBuf,
        #[source]
        source: Box<rattler::install::InstallerError>,
    },
}
