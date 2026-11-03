//! Phase 3: package installers. `zim` packages are linked into a library, listed in a Kiwix
//! library XML and served by a systemd user service. All state lives in
//! `<data-dir>/state/installed.json`; everything else is reconciled from it.

pub mod collection;
pub mod container;
pub mod flatpak;
pub mod map;
pub mod media;
pub mod model;
pub mod portal;
pub mod reconcile;
pub mod relocate;
pub mod runner;
pub mod service;
pub mod state;
pub mod tools;
pub mod zim;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::catalog::PackageType;
use crate::catalog::http::Http;
use runner::CommandRunner;
use state::{Record, State, StateGuard};

/// Default port of the content server (the roadmap's 8080 collides with too many dev servers).
pub const DEFAULT_PORT: u16 = 6589;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("`{tool}` not found. {hint}")]
    MissingTool { tool: String, hint: String },
    #[error("{context}: {source}")]
    Io { context: String, source: std::io::Error },
    #[error("`{command}` failed: {stderr}")]
    Command { command: String, stderr: String },
    #[error("{0}")]
    State(String),
    #[error("another kit process is changing the installation ({})", .0.display())]
    Busy(PathBuf),
    #[error("port {0} on 127.0.0.1 is already in use; choose another with --port")]
    PortInUse(u16),
    #[error("the content server did not start; see: journalctl --user -u kit-kiwix.service")]
    ServiceFailed,
    #[error("{} exists but was not written by kit; refusing to overwrite it", .0.display())]
    ForeignUnit(PathBuf),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Corrupt(String),
    #[error("paths must be valid UTF-8: {0}")]
    BadPath(String),
}

pub(crate) fn io_err(context: &str, path: &Path) -> impl FnOnce(std::io::Error) -> InstallError {
    let context = format!("{context} {}", path.display());
    move |source| InstallError::Io { context, source }
}

/// Every location kit writes to. All derived from two roots, so tests can use temp dirs.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    /// Directory for systemd user units (`~/.config/systemd/user`).
    pub unit_dir: PathBuf,
    /// Podman Quadlet files (`~/.config/containers/systemd`); systemd turns them into units.
    pub quadlet_dir: PathBuf,
}

impl Paths {
    pub fn new(data_dir: PathBuf, unit_dir: PathBuf) -> Result<Self, InstallError> {
        for p in [&data_dir, &unit_dir] {
            if p.to_str().is_none() {
                return Err(InstallError::BadPath(p.display().to_string()));
            }
        }
        let quadlet_dir = data_dir.join("quadlet");
        Ok(Self {
            data_dir,
            unit_dir,
            quadlet_dir,
        })
    }

    pub fn with_quadlet_dir(mut self, dir: PathBuf) -> Result<Self, InstallError> {
        if dir.to_str().is_none() {
            return Err(InstallError::BadPath(dir.display().to_string()));
        }
        self.quadlet_dir = dir;
        Ok(self)
    }

