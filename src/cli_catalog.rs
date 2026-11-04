//! `kit catalog`: the catalog repositories kit reads (add, remove, update, order, check) and the
//! loading of all enabled ones.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Serialize;

use crate::catalog::Catalog;
use crate::catalog::http::Http;
use crate::catalog::remote::{self, Source};
use crate::cli::{Env, Outcome};

/// Catalog installed by a distribution package; used when no source was added.
const SYSTEM_CATALOG: &str = "/usr/share/emergency-kit/catalog";

#[derive(Debug, Args)]
pub struct CatalogArgs {
    #[command(subcommand)]
    pub action: Option<CatalogAction>,
}

#[derive(Debug, Subcommand)]
pub enum CatalogAction {
    /// Show the catalogs in priority order (the default)
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Add a catalog: a GitHub repository URL (its latest release is used), the URL of a
    /// `catalog.tar.gz` (its `.sha256` file sits next to it), or a folder on this machine
    /// (read in place on every run, so edits show up at once)
    Add {
        /// Short name (`[a-z0-9][a-z0-9_-]*`)
        name: String,
        /// URL or folder, e.g. `/home/me/blackout-tool/katalog` or `~/katalog`
        url: String,
        /// Pin the archive's SHA-256 instead of reading `<url>.sha256` (updates then need a new pin)
        #[arg(long)]
        sha256: Option<String>,
        /// Give it the highest priority (default: lowest)
        #[arg(long)]
        first: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Forget a catalog and delete its downloaded copy
    Remove {
        name: String,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Fetch the newest version of one catalog or of all enabled ones
    Update {
        name: Option<String>,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Use a catalog again
    Enable { name: String },
    /// Keep a catalog but do not read it
    Disable { name: String },
    /// Change the priority: position 1 wins over all others
    Move {
        name: String,
        #[arg(value_parser = clap::value_parser!(usize))]
        position: usize,
    },
    /// Validate a catalog directory (for the catalog repository's CI)
    Check { dir: PathBuf },
}

/// Where the catalogs are kept: `--data-dir` of the subcommand, `KIT_DATA_DIR`, the saved place.
/// Other commands' `--data-dir` does not move the catalogs.
fn data_dir(env: &Env, explicit: &Option<PathBuf>) -> Result<PathBuf> {
    let from_env = std::env::var_os("KIT_DATA_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let explicit = explicit.clone().or(from_env);
    Ok(crate::datadir::resolve(explicit.as_deref(), &env.config_dir, || {
        crate::cli::default_data_dir()
    })?
    .0)
}

/// The sources to use: the saved ones, or the system catalog / the default when there are none.
fn effective_sources(env: &Env) -> Result<Vec<Source>> {
    sources_or_default(remote::load(&env.config_dir)?)
}

fn sources_or_default(saved: Vec<Source>) -> Result<Vec<Source>> {
    if !saved.is_empty() {
        return Ok(saved);
    }
    Ok(vec![Source::new(
        remote::DEFAULT_NAME,
        remote::DEFAULT_URL,
        None,
    )?])
}

/// Loads every enabled catalog. A catalog that is not on disk yet is fetched when online.
pub fn load_catalog(explicit: Option<&Path>, env: &Env, offline: bool) -> Result<Catalog> {
    if let Some(dir) = explicit {
        return Ok(Catalog::load(dir)?);
    }
    let data = data_dir(env, &None)?;
    load_sources(None, &env.config_dir, &data, env.probe, offline)
}

/// The loading behind [`load_catalog`], also used by the window: `explicit` alone, else the
/// system catalog when nothing was added, else the enabled sources of `config_dir`.
pub fn load_sources(
    explicit: Option<&Path>,
    config_dir: &Path,
    data: &Path,
    http: &dyn Http,
    offline: bool,
) -> Result<Catalog> {
    if let Some(dir) = explicit {
        return Ok(Catalog::load(dir)?);
    }
    let saved = remote::load(config_dir)?;
    if saved.is_empty() && Path::new(SYSTEM_CATALOG).is_dir() {
        return Ok(Catalog::load(Path::new(SYSTEM_CATALOG))?);
    }
    let mut dirs = Vec::new();
    for s in sources_or_default(saved)?.into_iter().filter(|s| s.enabled) {
        let dir = match s
            .local_dir()
            .map(Path::to_path_buf)
            .or_else(|| remote::fetched_dir(data, &s.name))
        {
            Some(d) => d,
            None if offline => bail!(
                "catalog {:?} is not downloaded yet; run `kit catalog update` while online",
                s.name
            ),
            None => {
                eprintln!("fetching catalog {:?} from {} ...", s.name, s.url);
                remote::fetch(http, &s, data)
                    .with_context(|| format!("cannot fetch catalog {:?}", s.name))?
                    .dir
            }
        };
        dirs.push((s.name, dir));
    }
    if dirs.is_empty() {
        bail!("no catalog is enabled; use `kit catalog add` or `kit catalog enable`");
    }
    let catalog = Catalog::load_all(&dirs)?;
    for w in catalog.warnings() {
        eprintln!("warning: {w}");
    }
    Ok(catalog)
}

/// One line of `kit catalog list --json` (the window reads it).
#[derive(Serialize, serde::Deserialize)]
pub struct Row {
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub pinned: bool,
    pub fetched: bool,
    pub sha256: Option<String>,
    pub packages: Option<usize>,
    pub error: Option<String>,
}

fn rows(env: &Env, data: &Path) -> Result<Vec<Row>> {
    let saved = remote::load(&env.config_dir)?;
    let sources = if saved.is_empty() {
        effective_sources(env)?
    } else {
        saved
    };
    Ok(sources
        .into_iter()
        .map(|s| {
            let dir = s
                .local_dir()
                .map(Path::to_path_buf)
                .or_else(|| remote::fetched_dir(data, &s.name));
            let loaded = dir.as_deref().map(Catalog::load);
            Row {
                pinned: s.sha256.is_some(),
                fetched: dir.is_some(),
                sha256: dir.as_deref().and_then(remote::stamp),
                packages: loaded
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .map(|c| c.packages().count()),
                error: loaded.and_then(|r| r.err()).map(|e| e.to_string()),
                name: s.name,
                url: s.url,
                enabled: s.enabled,
            }
        })
        .collect())
}

pub fn catalog_cmd(args: &CatalogArgs, env: &Env, offline: bool, out: &mut dyn Write) -> Result<Outcome> {
    let default_action = CatalogAction::List {
        json: false,
        data_dir: None,
    };
    match args.action.as_ref().unwrap_or(&default_action) {
        CatalogAction::List { json, data_dir: d } => {
            let rows = rows(env, &data_dir(env, d)?)?;
            if *json {
                serde_json::to_writer_pretty(&mut *out, &rows)?;
                writeln!(out)?;
                return Ok(Outcome::Ok);
            }
            let mut table = vec![
                ["#", "NAME", "STATE", "PACKAGES", "ARCHIVE", "URL"]
                    .map(String::from)
                    .to_vec(),
            ];
            for (i, r) in rows.iter().enumerate() {
                let state = match (r.enabled, r.fetched) {
                    (false, _) => "disabled",
                    (true, false) => "not downloaded",
                    (true, true) if r.error.is_some() => "invalid",
                    (true, true) => "ok",
                };
                table.push(vec![
                    (i + 1).to_string(),
                    r.name.clone(),
                    state.into(),
                    r.packages.map_or("-".into(), |n| n.to_string()),
                    r.sha256
                        .as_deref()
                        .map_or("-".into(), |h| h[..12.min(h.len())].to_string())
                        + if r.pinned { " (pinned)" } else { "" },
                    r.url.clone(),
                ]);
            }
            write!(out, "{}", crate::cli::table(&table, &[0, 3]))?;
            for r in rows.iter().filter(|r| r.error.is_some()) {
                writeln!(out, "{}: {}", r.name, r.error.as_deref().unwrap_or_default())?;
            }
            Ok(Outcome::Ok)
        }
        CatalogAction::Add {
            name,
            url,
            sha256,
            first,
            data_dir: d,
        } => {
            if offline && !remote::is_local_location(url) {
                bail!("cannot add a catalog in offline mode");
            }
            let source = Source::new(name, url, sha256.as_deref())?;
            let mut list = remote::load(&env.config_dir)?;
            if list.is_empty() {
                // keep the default catalog when the first one is added
                list = effective_sources(env)?;
            }
            if list.iter().any(|s| s.name == *name) {
                bail!("a catalog named {name:?} exists already; remove it first");
            }
            let note = match source.local_dir() {
                Some(dir) => {
                    // Nothing is downloaded; the folder has to load as a catalog right now.
                    remote::read_meta(dir)?;
                    let n = Catalog::load(dir)?.packages().count();
                    format!("local folder {}, {n} packages", dir.display())
                }
                None => format!(
                    "archive {}",
                    &remote::fetch(env.probe, &source, &data_dir(env, d)?)?.sha256[..12]
                ),
            };
            if *first {
                list.insert(0, source);
            } else {
                list.push(source);
            }
            remote::save(&env.config_dir, &list)?;
            writeln!(out, "added {name:?} ({note})")?;
            Ok(Outcome::Ok)
        }
        CatalogAction::Remove { name, data_dir: d } => {
            let mut list = remote::load(&env.config_dir)?;
            let before = list.len();
            list.retain(|s| s.name != *name);
            if list.len() == before {
                bail!("no catalog named {name:?}");
            }
            remote::save(&env.config_dir, &list)?;
            remote::validate_name(name)?;
            let dir = remote::catalogs_root(&data_dir(env, d)?).join(name);
            if dir.is_dir() {
                std::fs::remove_dir_all(&dir).with_context(|| format!("cannot delete {}", dir.display()))?;
            }
            writeln!(out, "removed {name:?}")?;
            Ok(Outcome::Ok)
        }
        CatalogAction::Update { name, data_dir: d } => {
            if offline {
                bail!("cannot update in offline mode");
            }
            let data = data_dir(env, d)?;
            let sources: Vec<Source> = effective_sources(env)?
                .into_iter()
                .filter(|s| name.as_ref().map_or(s.enabled, |n| s.name == *n))
                .collect();
            if sources.is_empty() {
                bail!("no such catalog (or none enabled)");
            }
            let mut failed = 0;
            for s in &sources {
                if s.local_dir().is_some() {
                    writeln!(out, "{}: local folder, read in place", s.name)?;
                    continue;
                }
                match remote::fetch(env.probe, s, &data) {
                    Ok(f) if f.unchanged => writeln!(out, "{}: up to date ({})", s.name, &f.sha256[..12])?,
                    Ok(f) => writeln!(out, "{}: updated to {}", s.name, &f.sha256[..12])?,
                    Err(e) => {
                        failed += 1;
                        writeln!(out, "{}: {e}", s.name)?;
                    }
                }
            }
            if failed > 0 {
                bail!(
                    "{failed} of {} catalogs could not be updated; the old copies stay in use",
                    sources.len()
                );
            }
            Ok(Outcome::Ok)
        }
        CatalogAction::Enable { name } => set_enabled(env, name, true, out),
        CatalogAction::Disable { name } => set_enabled(env, name, false, out),
        CatalogAction::Move { name, position } => {
            let mut list = remote::load(&env.config_dir)?;
            let Some(from) = list.iter().position(|s| s.name == *name) else {
                bail!("no catalog named {name:?}");
            };
            if *position == 0 || *position > list.len() {
                bail!("position must be between 1 and {}", list.len());
            }
            let s = list.remove(from);
            list.insert(position - 1, s);
            remote::save(&env.config_dir, &list)?;
            writeln!(out, "{name:?} is now number {position}")?;
            Ok(Outcome::Ok)
        }
        CatalogAction::Check { dir } => {
            remote::read_meta(dir)?;
            let c = Catalog::load(dir)?;
            writeln!(
                out,
                "ok: {} packages, {} profiles",
                c.packages().count(),
                c.profiles().count()
            )?;
            Ok(Outcome::Ok)
        }
    }
}

fn set_enabled(env: &Env, name: &str, enabled: bool, out: &mut dyn Write) -> Result<Outcome> {
    let mut list = remote::load(&env.config_dir)?;
    let Some(s) = list.iter_mut().find(|s| s.name == name) else {
        bail!("no catalog named {name:?}");
    };
    s.enabled = enabled;
    remote::save(&env.config_dir, &list)?;
    writeln!(out, "{name:?} {}", if enabled { "enabled" } else { "disabled" })?;
    Ok(Outcome::Ok)
}
