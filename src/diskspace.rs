//! Free disk space lookup, behind a trait so plans can be tested with fake values.

use std::io;
use std::path::Path;

pub trait FreeSpace {
    /// Bytes available to an unprivileged user on the filesystem that holds (or would hold) `path`.
    fn available(&self, path: &Path) -> io::Result<u64>;
}

/// Real implementation using `statvfs(3)`.
pub struct Statvfs;

impl FreeSpace for Statvfs {
    fn available(&self, path: &Path) -> io::Result<u64> {
        // The data directory usually does not exist before the first install; ask the
        // filesystem of the closest ancestor that does.
        let existing = path
            .ancestors()
            .find(|p| !p.as_os_str().is_empty() && p.exists())
            .unwrap_or_else(|| Path::new("."));
        let st = rustix::fs::statvfs(existing).map_err(io::Error::from)?;
        Ok(st.f_bavail.saturating_mul(st.f_frsize))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_statvfs_reports_something_for_existing_and_missing_dirs() {
        let d = tempfile::tempdir().unwrap();
        assert!(Statvfs.available(d.path()).unwrap() > 0);
        let missing = d.path().join("a/b/c");
        assert!(
            Statvfs.available(&missing).unwrap() > 0,
            "falls back to the nearest existing ancestor"
        );
    }
}