    pub fn services_dir(&self) -> PathBuf {
        self.data_dir.join("services")
    }
    /// Writable data of one service.
    pub fn service_data_dir(&self, id: &str) -> PathBuf {
        self.services_dir().join(id).join("data")
    }
    pub fn credentials_file(&self, id: &str) -> PathBuf {
        self.services_dir().join(id).join("credentials.json")
    }
    pub fn media_dir(&self, media: &str) -> PathBuf {
        self.data_dir.join("media").join(media)
    }
    /// Map regions (`<id>.pmtiles`); the portal links them next to the viewer.
    pub fn maps_dir(&self) -> PathBuf {
        self.data_dir.join("maps")
    }
    pub fn quadlet_file(&self, id: &str) -> PathBuf {
        self.quadlet_dir.join(format!("kit-{id}.container"))
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }
    pub fn library_dir(&self) -> PathBuf {
        self.data_dir.join("library")
    }
    pub fn library_xml(&self) -> PathBuf {
        self.library_dir().join("library.xml")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.data_dir.join("state")
    }
    pub fn state_file(&self) -> PathBuf {
        self.state_dir().join("installed.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.state_dir().join("installed.lock")
    }
    pub fn portal_dir(&self) -> PathBuf {
        self.data_dir.join("portal")
    }
    pub fn unit_file(&self) -> PathBuf {
        self.unit_dir.join(service::UNIT_NAME)
    }
}

/// Everything an installer needs from the outside world.
pub struct Ctx<'a> {
    pub paths: Paths,
    pub runner: &'a dyn CommandRunner,
    /// Used by `verify` to ask the running server what it serves.
    pub probe: &'a dyn Http,
    /// Directories searched for `kiwix-serve`, `systemctl`, ...
    pub search_path: Vec<PathBuf>,
    pub port: u16,
    pub user: Option<String>,
    /// How long to wait for the service to become active after (re)starting it.
    pub start_wait: Duration,
    /// Seconds since the Unix epoch, injectable for tests.
    pub now: u64,
    /// Check that the port is free before starting the service (off in tests that would collide).
    pub check_port: bool,
}

/// A downloaded package, ready to be installed.
#[derive(Debug, Clone)]
pub struct InstallInput {
    pub id: String,
    pub name: String,
    pub license: String,
    pub version: Option<String>,
    /// The verified download (`downloads/<id>/<file>`).
    pub source: PathBuf,
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
    /// The download engine verified the checksum in this run, so the installer need not hash again.
    pub verified: bool,
    /// `container` packages: the digest-pinned image reference.
    pub image: Option<String>,
    pub service: Option<crate::catalog::manifest::ServiceSpec>,
    /// `files` packages feeding a service: where and how they are unpacked.
    pub content: Option<crate::catalog::manifest::ContentSpec>,
    /// `flatpak` packages: the Flathub application id and the portal group.
    pub flatpak: Option<String>,
    pub group: Option<String>,
    /// `collection` packages: the site to fetch book by book.
    pub collection: Option<crate::catalog::manifest::CollectionRef>,
    /// `map` packages: the region to cut out.
    pub map: Option<crate::catalog::manifest::MapRef>,
    /// `model` packages: the Ollama model and the size the registry reported.
    pub ollama: Option<crate::catalog::manifest::OllamaRef>,
    /// `model` packages: what the assistant page may ask of it (reasoning).
    pub model_spec: Option<crate::catalog::manifest::ModelSpec>,
    /// Catalog category, kept in the record so the portal can tell survival books from the rest.
    pub category: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub trait Installer {
    /// Dry run: what `install` would do. No side effects.
    fn plan(&self, input: &InstallInput, ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError>;
    /// Idempotent. Returns hints for the user.
    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError>;
    /// Read-only health checks of one installed package.
    fn verify(&self, record: &Record, ctx: &Ctx, deep: bool) -> Vec<Check>;
    /// Idempotent; an unknown id is not an error.
    fn uninstall(
        &self,
        id: &str,
        ctx: &Ctx,
        state: &mut StateGuard,
        purge: bool,
    ) -> Result<Vec<String>, InstallError>;
}

pub fn installer_for(kind: PackageType) -> Result<Box<dyn Installer>, InstallError> {
    match kind {
        PackageType::Zim => Ok(Box::new(zim::ZimInstaller)),
        PackageType::Container => Ok(Box::new(container::ContainerInstaller)),
        PackageType::Files => Ok(Box::new(media::MediaInstaller)),
        PackageType::Flatpak => Ok(Box::new(flatpak::FlatpakInstaller)),
        PackageType::Map => Ok(Box::new(map::MapInstaller)),
        PackageType::Collection => Ok(Box::new(collection::CollectionInstaller)),
        PackageType::Model => Ok(Box::new(model::ModelInstaller)),
        other => Err(InstallError::Unsupported(format!(
            "packages of type {} cannot be installed yet",
            other.as_str()
        ))),
    }
}
