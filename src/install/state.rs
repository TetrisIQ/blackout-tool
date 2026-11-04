//! What kit has installed (`<data-dir>/state/installed.json`). Everything else (library XML,
//! service unit, portal page) is derived from this file, which makes installs idempotent.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use rustix::fs::FlockOperation;
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use super::{InstallError, Paths, io_err};
use crate::catalog::PackageType;

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    /// File name inside `<data-dir>/library/`.
    pub name: String,
    pub size: u64,
    pub sha256: String,
    /// Where the file was linked from (`downloads/<id>/...`), if still there.
    pub source: Option<PathBuf>,
    /// The download was moved instead of hard-linked, so the library copy is the only one.
    pub moved: bool,
}

/// A running container service behind the local front.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRecord {
    /// Host name under which the front serves it.
    pub host: String,
    /// Port on 127.0.0.1 (same as inside the container).
    pub port: u16,
    /// systemd user unit generated from the Quadlet file.
    pub unit: String,
    pub image: String,
}

/// A local program (Flatpak) installed per user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRecord {
    /// Flatpak application id; `flatpak run <app_id>` starts it.
    pub app_id: String,
    /// Portal group (`arcade`, `strategy`, ...).
    pub group: String,
    /// kit installed it, so kit removes it again. An app that was there before stays.
    pub by_kit: bool,
}

/// A language model held by Ollama.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRecord {
    /// The name Ollama knows it by, e.g. `granite4.2:3b`.
    pub name: String,
    pub size: u64,
    /// kit pulled it, so kit removes it again. A model that was there before stays.
    pub by_kit: bool,
    /// The model can reason before answering; the assistant page offers a switch for it.
    #[serde(default)]
    pub thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: PackageType,
    pub version: Option<String>,
    pub license: String,
    /// Seconds since the Unix epoch.
    pub installed_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub files: Vec<FileRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRecord>,
}

impl Record {
    pub fn size_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum::<u64>() + self.model.as_ref().map_or(0, |m| m.size)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    /// Port of the content server; kept so later commands and the portal agree.
    pub port: Option<u16>,
    pub packages: BTreeMap<String, Record>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            port: None,
            packages: BTreeMap::new(),
        }
    }
}

impl State {
    /// ZIM files of every installed zim package, in a stable order (the Kiwix library).
    pub fn library_files(&self) -> Vec<&FileRecord> {
        self.packages
            .values()
            .filter(|r| r.kind == PackageType::Zim)
            .flat_map(|r| r.files.iter())
            .collect()
    }

    /// Installed local programs, in a stable order.
    pub fn apps(&self) -> Vec<(&Record, &AppRecord)> {
        self.packages
            .values()
            .filter_map(|r| r.app.as_ref().map(|a| (r, a)))
            .collect()
    }

    /// Installed language models, in a stable order.
    pub fn models(&self) -> Vec<(&Record, &ModelRecord)> {
        self.packages
            .values()
            .filter_map(|r| r.model.as_ref().map(|m| (r, m)))
            .collect()
    }

    /// Installed browser games: `files` packages unpacked into `media/games/<id>/`.
    pub fn games(&self) -> Vec<&Record> {
        self.packages
            .values()
            .filter(|r| r.kind == PackageType::Files)
            .filter(|r| r.files.first().is_some_and(|f| f.name.starts_with("games/")))
            .collect()
    }

    /// Installed container services.
    pub fn services(&self) -> Vec<(&Record, &ServiceRecord)> {
        self.packages
            .values()
            .filter_map(|r| r.service.as_ref().map(|s| (r, s)))
            .collect()
    }
}

/// The state, loaded under an exclusive lock that is held until the guard is dropped.
pub struct StateGuard {
    _lock: File,
    path: PathBuf,
    pub state: State,
}

impl StateGuard {
    pub fn open(paths: &Paths) -> Result<Self, InstallError> {
        let dir = paths.state_dir();
        fs::create_dir_all(&dir).map_err(io_err("cannot create", &dir))?;
        let lock_path = paths.lock_file();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_err("cannot open", &lock_path))?;
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(Errno::WOULDBLOCK) => return Err(InstallError::Busy(lock_path)),
            Err(e) => {
                return Err(InstallError::Io {
                    context: format!("cannot lock {}", lock_path.display()),
                    source: e.into(),
                });
            }
        }
        let path = paths.state_file();
        let state = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<State>(&bytes).map_err(|e| {
                InstallError::State(format!("{} is unreadable ({e}); not touching it", path.display()))
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(io_err("cannot read", &path)(e)),
        };
        if state.version != STATE_VERSION {
            return Err(InstallError::State(format!(
                "{} has version {}, this kit understands {STATE_VERSION}",
                path.display(),
                state.version
            )));
        }
        Ok(Self {
            _lock: lock,
            path,
            state,
        })
    }

    /// Atomic and durable: temp file, fsync, rename.
    pub fn save(&self) -> Result<(), InstallError> {
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(&self.state).map_err(|e| InstallError::State(e.to_string()))?;
        let write = || -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)
        };
        write().map_err(io_err("cannot write", &self.path))
    }
}

impl Deref for StateGuard {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}

impl DerefMut for StateGuard {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}

#[cfg(test)]
pub(crate) fn sample_record(id: &str, file: &str, size: u64) -> Record {
    Record {
        id: id.into(),
        name: format!("Package {id}"),
        kind: PackageType::Zim,
        version: Some("2026-10-01".into()),
        license: "CC-BY-SA-4.0".into(),
        installed_at: 1_790_000_000,
        category: None,
        files: vec![FileRecord {
            name: file.into(),
            size,
            sha256: "a".repeat(64),
            source: None,
            moved: false,
        }],
        service: None,
        app: None,
        model: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(d: &std::path::Path) -> Paths {
        Paths::new(d.join("data"), d.join("units")).unwrap()
    }

    #[test]
    fn fresh_state_is_empty_and_saves_and_reloads() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        {
            let mut g = StateGuard::open(&p).unwrap();
            assert!(g.packages.is_empty());
            g.packages.insert("a".into(), sample_record("a", "a.zim", 10));
            g.port = Some(6589);
            g.save().unwrap();
        }
        let g = StateGuard::open(&p).unwrap();
        assert_eq!(g.port, Some(6589));
        assert_eq!(g.packages["a"].size_bytes(), 10);
        assert_eq!(g.library_files().len(), 1);
        assert!(!p.state_dir().join("installed.json.tmp").exists());
    }

    #[test]
    fn second_holder_is_refused_until_the_first_is_dropped() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        let first = StateGuard::open(&p).unwrap();
        assert!(matches!(StateGuard::open(&p), Err(InstallError::Busy(_))));
        drop(first);
        assert!(StateGuard::open(&p).is_ok());
    }

    #[test]
    fn corrupt_state_is_never_silently_reset() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        fs::create_dir_all(p.state_dir()).unwrap();
        fs::write(p.state_file(), "{ not json").unwrap();
        match StateGuard::open(&p) {
            Err(InstallError::State(m)) => assert!(m.contains("not touching"), "{m}"),
            other => panic!("{:?}", other.err()),
        }
        assert_eq!(fs::read_to_string(p.state_file()).unwrap(), "{ not json");
    }

    #[test]
    fn unknown_version_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = paths(d.path());
        fs::create_dir_all(p.state_dir()).unwrap();
        fs::write(p.state_file(), r#"{"version":99,"port":null,"packages":{}}"#).unwrap();
        assert!(matches!(StateGuard::open(&p), Err(InstallError::State(_))));
    }
}
