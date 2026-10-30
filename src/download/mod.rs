//! Phase 2: resumable, parallel, checksum-verified downloads.
//!
//! A file is split into fixed-size chunks that worker threads fetch with HTTP range requests
//! into `<name>.part`. `<name>.part.state` records finished chunks, so after `kill -9`, a crash
//! or a lost connection the next run continues where it stopped. Only a file whose SHA-256
//! matches the catalog is renamed to its final name.

mod engine;
mod http;
pub mod ratelimit;
pub mod state;

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use engine::{backoff_delay, download};
pub use http::UreqRange;
pub use ratelimit::RateLimiter;
use state::State;

pub const DEFAULT_CHUNK_SIZE: u64 = 8 * 1024 * 1024;

/// One file to fetch.
#[derive(Debug, Clone)]
pub struct Target {
    /// File name inside `dir`.
    pub name: String,
    pub dir: PathBuf,
    /// Primary URL first, then mirrors in order of preference. All must serve identical bytes.
    pub sources: Vec<String>,
    pub size: u64,
    /// Lowercase hex SHA-256 of the complete file.
    pub sha256: String,
}

impl Target {
    pub fn final_path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    pub fn part_path(&self) -> PathBuf {
        self.dir.join(format!("{}.part", self.name))
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join(format!("{}.part.state", self.name))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(format!("{}.lock", self.name))
    }

    pub(crate) fn validate(&self) -> Result<(), DownloadError> {
        let bad = |m: &str| Err(DownloadError::BadTarget(m.to_string()));
        if !is_safe_file_name(&self.name) {
            return bad("unsafe file name");
        }
        if self.sources.is_empty() {
            return bad("no download source");
        }
        if self.size == 0 {
            return bad("size must be greater than 0");
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return bad("sha256 must be 64 lowercase hex characters");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Parallel chunk downloads.
    pub jobs: usize,
    pub chunk_size: u64,
    /// Attempts per chunk before giving up (progress stays on disk for the next run).
    pub retries: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            jobs: 4,
            chunk_size: DEFAULT_CHUNK_SIZE,
            retries: 10,
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The final file existed and its checksum matched; nothing was fetched.
    AlreadyPresent,
    Downloaded {
        /// Bytes that were already on disk from an earlier, interrupted run.
        resumed_bytes: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("invalid download target: {0}")]
    BadTarget(String),
    #[error("{context}: {source}")]
    Io { context: String, source: std::io::Error },
    #[error("not enough disk space in {}", .0.display())]
    NoSpace(PathBuf),
    #[error("another kit process is downloading this file ({})", .0.display())]
    Locked(PathBuf),
    #[error("download failed: {0}")]
    Failed(String),
    #[error("checksum mismatch: expected {expected}, got {actual}; the partial download was deleted")]
    Corrupt { expected: String, actual: String },
}

/// Why one range request did not deliver data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Worth retrying (network error, timeout, 5xx, 429).
    Transient {
        message: String,
        retry_after: Option<Duration>,
    },
    /// This source cannot serve the file (404, ignores `Range`, wrong size). Drop it for the run.
    SourceUnusable(String),
}

impl FetchError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self::Transient {
            message: message.into(),
            retry_after: None,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient { message, .. } | Self::SourceUnusable(message) => f.write_str(message),
        }
    }
}

/// Fetches byte ranges; implemented by [`UreqRange`] and by fakes in tests.
pub trait RangeSource: Sync {
    /// Replaces `buf` with exactly bytes `start..start+len` of the `total`-byte file at `url`.
    fn read_range(
        &self,
        url: &str,
        start: u64,
        len: u64,
        total: u64,
        buf: &mut Vec<u8>,
    ) -> Result<(), FetchError>;
}

/// Receives progress; called from worker threads.
pub trait Progress: Sync {
    fn on_progress(&self, _done_bytes: u64, _total_bytes: u64) {}
    /// All data is on disk; the checksum is being verified.
    fn on_verify(&self) {}
}

