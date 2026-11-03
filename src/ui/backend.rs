//! Everything the download manager does outside the window: reading the catalog and the
//! installed state, and running `kit install` / `kit uninstall` as child processes.
//!
//! Downloads are child processes on purpose. The engine resumes from the `.part` file after
//! `kill -9`, so "pause" is killing the child and "resume" is starting it again; progress is the
//! size of the `.part` file. The window never holds a lock the CLI needs.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::model::{Installed, Pkg, ProfileDef};
use crate::catalog::Catalog;
use crate::catalog::http::{CachedHttp, UreqHttp};
use crate::catalog::resolve::{LiveResolver, Resolver};
use crate::download;
use crate::install::DEFAULT_PORT;
use crate::install::state::State;

const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone)]
pub struct Config {
    pub catalog_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    /// The `kit` executable that installs and removes.
    pub kit_bin: PathBuf,
}

impl Config {
    pub fn discover() -> Result<Self> {
        let exe = std::env::current_exe().ok();
        let exe_dir = exe.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        let mut catalogs: Vec<PathBuf> = std::env::var_os("KIT_CATALOG")
            .map(PathBuf::from)
            .into_iter()
            .collect();
        catalogs.push("catalog".into());
        // target/release/kit-gui -> repository root
        catalogs.extend(
            exe_dir
                .iter()
                .flat_map(|d| d.ancestors().take(4).map(|a| a.join("catalog"))),
        );
        catalogs.push("/usr/share/emergency-kit/catalog".into());
        let catalog_dir = catalogs
            .into_iter()
            .find(|p| p.join("profiles").is_dir())
            .context("no catalog found; set KIT_CATALOG or start from the repository root")?;
        let cache_dir = match std::env::var_os("KIT_CACHE_DIR").filter(|v| !v.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
                Some(d) => PathBuf::from(d).join("kit"),
                None => {
                    PathBuf::from(std::env::var_os("HOME").context("$HOME is not set")?).join(".cache/kit")
                }
            },
        };
        let mut kit_bin = exe_dir
            .as_deref()
            .map(|d| d.join("kit"))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| "kit".into());
        // An unoptimised `kit` downloads about 20 times slower (hashing and copying), so a debug
        // window uses the release build next to it when there is one.
        if cfg!(debug_assertions) {
            let release = exe_dir
                .as_deref()
                .and_then(Path::parent)
                .map(|t| t.join("release/kit"))
                .filter(|p| p.is_file());
            match release {
                Some(r) => kit_bin = r,
                None => eprintln!(
                    "kit-gui: debug build; run `make build-gui` (or cargo build --release) \
                     for full download speed"
                ),
            }
        }
        Ok(Self {
            catalog_dir,
            data_dir: crate::cli::data_dir_of(
                &std::env::var_os("KIT_DATA_DIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from),
            )?,
            cache_dir,
            kit_bin,
        })
    }

    fn command(&self, offline: bool) -> Command {
        let mut c = Command::new(&self.kit_bin);
        c.arg("--catalog")
            .arg(&self.catalog_dir)
            .arg("--cache-dir")
            .arg(&self.cache_dir);
        if offline {
            c.arg("--offline");
        }
        c
    }

    fn downloads_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }
}

/// Catalog, sizes and versions, as far as they are known.
pub struct Loaded {
    pub pkgs: Vec<Pkg>,
    pub profiles: Vec<ProfileDef>,
    pub installed: BTreeMap<String, Installed>,
    pub services: Vec<ServiceInfo>,
    pub drive_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub host: String,
    /// systemd user unit, if kit manages one.
    pub unit: Option<String>,
    pub url: String,
}

/// Reads the catalog and resolves sizes. With `offline` only the cache is used, so this returns
/// at once; packages whose live size is not cached fall back to the manifest, or to 0 (unknown).
pub fn load(cfg: &Config, offline: bool) -> Result<Loaded> {
    let catalog = Catalog::load(&cfg.catalog_dir)?;
    let http = CachedHttp::new(UreqHttp::default(), cfg.cache_dir.clone(), CACHE_TTL, offline);
    let resolver = LiveResolver::new(&http);
    let pkgs = catalog
        .packages()
        .map(|m| {
            let r = resolver.resolve(m).ok();
            Pkg {
                id: m.id.clone(),
                name: m.name.clone(),
                desc: m.description.clone(),
                category: m.category.clone(),
                kind: m.kind,
                bytes: r.as_ref().map(|r| r.size_bytes).or(m.size_bytes).unwrap_or(0),
                license: m.license.spdx.clone(),
                langs: m.languages.iter().map(|l| l.to_ascii_uppercase()).collect(),
                needs: m.depends_on.clone(),
                latest: r.as_ref().and_then(|r| r.version.clone()),
                file: r
                    .as_ref()
                    .and_then(|r| r.url.as_deref())
                    .and_then(download::file_name_from_url),
                sha256: r.as_ref().and_then(|r| r.sha256.clone()),
                host: m.service.as_ref().map(|s| s.host.clone()),
            }
        })
        .collect();
    let mut profiles: Vec<ProfileDef> = catalog
        .profiles()
        .filter(|p| p.id != "test")
        .map(|p| ProfileDef {
            id: p.id.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
            ids: p.packages.clone(),
            budget_bytes: p.budget_bytes,
        })
        .collect();
    // Smallest first, as the design shows them.
    profiles.sort_by_key(|p| p.budget_bytes);
    let state = read_state(&cfg.data_dir);
    let (drive_bytes, free_bytes) = disk(&cfg.data_dir);
    Ok(Loaded {
        pkgs,
        profiles,
        installed: installed_of(&state),
        services: services_of(&state),
        drive_bytes,
        free_bytes,
    })
}

