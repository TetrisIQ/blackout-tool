//! `kit spell`: the optional "did you mean" index of installed ZIM files, set per package at a
//! level of effort with an estimate of time and disk space (see `spell`). `kit install` builds the
//! indexes that are switched on and missing; the portal and the assistant read them.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::cli::{Env, Outcome, data_dir_of, make_ctx, paths_for, table};
use crate::install::{self, DEFAULT_PORT, Paths, state::StateGuard};
use crate::plan::format_bytes;
use crate::spell::{self, Estimate, Level, Meta, Settings};

#[derive(Debug, Args)]
pub struct SpellArgs {
    #[command(subcommand)]
    pub action: Option<SpellAction>,
}

#[derive(Debug, Subcommand)]
pub enum SpellAction {
    /// Show every installed ZIM: its level, its index and what each level would cost (the default)
    List {
        #[arg(long)]
        json: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Estimate the time and disk space of the levels for installed ZIMs
    Estimate {
        /// Package ids (default: every installed ZIM)
        ids: Vec<String>,
        /// Only this level
        #[arg(long)]
        level: Option<Level>,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Switch the index on for packages at a level and build it now
    Enable {
        #[arg(required = true)]
        ids: Vec<String>,
        /// titles (1): article titles; sample (2): and an eighth of the text; full (3): and all text
        #[arg(long, default_value = "titles")]
        level: Level,
        /// Only remember the choice; `kit install` or `kit spell index` builds it later
        #[arg(long)]
        no_build: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Switch the index off and delete it
    Disable {
        #[arg(required = true)]
        ids: Vec<String>,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Build the indexes that are switched on (`kit install` does this by itself; use it to repeat
    /// it after an interruption or with --force)
    Index {
        /// Package ids (default: every package that has a level)
        ids: Vec<String>,
        /// Rebuild even if the index is current
        #[arg(long)]
        force: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
}

/// One installed ZIM as `kit spell list --json` reports it (the window reads this).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub book: String,
    pub zim_bytes: u64,
    /// The level chosen for this package; none: off.
    pub level: Option<Level>,
    /// The index on disk (maybe of another level than `level`, or of an older file).
    pub index: Option<Meta>,
    /// Whether that index is the one `level` asks for, built from the installed file.
    pub current: bool,
    pub estimates: Vec<LevelEstimate>,
    /// Why no estimate could be made.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LevelEstimate {
    pub level: Level,
    #[serde(flatten)]
    pub estimate: Estimate,
}

/// `(id, name, book)` of the installed ZIMs that can have the index.
fn eligible(paths: &Paths) -> Result<Vec<(String, String, String)>> {
    let guard = StateGuard::open(paths)?;
    Ok(guard
        .packages
        .values()
        .filter_map(|r| Some((r.id.clone(), r.name.clone(), spell::book_of(r)?)))
        .collect())
}

fn zim_path(paths: &Paths, book: &str) -> PathBuf {
    paths.library_dir().join(format!("{book}.zim"))
}

fn check_ids(ids: &[String], known: &[(String, String, String)]) -> Result<()> {
    if known.is_empty() {
        bail!("no ZIM package is installed");
    }
    if let Some(bad) = ids.iter().find(|i| !known.iter().any(|k| &k.0 == *i)) {
        bail!("{bad} is not an installed ZIM package (maps have their own search)");
    }
    Ok(())
}

/// "5 min", "1 h 20 min": a rough duration for people.
pub fn fmt_secs(secs: u64) -> String {
    match secs {
        0..=59 => format!("{} s", secs.max(1)),
        60..=3599 => format!("{} min", secs.div_ceil(60)),
        _ => {
            let m = secs.div_ceil(60);
            format!("{} h {} min", m / 60, m % 60)
        }
    }
}

fn estimates_for(paths: &Paths, book: &str) -> Result<Vec<LevelEstimate>, String> {
    let zim = zim_path(paths, book);
    let cache = paths.spell_dir().join("profiles");
    Level::ALL
        .iter()
        .map(|l| {
            spell::estimate(&zim, *l, Some(&cache))
                .map(|estimate| LevelEstimate { level: *l, estimate })
                .map_err(|e| e.to_string())
        })
        .collect()
}

fn rows(paths: &Paths) -> Result<Vec<Row>> {
    let settings = Settings::load(&paths.spell_dir());
    Ok(eligible(paths)?
        .into_iter()
        .map(|(id, name, book)| {
            let zim = zim_path(paths, &book);
            let dir = spell::index_dir(&paths.spell_dir(), &book);
            let level = settings.packages.get(&id).copied();
            let (estimates, error) = match estimates_for(paths, &book) {
                Ok(e) => (e, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            Row {
                zim_bytes: std::fs::metadata(&zim).map_or(0, |m| m.len()),
                current: level.is_some_and(|l| spell::is_current(&dir, &zim, l)),
                index: spell::read_meta(&dir),
                level,
                estimates,
                error,
                id,
                name,
                book,
            }
        })
        .collect())
}

fn cost(e: &LevelEstimate) -> String {
    format!(
        "{}, {}",
        fmt_secs(e.estimate.secs),
        format_bytes(e.estimate.bytes)
    )
}

/// Builds the indexes of the packages that have a level (those in `only`, or all) and lack a
/// current one. The search works without them, so a failure is a note and not an error.
/// Returns notes to show.
pub fn build_indexes(
    ctx: &install::Ctx,
    only: &[String],
    force: bool,
    out: &mut dyn Write,
) -> Result<Vec<String>> {
    let settings = Settings::load(&ctx.paths.spell_dir());
    let todo: Vec<(String, String, Level)> = eligible(&ctx.paths)?
        .into_iter()
        .filter(|(id, _, _)| only.is_empty() || only.contains(id))
        .filter_map(|(id, _, book)| Some((id.clone(), book, *settings.packages.get(&id)?)))
        .collect();
    let (mut notes, mut changed) = (Vec::new(), false);
    let cache = ctx.paths.spell_dir().join("profiles");
    for (id, book, level) in todo {
        let zim = zim_path(&ctx.paths, &book);
        let dir = spell::index_dir(&ctx.paths.spell_dir(), &book);
        if !force && spell::is_current(&dir, &zim, level) {
            continue;
        }
        let eta = spell::estimate(&zim, level, Some(&cache)).map_or(String::new(), |e| {
            format!(", about {} and {}", fmt_secs(e.secs), format_bytes(e.bytes))
        });
        writeln!(
            out,
            "{id}: building the spelling index ({}{eta}) ...",
            level.name()
        )?;
        out.flush()?;
        let mut last = 0;
        let result = spell::build(&zim, &dir, level, &mut |done, total| {
            let pct = done * 100 / total.max(1);
            if total > 0 && pct >= last + 10 {
                last = pct - pct % 10;
                let _ = writeln!(out, "{id}: spelling index {pct}%");
            }
        });
        match result {
            Ok(m) => {
                changed = true;
                writeln!(
                    out,
                    "{id}: {} words indexed ({}, {} s)",
                    m.words,
                    format_bytes(m.bytes),
                    m.secs
                )?;
            }
            Err(e) => notes.push(format!(
                "{id}: the spelling index could not be built ({e}); the search works without it"
            )),
        }
    }
    if changed {
        refresh_portal(&ctx.paths)?;
    }
    Ok(notes)
}

/// The portal links the indexes that exist. Only its pages change; no service is touched.
fn refresh_portal(paths: &Paths) -> Result<()> {
    let guard = StateGuard::open(paths)?;
    install::portal::write(paths, &guard)?;
    Ok(())
}

/// Forgets the level and deletes the index of an uninstalled package. Does not touch the portal
/// (the caller rebuilds it).
pub fn forget(paths: &Paths, id: &str, book: &str) {
    let _ = std::fs::remove_dir_all(spell::index_dir(&paths.spell_dir(), book));
    let mut settings = Settings::load(&paths.spell_dir());
    if settings.packages.remove(id).is_some() {
        let _ = settings.save(&paths.spell_dir());
    }
}

pub fn spell_cmd(args: &SpellArgs, env: &Env, out: &mut dyn Write) -> Result<Outcome> {
    let default_action = SpellAction::List {
        json: false,
        data_dir: None,
    };
    match args.action.as_ref().unwrap_or(&default_action) {
        SpellAction::List { json, data_dir } => {
            let paths = paths_for(env, data_dir_of(data_dir)?)?;
            let rows = rows(&paths)?;
            if *json {
                serde_json::to_writer_pretty(&mut *out, &rows)?;
                writeln!(out)?;
                return Ok(Outcome::Ok);
            }
            if rows.is_empty() {
                writeln!(out, "no ZIM package is installed")?;
                return Ok(Outcome::Ok);
            }
            let mut t = vec![
                ["ID", "LEVEL", "INDEX", "TITLES", "SAMPLE", "FULL"]
                    .map(String::from)
                    .to_vec(),
            ];
            for r in &rows {
                let index = match (&r.index, r.level, r.current) {
                    (Some(m), Some(_), true) => format!("{} words, {}", m.words, format_bytes(m.bytes)),
                    (Some(_), Some(_), false) => "outdated (kit spell index)".into(),
                    (Some(_), None, _) => "unused (kit spell disable)".into(),
                    (None, Some(_), _) => "not built (kit spell index)".into(),
                    (None, None, _) => "-".into(),
                };
                let mut row = vec![r.id.clone(), r.level.map_or("off", Level::name).into(), index];
                row.extend(Level::ALL.iter().map(|l| {
                    r.estimates
                        .iter()
                        .find(|e| e.level == *l)
                        .map_or("?".into(), cost)
                }));
                t.push(row);
            }
            write!(out, "{}", table(&t, &[]))?;
            writeln!(
                out,
                "\nEstimates (time, disk) are rough. Switch on with `kit spell enable <id> --level titles|sample|full`."
            )?;
            for r in rows.iter().filter(|r| r.error.is_some()) {
                writeln!(out, "{}: {}", r.id, r.error.as_deref().unwrap_or_default())?;
            }
            Ok(Outcome::Ok)
        }
        SpellAction::Estimate { ids, level, data_dir } => {
            let paths = paths_for(env, data_dir_of(data_dir)?)?;
            let known = eligible(&paths)?;
            check_ids(ids, &known)?;
            for (id, _, book) in known.iter().filter(|k| ids.is_empty() || ids.contains(&k.0)) {
                writeln!(out, "{id}")?;
                let mut t = vec![
                    ["LEVEL", "TIME", "DISK", "WORDS", "WHAT IT READS"]
                        .map(String::from)
                        .to_vec(),
                ];
                for e in estimates_for(&paths, book).map_err(anyhow::Error::msg)? {
                    if level.is_some_and(|l| l != e.level) {
                        continue;
                    }
                    t.push(vec![
                        e.level.name().into(),
                        fmt_secs(e.estimate.secs),
                        format_bytes(e.estimate.bytes),
                        e.estimate.words.to_string(),
                        e.level.describe().into(),
                    ]);
                }
                write!(out, "{}", table(&t, &[1, 2, 3]))?;
            }
            writeln!(
                out,
                "\nThe time is for this machine's processor cores; a slow disk or a cold file takes longer."
            )?;
            Ok(Outcome::Ok)
        }
        SpellAction::Enable {
            ids,
            level,
            no_build,
            data_dir,
        } => {
            let paths = paths_for(env, data_dir_of(data_dir)?)?;
            let known = eligible(&paths)?;
            check_ids(ids, &known)?;
            let dir = paths.spell_dir();
            let mut settings = Settings::load(&dir);
            for id in ids {
                settings.packages.insert(id.clone(), *level);
                writeln!(
                    out,
                    "{id}: spelling index on at level {} ({})",
                    level.name(),
                    level.describe()
                )?;
            }
            settings.save(&dir)?;
            if !no_build {
                let port = StateGuard::open(&paths)?.port.unwrap_or(DEFAULT_PORT);
                let ctx = make_ctx(env, &paths, port);
                if let Some(note) = build_indexes(&ctx, ids, false, out)?.into_iter().next() {
                    bail!("{note}");
                }
            }
            Ok(Outcome::Ok)
        }
        SpellAction::Disable { ids, data_dir } => {
            let paths = paths_for(env, data_dir_of(data_dir)?)?;
            let known = eligible(&paths)?;
            check_ids(ids, &known)?;
            for (id, _, book) in known.iter().filter(|k| ids.contains(&k.0)) {
                forget(&paths, id, book);
                writeln!(out, "{id}: spelling index off, files deleted")?;
            }
            refresh_portal(&paths)?;
            Ok(Outcome::Ok)
        }
        SpellAction::Index { ids, force, data_dir } => {
            let paths = paths_for(env, data_dir_of(data_dir)?)?;
            check_ids(ids, &eligible(&paths)?)?;
            let port = StateGuard::open(&paths)?.port.unwrap_or(DEFAULT_PORT);
            let ctx = make_ctx(env, &paths, port);
            if let Some(note) = build_indexes(&ctx, ids, *force, out)?.into_iter().next() {
                bail!("{note}");
            }
            Ok(Outcome::Ok)
        }
    }
}