pub struct NoProgress;
impl Progress for NoProgress {}

/// A name that cannot escape the download directory.
pub fn is_safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && !name.ends_with(".part")
        && !name.ends_with(".state")
        && !name.ends_with(".lock")
}

/// Last path segment of a URL, if it is usable as a file name.
pub fn file_name_from_url(url: &str) -> Option<String> {
    let no_query = url.split(['?', '#']).next()?;
    let name = no_query.rsplit('/').next()?;
    is_safe_file_name(name).then(|| name.to_string())
}

/// Bytes still to be fetched for `target`, crediting a finished file or a resumable partial one.
pub fn remaining_bytes(target: &Target) -> u64 {
    if std::fs::metadata(target.final_path()).is_ok_and(|m| m.is_file() && m.len() == target.size) {
        return 0;
    }
    let part_ok = std::fs::metadata(target.part_path()).is_ok_and(|m| m.is_file() && m.len() == target.size);
    match State::load(&target.state_path()) {
        Some(s) if part_ok && s.matches(&target.name, target.size, &target.sha256) => {
            target.size - s.done_bytes()
        }
        _ => target.size,
    }
}

pub(crate) fn io_err(context: &str, path: &Path) -> impl FnOnce(std::io::Error) -> DownloadError {
    let context = format!("{context} {}", path.display());
    move |source| DownloadError::Io { context, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_from_urls() {
        assert_eq!(
            file_name_from_url("https://x.org/zim/wikipedia/a_b-1.zim").as_deref(),
            Some("a_b-1.zim")
        );
        assert_eq!(
            file_name_from_url("https://x.org/a.zim?x=1#f").as_deref(),
            Some("a.zim")
        );
        assert_eq!(file_name_from_url("https://x.org/dir/"), None);
        assert_eq!(file_name_from_url("https://x.org/.hidden"), None);
        assert_eq!(file_name_from_url("https://x.org/a.part"), None);
    }

    #[test]
    fn unsafe_names_rejected() {
        for n in [
            "", ".", "..", "a/b", "a\\b", "a\0b", ".x", "x.part", "x.state", "x.lock",
        ] {
            assert!(!is_safe_file_name(n), "{n:?}");
        }
        assert!(is_safe_file_name("wikipedia_de_all_nopic_2026-10.zim"));
    }

    fn target(dir: &Path) -> Target {
        Target {
            name: "f.bin".into(),
            dir: dir.into(),
            sources: vec!["http://127.0.0.1/f".into()],
            size: 100,
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn target_validation() {
        let d = tempfile::tempdir().unwrap();
        assert!(target(d.path()).validate().is_ok());
        let mut t = target(d.path());
        t.sources.clear();
        assert!(t.validate().is_err());
        let mut t = target(d.path());
        t.size = 0;
        assert!(t.validate().is_err());
        let mut t = target(d.path());
        t.sha256 = "ABC".into();
        assert!(t.validate().is_err());
        let mut t = target(d.path());
        t.name = "../evil".into();
        assert!(t.validate().is_err());
    }

    #[test]
    fn remaining_bytes_credits_existing_data() {
        let d = tempfile::tempdir().unwrap();
        let t = target(d.path());
        assert_eq!(remaining_bytes(&t), 100, "nothing on disk");

        let mut st = State::new(&t.name, t.size, &t.sha256, 40);
        st.mark_done(0);
        st.save(&t.state_path()).unwrap();
        assert_eq!(remaining_bytes(&t), 100, "state without part file is not trusted");

        std::fs::write(t.part_path(), vec![0u8; 100]).unwrap();
        assert_eq!(remaining_bytes(&t), 60);

        let other = Target {
            sha256: "1".repeat(64),
            ..t.clone()
        };
        assert_eq!(
            remaining_bytes(&other),
            100,
            "state of a different file is not trusted"
        );

        std::fs::write(t.final_path(), vec![0u8; 100]).unwrap();
        assert_eq!(remaining_bytes(&t), 0);
    }
}