pub fn read_state(data_dir: &Path) -> State {
    std::fs::read_to_string(data_dir.join("state/installed.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn installed_of(state: &State) -> BTreeMap<String, Installed> {
    state
        .packages
        .iter()
        .map(|(id, r)| {
            (
                id.clone(),
                Installed {
                    version: r.version.clone(),
                    bytes: r.size_bytes(),
                },
            )
        })
        .collect()
}

pub fn services_of(state: &State) -> Vec<ServiceInfo> {
    let port = state.port.unwrap_or(DEFAULT_PORT);
    let mut out = Vec::new();
    if !state.library_files().is_empty() {
        out.push(ServiceInfo {
            name: "Library server".into(),
            host: format!("localhost:{port}"),
            unit: Some(crate::install::service::UNIT_NAME.into()),
            url: format!("http://localhost:{port}/"),
        });
    }
    for r in state.packages.values() {
        if let Some(s) = &r.service {
            out.push(ServiceInfo {
                name: r.name.clone(),
                host: s.host.clone(),
                unit: Some(s.unit.clone()),
                url: format!("https://{}", s.host),
            });
        }
    }
    out
}

/// `(size of the drive, free for this user)` of the filesystem holding `dir`.
pub fn disk(dir: &Path) -> (u64, u64) {
    let existing = dir.ancestors().find(|p| !p.as_os_str().is_empty() && p.exists());
    existing
        .and_then(|p| rustix::fs::statvfs(p).ok())
        .map_or((0, 0), |st| {
            (
                st.f_blocks.saturating_mul(st.f_frsize),
                st.f_bavail.saturating_mul(st.f_frsize),
            )
        })
}

/// Can the Kiwix mirrors be reached? A short TCP connect, not a download.
pub fn online() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    "download.kiwix.org:443"
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.find(|_| true))
        .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok())
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn unit_active(unit: &str) -> bool {
    systemctl(&["is-active", "--quiet", unit])
}

pub fn restart_unit(unit: &str) -> bool {
    systemctl(&["restart", unit])
}

pub fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// `kit verify`: whether it passed and its report.
pub fn health_check(cfg: &Config) -> (bool, String) {
    let out = cfg
        .command(true)
        .arg("verify")
        .arg("--data-dir")
        .arg(&cfg.data_dir)
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let tail = text.lines().last().unwrap_or("").to_string();
            (
                o.status.success(),
                if tail.is_empty() {
                    "No report.".into()
                } else {
                    tail
                },
            )
        }
        Err(e) => (false, format!("Could not run kit: {e}")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub bytes: u64,
    pub got: u64,
    pub state: JobState,
    /// An update of something installed, not a first install.
    pub update: bool,
    /// Bytes per second, smoothed.
    pub speed: f64,
    file: Option<String>,
    sha256: Option<String>,
    last: Option<(Instant, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Done {
        id: String,
        name: String,
        update: bool,
    },
    Failed {
        id: String,
        message: String,
    },
    Removed(Vec<String>),
    RemoveFailed(String),
    /// The data directory is now this one.
    Relocated(PathBuf),
    RelocateFailed(String),
}

/// What the user typed as the new place, made absolute (`~/` allowed) and checked with the same
/// rules as `kit data-dir set`. The error is meant to be shown as it is.
pub fn check_location(current: &Path, typed: &str) -> Result<PathBuf, String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Err("Enter the folder where kit should keep its data.".into());
    }
    let path = match typed.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(typed),
    };
    if !path.is_absolute() {
        return Err("Use a full path, starting with / (or ~/).".into());
    }
    crate::install::relocate::check_target(current, &path)
        .map(|()| path)
        .map_err(|e| e.to_string())
}

struct Proc {
    child: Child,
    err: Arc<Mutex<String>>,
}

