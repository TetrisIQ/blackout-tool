//! Where kit keeps its data. Order: `--data-dir` / `KIT_DATA_DIR`, then the location saved by
//! `kit data-dir set`, then `$XDG_DATA_HOME/kit`. The saved location is a small JSON file in
//! `<config>/kit/config.json`, so every kit program (CLI, window) agrees without environment variables.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `--data-dir` or `KIT_DATA_DIR`.
    Explicit,
    /// Saved by `kit data-dir set`.
    Saved,
    Default,
}

/// The saved location, if any. A missing file is not an error; a damaged one is.
pub fn load(config_dir: &Path) -> io::Result<Option<PathBuf>> {
    let path = config_dir.join(CONFIG_FILE);
    match fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str::<Config>(&t)
            .map(|c| c.data_dir)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Atomic: a crash leaves the old or the new file, never half of one.
pub fn save(config_dir: &Path, data_dir: &Path) -> io::Result<()> {
    fs::create_dir_all(config_dir)?;
    let path = config_dir.join(CONFIG_FILE);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(&Config {
        data_dir: Some(data_dir.to_path_buf()),
    })
    .map_err(io::Error::other)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)
}

/// Forgets the saved location (back to the default).
pub fn clear(config_dir: &Path) -> io::Result<()> {
    match fs::remove_file(config_dir.join(CONFIG_FILE)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

pub fn resolve(
    explicit: Option<&Path>,
    config_dir: &Path,
    default: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<(PathBuf, Source)> {
    if let Some(d) = explicit {
        return Ok((d.to_path_buf(), Source::Explicit));
    }
    if let Some(d) = load(config_dir)? {
        return Ok((d, Source::Saved));
    }
    Ok((default()?, Source::Default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_beats_saved_beats_default() {
        let d = tempfile::tempdir().unwrap();
        let default = || Ok(PathBuf::from("/default"));
        assert_eq!(
            resolve(None, d.path(), default).unwrap(),
            (PathBuf::from("/default"), Source::Default)
        );
        save(d.path(), Path::new("/mnt/disk/kit")).unwrap();
        assert_eq!(
            resolve(None, d.path(), default).unwrap(),
            (PathBuf::from("/mnt/disk/kit"), Source::Saved)
        );
        assert_eq!(
            resolve(Some(Path::new("/tmp/x")), d.path(), default).unwrap(),
            (PathBuf::from("/tmp/x"), Source::Explicit)
        );
        clear(d.path()).unwrap();
        clear(d.path()).unwrap();
        assert_eq!(resolve(None, d.path(), default).unwrap().1, Source::Default);
    }

    #[test]
    fn a_damaged_config_is_reported_not_ignored() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(CONFIG_FILE), "{\"data_dir\": 5}").unwrap();
        assert!(load(d.path()).is_err());
        fs::write(d.path().join(CONFIG_FILE), "{\"other\": 1}").unwrap();
        assert!(load(d.path()).is_err(), "unknown keys are rejected");
    }
}
