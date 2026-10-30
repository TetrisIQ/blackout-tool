//! Phase 4: local CA, Caddy, hosts (every change reversible, marked as kit's).

pub mod ca;
pub mod caddy;
pub mod hosts;
pub mod nft;
pub mod trust;

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("{context}: {source}")]
    Io { context: String, source: std::io::Error },
    #[error("certificate error: {0}")]
    Cert(String),
    #[error("{0}")]
    Invalid(String),
    #[error("`{tool}` not found. {hint}")]
    MissingTool { tool: String, hint: String },
    #[error("`{command}` failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("{} exists but was not written by kit; refusing to overwrite it", .0.display())]
    Foreign(PathBuf),
}

pub(crate) fn io_err(context: &str, path: &Path) -> impl FnOnce(std::io::Error) -> NetError {
    let context = format!("{context} {}", path.display());
    move |source| NetError::Io { context, source }
}

/// Locations of the network layer, all below the data directory.
#[derive(Debug, Clone)]
pub struct NetPaths {
    pub dir: PathBuf,
}

impl NetPaths {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("net"),
        }
    }
    pub fn ca_dir(&self) -> PathBuf {
        self.dir.join("ca")
    }
}