impl Proc {
    fn spawn(mut cmd: Command) -> std::io::Result<Self> {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let err = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&err);
            std::thread::spawn(move || {
                // Progress lines end in \r, so the last line is whatever follows the last break.
                let (mut buf, mut line) = ([0u8; 4096], String::new());
                while let Ok(n @ 1..) = pipe.read(&mut buf) {
                    for ch in String::from_utf8_lossy(&buf[..n]).chars() {
                        if ch == '\n' || ch == '\r' {
                            if !line.trim().is_empty() {
                                *sink.lock().unwrap() = std::mem::take(&mut line).trim().to_string();
                            }
                            line.clear();
                        } else {
                            line.push(ch);
                        }
                    }
                }
            });
        }
        Ok(Self { child, err })
    }

    fn last_line(&self) -> String {
        self.err.lock().unwrap().clone()
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The download queue. One package at a time: the install step takes the state lock, and the
/// engine already downloads each file in parallel chunks.
pub struct Runner {
    cfg: Config,
    pub jobs: Vec<Job>,
    running: Option<(String, Proc)>,
    removing: Option<(Vec<String>, Proc)>,
    relocating: Option<(PathBuf, Proc)>,
    /// Failures to start a child, reported on the next poll.
    pending: Vec<Event>,
}

impl Runner {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            jobs: Vec::new(),
            running: None,
            removing: None,
            relocating: None,
            pending: Vec::new(),
        }
    }

    pub fn is_queued(&self, id: &str) -> bool {
        self.jobs.iter().any(|j| j.id == id)
    }

    pub fn is_relocating(&self) -> bool {
        self.relocating.is_some()
    }

    /// What the running move says it is doing ("copying ... (this can take a while) ...").
    pub fn relocate_status(&self) -> String {
        self.relocating
            .as_ref()
            .map_or_else(String::new, |(_, p)| p.last_line())
    }

    /// A move needs the library quiet: no download, removal or earlier move running.
    pub fn can_relocate(&self) -> bool {
        self.running.is_none() && self.removing.is_none() && self.relocating.is_none()
    }

    /// Runs `kit data-dir set <new> [--move]`. With `move_data` the installed content follows.
    pub fn relocate(&mut self, new: PathBuf, move_data: bool) {
        if !self.can_relocate() {
            return;
        }
        let mut cmd = self.cfg.command(true);
        cmd.arg("data-dir").arg("set").arg(&new);
        if move_data {
            cmd.arg("--move");
        }
        cmd.arg("--data-dir").arg(&self.cfg.data_dir);
        match Proc::spawn(cmd) {
            Ok(p) => self.relocating = Some((new, p)),
            Err(e) => self
                .pending
                .push(Event::RelocateFailed(format!("cannot start kit: {e}"))),
        }
    }

    pub fn is_removing(&self) -> bool {
        self.removing.is_some()
    }

    pub fn enqueue(&mut self, p: &Pkg, update: bool) {
        if self.is_queued(&p.id) {
            return;
        }
        self.jobs.push(Job {
            id: p.id.clone(),
            name: p.name.clone(),
            bytes: p.bytes,
            got: 0,
            state: JobState::Queued,
            update,
            speed: 0.0,
            file: p.file.clone(),
            sha256: p.sha256.clone(),
            last: None,
        });
    }

    pub fn toggle(&mut self, id: &str) {
        let Some(i) = self.jobs.iter().position(|j| j.id == id) else {
            return;
        };
        match self.jobs[i].state {
            JobState::Paused | JobState::Failed(_) => self.jobs[i].state = JobState::Queued,
            JobState::Queued | JobState::Running => self.pause_job(i),
        }
    }

    /// Drops a job from the queue and stops its child. The partial file stays for a later retry.
    pub fn cancel(&mut self, id: &str) {
        let Some(i) = self.jobs.iter().position(|j| j.id == id) else {
            return;
        };
        self.pause_job(i);
        self.jobs.remove(i);
    }

    pub fn all_paused(&self) -> bool {
        self.jobs.iter().all(|j| j.state == JobState::Paused)
    }

    pub fn pause_all(&mut self) {
        for i in 0..self.jobs.len() {
            self.pause_job(i);
        }
    }

    pub fn resume_all(&mut self) {
        for j in &mut self.jobs {
            if j.state == JobState::Paused {
                j.state = JobState::Queued;
            }
        }
    }

    fn pause_job(&mut self, i: usize) {
        if self
            .running
            .as_ref()
            .is_some_and(|(id, _)| *id == self.jobs[i].id)
            && let Some((_, mut p)) = self.running.take()
        {
            p.stop();
        }
        self.jobs[i].state = JobState::Paused;
        self.jobs[i].speed = 0.0;
        self.jobs[i].last = None;
    }

    /// Removes packages with `kit uninstall`; the downloaded files are kept for a re-install.
    pub fn remove(&mut self, ids: Vec<String>) {
        if self.removing.is_some() || ids.is_empty() {
            return;
        }
        let mut cmd = self.cfg.command(true);
        cmd.arg("uninstall")
            .args(&ids)
            .arg("--data-dir")
            .arg(&self.cfg.data_dir);
        match Proc::spawn(cmd) {
            Ok(p) => self.removing = Some((ids, p)),
            Err(e) => self
                .pending
                .push(Event::RemoveFailed(format!("cannot start kit: {e}"))),
        }
    }

    /// Call every frame: tracks the running child, starts the next job, reports what finished.
    pub fn poll(&mut self, online: bool) -> Vec<Event> {
        let mut events = std::mem::take(&mut self.pending);

        if let Some((ids, p)) = &mut self.removing
            && let Ok(Some(status)) = p.child.try_wait()
        {
            let ids = std::mem::take(ids);
            events.push(if status.success() {
                Event::Removed(ids)
            } else {
                Event::RemoveFailed(p.last_line())
            });
            self.removing = None;
        }

        if let Some((new, p)) = &mut self.relocating
            && let Ok(Some(status)) = p.child.try_wait()
        {
            let new = std::mem::take(new);
            events.push(if status.success() {
                self.cfg.data_dir = new.clone();
                Event::Relocated(new)
            } else {
                let m = p.last_line();
                Event::RelocateFailed(m.strip_prefix("error: ").unwrap_or(&m).to_string())
            });
            self.relocating = None;
        }

        if let Some((id, p)) = &mut self.running {
            let id = id.clone();
            if let Ok(Some(status)) = p.child.try_wait() {
                let msg = p.last_line();
                self.running = None;
                if let Some(i) = self.jobs.iter().position(|j| j.id == id) {
                    if status.success() {
                        let j = self.jobs.remove(i);
                        events.push(Event::Done {
                            id,
                            name: j.name,
                            update: j.update,
                        });
                    } else {
                        let msg = msg.strip_prefix("error: ").unwrap_or(&msg).to_string();
                        self.jobs[i].state = JobState::Failed(msg.clone());
                        self.jobs[i].speed = 0.0;
                        events.push(Event::Failed { id, message: msg });
                    }
                }
            } else if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                j.got = progress_of(&self.cfg.downloads_dir(), j);
                let now = Instant::now();
                match j.last {
                    Some((t, was)) if now.duration_since(t) >= Duration::from_millis(900) => {
                        let rate = j.got.saturating_sub(was) as f64 / now.duration_since(t).as_secs_f64();
                        j.speed = if j.speed == 0.0 {
                            rate
                        } else {
                            j.speed * 0.6 + rate * 0.4
                        };
                        j.last = Some((now, j.got));
                    }
                    None => j.last = Some((now, j.got)),
                    _ => {}
                }
            }
        }

        if self.running.is_none()
            && self.relocating.is_none()
            && online
            && let Some(j) = self.jobs.iter_mut().find(|j| j.state == JobState::Queued)
        {
            let mut cmd = self.cfg.command(false);
            cmd.arg("install")
                .arg("--select")
                .arg(&j.id)
                .arg("--data-dir")
                .arg(&self.cfg.data_dir);
            match Proc::spawn(cmd) {
                Ok(p) => {
                    j.state = JobState::Running;
                    j.got = progress_of(&self.cfg.downloads_dir(), j);
                    self.running = Some((j.id.clone(), p));
                }
                Err(e) => {
                    let message = format!("cannot start kit: {e}");
                    j.state = JobState::Failed(message.clone());
                    events.push(Event::Failed {
                        id: j.id.clone(),
                        message,
                    });
                }
            }
        }
        if !online {
            // No connection: stop the child; the `.part` file keeps what was fetched.
            if let Some((id, mut p)) = self.running.take() {
                p.stop();
                if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
                    j.state = JobState::Queued;
                    j.speed = 0.0;
                    j.last = None;
                }
            }
        }
        events
    }

    pub fn remaining_bytes(&self) -> u64 {
        self.jobs.iter().map(|j| j.bytes.saturating_sub(j.got)).sum()
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        // Closing the window pauses: partial files stay and resume next time.
        if let Some((_, mut p)) = self.running.take() {
            p.stop();
        }
    }
}

/// Bytes on disk for the job: the resumable partial file, or the finished one.
fn progress_of(downloads: &Path, j: &Job) -> u64 {
    let (Some(name), Some(sha256)) = (&j.file, &j.sha256) else {
        return j.got;
    };
    let target = download::Target {
        name: name.clone(),
        // `kit` keeps each package in its own folder: downloads/<id>/<file>.
        dir: downloads.join(&j.id),
        sources: Vec::new(),
        size: j.bytes,
        sha256: sha256.clone(),
    };
    j.bytes.saturating_sub(download::remaining_bytes(&target))
}
