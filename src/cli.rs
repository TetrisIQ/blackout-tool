//! Command line interface: `kit list | show | profiles | plan`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::catalog::Catalog;
use crate::catalog::http::{CachedHttp, Http, UreqHttp};
use crate::catalog::manifest::Manifest;
use crate::catalog::resolve::{LiveResolver, Resolved, Resolver};
use crate::diskspace::{FreeSpace, Statvfs};
use crate::download::{
    self, DownloadError, Options as DownloadOptions, Outcome as DownloadOutcome, Progress, RangeSource,
    RateLimiter, Target, UreqRange,
};
use crate::plan::{self, Selection, format_bytes, margin_for};

/// How long cached Kiwix catalog responses count as fresh.
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Parser)]
#[command(
    name = "kit",
    version,
    about = "Emergency offline kit: pick, size and install offline content"
)]
pub struct Cli {
    /// Catalog directory (default: ./catalog, then /usr/share/emergency-kit/catalog)
    #[arg(long, global = true, env = "KIT_CATALOG")]
    pub catalog: Option<PathBuf>,

    /// Never use the network; rely on cached catalog data only
    #[arg(long, global = true)]
    pub offline: bool,

    /// Where Kiwix catalog responses are cached (default: $XDG_CACHE_HOME/kit)
    #[arg(long, global = true, env = "KIT_CACHE_DIR")]
    pub cache_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List available packages
    List {
        /// Only show this category
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one package, including live size and checksum for Kiwix content
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// List install profiles
    Profiles {
        #[arg(long)]
        json: bool,
    },
    /// Compute total size for a profile or selection and check free disk space
    Plan(PlanArgs),
    /// Download (and verify) the packages of a profile or selection; re-run to resume
    Download(DownloadArgs),
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("what").required(true).args(["profile", "select"])))]
pub struct PlanArgs {
    /// Profile id (see `kit profiles`)
    #[arg(long)]
    pub profile: Option<String>,

    /// Custom selection: comma-separated package ids
    #[arg(long, value_delimiter = ',')]
    pub select: Vec<String>,

    /// Where content will be stored (default: $XDG_DATA_HOME/kit)
    #[arg(long)]
    pub data_dir: Option<PathBuf>,

    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("what").required(true).args(["profile", "select"])))]
pub struct DownloadArgs {
    /// Profile id (see `kit profiles`)
    #[arg(long)]
    pub profile: Option<String>,

    /// Custom selection: comma-separated package ids
    #[arg(long, value_delimiter = ',')]
    pub select: Vec<String>,

    /// Where content will be stored (default: $XDG_DATA_HOME/kit)
    #[arg(long, env = "KIT_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Parallel connections per file
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=64))]
    pub jobs: u64,

    /// Bandwidth limit in bytes per second; suffixes k, M, G (decimal), e.g. 5M
    #[arg(long, value_parser = parse_rate)]
    pub limit_rate: Option<u64>,

    /// Chunk size in bytes (advanced; a resumed download keeps the size it started with)
    #[arg(long, default_value_t = download::DEFAULT_CHUNK_SIZE, value_parser = clap::value_parser!(u64).range(1..))]
    pub chunk_size: u64,

    /// Attempts per chunk before giving up
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..))]
    pub retries: u32,
}

fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (digits, factor) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1_000),
        Some('m' | 'M') => (&s[..s.len() - 1], 1_000_000),
        Some('g' | 'G') => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a rate like 500k, 5M or 1048576"))?;
    n.checked_mul(factor)
        .filter(|r| *r > 0)
        .ok_or_else(|| "rate must be between 1 and 2^64 bytes per second".to_string())
}

/// Network and disk access, injectable for tests.
pub struct Env<'a> {
    pub http: &'a dyn Http,
    pub space: &'a dyn FreeSpace,
    pub ranges: &'a dyn RangeSource,
}

/// What a successful command wants the process exit code to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// `kit plan` refused: not enough free disk space.
    InsufficientSpace,
}

impl Outcome {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::InsufficientSpace => 3,
        }
    }
}

pub fn run(cli: &Cli, out: &mut dyn Write) -> Result<Outcome> {
    let cache_dir = match &cli.cache_dir {
        Some(d) => d.clone(),
        None => xdg_dir("XDG_CACHE_HOME", ".cache")?,
    };
    let http = CachedHttp::new(UreqHttp::default(), cache_dir, CACHE_TTL, cli.offline);
    let (jobs, rate) = match &cli.command {
        Command::Download(a) => (a.jobs as usize, a.limit_rate),
        _ => (1, None),
    };
    let ranges = UreqRange::new(Arc::new(RateLimiter::new(rate)), jobs);
    run_with(
        cli,
        out,
        &Env {
            http: &http,
            space: &Statvfs,
            ranges: &ranges,
        },
    )
}

/// Same as [`run`] with the network and disk lookups injected.
pub fn run_with(cli: &Cli, out: &mut dyn Write, env: &Env) -> Result<Outcome> {
    let (http, space) = (env.http, env.space);
    let catalog = load_catalog(cli.catalog.as_deref())?;
    match &cli.command {
        Command::List { category, json } => list(&catalog, category.as_deref(), *json, out)?,
        Command::Show { id, json } => show(&catalog, id, *json, http, out)?,
        Command::Profiles { json } => profiles(&catalog, *json, out)?,
        Command::Plan(args) => return plan_cmd(&catalog, args, http, space, out),
        Command::Download(args) => return download_cmd(&catalog, args, env, out),
    }
    Ok(Outcome::Ok)
}

fn xdg_dir(var: &str, home_rel: &str) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p).join("kit"));
    }
    let home = std::env::var_os("HOME").context("neither $HOME nor $XDG dirs are set")?;
    let rel = if var == "XDG_DATA_HOME" {
        ".local/share"
    } else {
        home_rel
    };
    Ok(PathBuf::from(home).join(rel).join("kit"))
}

fn load_catalog(explicit: Option<&Path>) -> Result<Catalog> {
    let dir = match explicit {
        Some(d) => d.to_path_buf(),
        None => ["catalog", "/usr/share/emergency-kit/catalog"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_dir())
            .context("no catalog found; pass --catalog <dir> or run from the repository root")?,
    };
    Ok(Catalog::load(&dir)?)
}

fn table(rows: &[Vec<String>], right_aligned: &[usize]) -> String {
    let cols = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for r in rows {
        let cells: Vec<String> = r
            .iter()
            .enumerate()
            .map(|(c, cell)| {
                if right_aligned.contains(&c) {
                    format!("{cell:>w$}", w = widths[c])
                } else {
                    format!("{cell:<w$}", w = widths[c])
                }
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn list(catalog: &Catalog, category: Option<&str>, json: bool, out: &mut dyn Write) -> Result<()> {
    let pkgs: Vec<&Manifest> = catalog
        .packages()
        .filter(|m| category.is_none_or(|c| m.category == c))
        .collect();
    if json {
        serde_json::to_writer_pretty(&mut *out, &pkgs)?;
        writeln!(out)?;
        return Ok(());
    }
    if pkgs.is_empty() {
        writeln!(out, "no packages found")?;
        return Ok(());
    }
    let mut rows = vec![
        ["ID", "TYPE", "CATEGORY", "SIZE", "LANGUAGES", "LICENSE"]
            .map(String::from)
            .to_vec(),
    ];
    for m in pkgs {
        rows.push(vec![
            m.id.clone(),
            m.kind.as_str().into(),
            m.category.clone(),
            match (m.source.kiwix.is_some(), m.size_bytes) {
                (true, _) => "live".into(),
                (false, Some(s)) => format_bytes(s),
                (false, None) => "?".into(),
            },
            if m.languages.is_empty() {
                "-".into()
            } else {
                m.languages.join(",")
            },
            m.license.spdx.clone(),
        ]);
    }
    write!(out, "{}", table(&rows, &[3]))?;
    writeln!(
        out,
        "\nSizes marked \"live\" are read from the Kiwix catalog; see `kit show <id>` or `kit plan`."
    )?;
    Ok(())
}

#[derive(Serialize)]
struct ShowJson<'a> {
    manifest: &'a Manifest,
    resolved: Option<Resolved>,
    resolve_error: Option<String>,
}

fn show(catalog: &Catalog, id: &str, json: bool, http: &dyn Http, out: &mut dyn Write) -> Result<()> {
    let m = catalog.package(id)?;
    let resolver = LiveResolver::new(http);
    let resolved = resolver.resolve(m);
    if json {
        let (resolved, resolve_error) = match resolved {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(e.to_string())),
        };
        serde_json::to_writer_pretty(
            &mut *out,
            &ShowJson {
                manifest: m,
                resolved,
                resolve_error,
            },
        )?;
        writeln!(out)?;
        return Ok(());
    }
    writeln!(out, "{} - {}", m.id, m.name)?;
    writeln!(out, "  {}", m.description)?;
    writeln!(out, "  type:       {}", m.kind.as_str())?;
    writeln!(out, "  category:   {}", m.category)?;
    if !m.languages.is_empty() {
        writeln!(out, "  languages:  {}", m.languages.join(", "))?;
    }
    write!(out, "  license:    {}", m.license.spdx)?;
    if let Some(u) = &m.license.url {
        write!(out, " ({u})")?;
    }
    writeln!(out)?;
    if let Some(a) = &m.license.attribution {
        writeln!(out, "  attribution: {a}")?;
    }
    if !m.depends_on.is_empty() {
        writeln!(out, "  depends on: {}", m.depends_on.join(", "))?;
    }
    if !m.post_install.is_empty() {
        writeln!(out, "  post-install: {}", m.post_install.join(", "))?;
    }
    if let Some(k) = &m.source.kiwix {
        writeln!(
            out,
            "  source:     Kiwix catalog {} (flavour {}, language {})",
            k.name,
            k.flavour.as_deref().unwrap_or("any"),
            k.lang.as_deref().unwrap_or("any")
        )?;
    }
    if let Some(i) = &m.source.image {
        writeln!(out, "  image:      {i}")?;
    }
    if !m.source.packages.is_empty() {
        writeln!(out, "  packages:   {}", m.source.packages.join(", "))?;
    }
    match resolved {
        Ok(r) => {
            let origin = if r.live { "live" } else { "manifest" };
            writeln!(
                out,
                "  size:       {} ({} bytes, {origin})",
                format_bytes(r.size_bytes),
                r.size_bytes
            )?;
            if let Some(h) = &r.sha256 {
                writeln!(out, "  sha256:     {h}")?;
            }
            if let Some(v) = &r.version {
                writeln!(out, "  version:    {v}")?;
            }
            if let Some(u) = &r.url {
                writeln!(out, "  url:        {u}")?;
            }
            if !r.mirrors.is_empty() {
                writeln!(out, "  mirrors:    {}", r.mirrors.len())?;
            }
        }
        Err(e) => writeln!(out, "  size:       unavailable ({e})")?,
    }
    for w in resolver.warnings() {
        writeln!(out, "warning: {w}")?;
    }
    Ok(())
}

fn profiles(catalog: &Catalog, json: bool, out: &mut dyn Write) -> Result<()> {
    let ps: Vec<_> = catalog.profiles().collect();
    if json {
        serde_json::to_writer_pretty(&mut *out, &ps)?;
        writeln!(out)?;
        return Ok(());
    }
    let mut rows = vec![
        ["ID", "BUDGET", "PACKAGES", "DESCRIPTION"]
            .map(String::from)
            .to_vec(),
    ];
    for p in ps {
        rows.push(vec![
            p.id.clone(),
            format_bytes(p.budget_bytes),
            p.packages.len().to_string(),
            p.description.clone(),
        ]);
    }
    write!(out, "{}", table(&rows, &[1, 2]))?;
    writeln!(
        out,
        "\nUse `kit plan --profile <id>` or `kit plan --select id,id,...` for a custom set."
    )?;
    Ok(())
}

fn selection_from(catalog: &Catalog, profile: Option<&str>, select: &[String]) -> Result<Selection> {
    match profile {
        Some(id) => {
            let p = catalog.profile(id)?;
            Ok(Selection {
                label: format!("profile {} ({})", p.id, p.name),
                ids: p.packages.clone(),
                budget_bytes: Some(p.budget_bytes),
            })
        }
        None => {
            if select.iter().any(|s| s.trim().is_empty()) {
                bail!("--select contains an empty package id");
            }
            Ok(Selection {
                label: "custom selection".into(),
                ids: select.to_vec(),
                budget_bytes: None,
            })
        }
    }
}

/// Prints one status line per second on a terminal, or every 10 s when output is a log.
struct StderrProgress {
    tty: bool,
    inner: Mutex<ProgressInner>,
}

struct ProgressInner {
    started: Instant,
    last_print: Option<Instant>,
    first_done: Option<u64>,
}

impl StderrProgress {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            tty: std::io::stderr().is_terminal(),
            inner: Mutex::new(ProgressInner {
                started: Instant::now(),
                last_print: None,
                first_done: None,
            }),
        }
    }
}

impl Progress for StderrProgress {
    fn on_progress(&self, done: u64, total: u64) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let first = *st.first_done.get_or_insert(done);
        let every = Duration::from_secs(if self.tty { 1 } else { 10 });
        let finished = done >= total;
        if !finished && st.last_print.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        st.last_print = Some(Instant::now());
        let secs = st.started.elapsed().as_secs_f64().max(0.001);
        let speed = (done.saturating_sub(first)) as f64 / secs;
        let line = format!(
            "  {:5.1}%  {} / {}  {}/s",
            done as f64 * 100.0 / total as f64,
            format_bytes(done),
            format_bytes(total),
            format_bytes(speed as u64)
        );
        if self.tty {
            eprint!("\r{line:<70}");
        } else {
            eprintln!("{line}");
        }
    }

    fn on_verify(&self) {
        if self.tty {
            eprint!("\r{:<70}\r", "");
        }
        eprintln!("  verifying checksum...");
    }
}

fn download_cmd(catalog: &Catalog, args: &DownloadArgs, env: &Env, out: &mut dyn Write) -> Result<Outcome> {
    let selection = selection_from(catalog, args.profile.as_deref(), &args.select)?;
    let data_dir = match &args.data_dir {
        Some(d) => d.clone(),
        None => xdg_dir("XDG_DATA_HOME", "")?,
    };
    let resolver = LiveResolver::new(env.http);
    let plan = plan::build(catalog, &selection, &resolver, env.space, &data_dir)?;
    let downloads = data_dir.join("downloads");

    let mut jobs = Vec::new();
    for item in &plan.items {
        let (Some(url), Some(sha256)) = (&item.url, &item.sha256) else {
            writeln!(
                out,
                "skipping {}: no direct download (handled by its installer)",
                item.id
            )?;
            continue;
        };
        let name = download::file_name_from_url(url)
            .with_context(|| format!("{}: cannot derive a file name from {url}", item.id))?;
        let mut sources = vec![url.clone()];
        sources.extend(item.mirrors.iter().cloned());
        let target = Target {
            name,
            dir: downloads.join(&item.id),
            sources,
            size: item.size_bytes,
            sha256: sha256.clone(),
        };
        jobs.push((item, target));
    }

    let remaining: u64 = jobs.iter().map(|(_, t)| download::remaining_bytes(t)).sum();
    let needed = remaining.saturating_add(margin_for(remaining));
    let free = env
        .space
        .available(&downloads)
        .with_context(|| format!("cannot determine free space for {}", downloads.display()))?;
    writeln!(
        out,
        "{}: {} file(s), {} still to download, {} free in {}",
        plan.label,
        jobs.len(),
        format_bytes(remaining),
        format_bytes(free),
        downloads.display()
    )?;
    for w in &plan.warnings {
        writeln!(out, "warning: {w}")?;
    }
    if free < needed {
        writeln!(
            out,
            "INSUFFICIENT DISK SPACE: need {} (including a {}% margin), short by {}. Nothing was downloaded.",
            format_bytes(needed),
            plan::MARGIN_PERCENT,
            format_bytes(needed - free)
        )?;
        return Ok(Outcome::InsufficientSpace);
    }

    let opts = DownloadOptions {
        jobs: args.jobs as usize,
        chunk_size: args.chunk_size,
        retries: args.retries,
        ..DownloadOptions::default()
    };
    let total = jobs.len();
    for (n, (item, target)) in jobs.iter().enumerate() {
        writeln!(
            out,
            "[{}/{}] {} ({})",
            n + 1,
            total,
            item.id,
            format_bytes(target.size)
        )?;
        out.flush()?;
        let progress = StderrProgress::new();
        match download::download(target, &opts, env.ranges, &progress) {
            Ok(DownloadOutcome::AlreadyPresent) => writeln!(out, "  already downloaded and verified")?,
            Ok(DownloadOutcome::Downloaded { resumed_bytes }) if resumed_bytes > 0 => writeln!(
                out,
                "  done, verified (resumed after {})",
                format_bytes(resumed_bytes)
            )?,
            Ok(DownloadOutcome::Downloaded { .. }) => writeln!(out, "  done, verified")?,
            Err(e @ DownloadError::NoSpace(_)) => bail!("{}: {e}", item.id),
            Err(e) => bail!("{}: {e}", item.id),
        }
    }
    writeln!(out, "All downloads complete: {}", downloads.display())?;
    Ok(Outcome::Ok)
}

fn plan_cmd(
    catalog: &Catalog,
    args: &PlanArgs,
    http: &dyn Http,
    space: &dyn FreeSpace,
    out: &mut dyn Write,
) -> Result<Outcome> {
    let selection = selection_from(catalog, args.profile.as_deref(), &args.select)?;
    let data_dir = match &args.data_dir {
        Some(d) => d.clone(),
        None => xdg_dir("XDG_DATA_HOME", "")?,
    };
    let resolver = LiveResolver::new(http);
    let plan = plan::build(catalog, &selection, &resolver, space, &data_dir)?;
    if args.json {
        serde_json::to_writer_pretty(
            &mut *out,
            &serde_json::json!({ "plan": plan, "sufficient": plan.sufficient() }),
        )?;
        writeln!(out)?;
    } else {
        write!(out, "{}", plan::render(&plan))?;
    }
    Ok(if plan.sufficient() {
        Outcome::Ok
    } else {
        Outcome::InsufficientSpace
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use crate::catalog::kiwix::{DEFAULT_BASE, KiwixClient};
    use crate::catalog::manifest::KiwixRef;
    use std::io;

    struct FixedSpace(u64);
    impl FreeSpace for FixedSpace {
        fn available(&self, _: &Path) -> io::Result<u64> {
            Ok(self.0)
        }
    }

    /// Range source for tests that must never download anything.
    struct NoRanges;
    impl RangeSource for NoRanges {
        fn read_range(
            &self,
            url: &str,
            _: u64,
            _: u64,
            _: u64,
            _: &mut Vec<u8>,
        ) -> Result<(), download::FetchError> {
            Err(download::FetchError::SourceUnusable(format!(
                "unexpected download of {url}"
            )))
        }
    }

    const OPDS: &str = include_str!("../tests/fixtures/kiwix/opds_wikipedia_de_all.xml");
    const META_NOPIC: &str = include_str!("../tests/fixtures/kiwix/meta4_wikipedia_de_all_nopic_2026-10.xml");
    const META_MINI: &str = include_str!("../tests/fixtures/kiwix/meta4_wikipedia_de_all_mini_2026-10.xml");
    const SHA: &str = "0e281a87af92a798297fc671f8b37004cb7874ac051e13876b7335be96649e59";

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn kiwix_yaml(id: &str, flavour: &str) -> String {
        format!(
            "id: {id}\nname: Wikipedia DE {flavour}\ndescription: d\ncategory: wikipedia\ntype: zim\n\
             source:\n  kiwix: {{name: wikipedia_de_all, flavour: {flavour}, lang: deu}}\n\
             license: {{spdx: CC-BY-SA-4.0}}\nlanguages: [de]\n"
        )
    }

    /// Catalog with two Kiwix packages, one static package that depends on the first, and profiles.
    fn catalog_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "wikipedia/wiki_nopic.yaml",
            &kiwix_yaml("wiki_nopic", "nopic"),
        );
        write(
            d.path(),
            "wikipedia/wiki_mini.yaml",
            &kiwix_yaml("wiki_mini", "mini"),
        );
        write(
            d.path(),
            "games/game.yaml",
            &format!(
                "id: game\nname: Game\ndescription: d\ncategory: games\ntype: files\n\
                 source: {{url: 'https://example.org/game.tar'}}\nsha256: {SHA}\nsize_bytes: 1000000000\n\
                 license: {{spdx: GPL-3.0-or-later}}\ndepends_on: [wiki_mini]\n"
            ),
        );
        write(
            d.path(),
            "profiles/small.yaml",
            "id: small\nname: Small\ndescription: tiny\nbudget_bytes: 10000000000\npackages: [wiki_nopic, game]\n",
        );
        d
    }

    fn http() -> FakeHttp {
        let probe = FakeHttp::default();
        let url = KiwixClient::new(&probe, DEFAULT_BASE).query_url(&KiwixRef {
            name: "wikipedia_de_all".into(),
            flavour: None,
            lang: Some("deu".into()),
        });
        FakeHttp::default()
            .with(&url, OPDS)
            .with(
                "https://lb.download.kiwix.org/zim/wikipedia/wikipedia_de_all_nopic_2026-10.zim.meta4",
                META_NOPIC,
            )
            .with(
                "https://lb.download.kiwix.org/zim/wikipedia/wikipedia_de_all_mini_2026-10.zim.meta4",
                META_MINI,
            )
    }

    fn exec(dir: &Path, args: &[&str], http: &dyn Http, free: u64) -> (Result<Outcome>, String) {
        let mut argv = vec!["kit", "--catalog", dir.to_str().unwrap()];
        argv.extend_from_slice(args);
        let cli = Cli::try_parse_from(argv).unwrap();
        let mut out = Vec::new();
        let r = run_with(
            &cli,
            &mut out,
            &Env {
                http,
                space: &FixedSpace(free),
                ranges: &NoRanges,
            },
        );
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn plan_profile_prints_correct_size_table() {
        let d = catalog_dir();
        let (r, out) = exec(
            d.path(),
            &["plan", "--profile", "small", "--data-dir", "/data"],
            &http(),
            u64::MAX,
        );
        assert_eq!(r.unwrap(), Outcome::Ok);
        assert!(out.contains("wiki_nopic"), "{out}");
        assert!(out.contains("18.6 GB"), "{out}");
        assert!(out.contains("wiki_mini (dependency)"), "dependency listed: {out}");
        assert!(out.contains("4.7 GB"), "{out}");
        assert!(out.contains("1.0 GB"), "{out}");
        // 18585337585 + 4686161304 + 1000000000 = 24271498889 packages; reserve = largest (18585337585)
        assert!(out.contains("24.3 GB"), "{out}");
        assert!(out.contains("OK: fits"), "{out}");
    }

    #[test]
    fn plan_refuses_when_disk_is_too_small() {
        let d = catalog_dir();
        let (r, out) = exec(d.path(), &["plan", "--profile", "small"], &http(), 25_000_000_000);
        assert_eq!(r.unwrap(), Outcome::InsufficientSpace);
        assert_eq!(Outcome::InsufficientSpace.exit_code(), 3);
        assert!(out.contains("INSUFFICIENT DISK SPACE"), "{out}");
    }

    #[test]
    fn plan_json_is_machine_readable() {
        let d = catalog_dir();
        let (r, out) = exec(
            d.path(),
            &["plan", "--select", "wiki_mini", "--json"],
            &http(),
            u64::MAX,
        );
        assert_eq!(r.unwrap(), Outcome::Ok);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["sufficient"], true);
        assert_eq!(v["plan"]["total_bytes"], 4_686_161_304u64);
        assert_eq!(v["plan"]["items"][0]["type"], "zim");
    }

    #[test]
    fn plan_fails_cleanly_when_sizes_cannot_be_read() {
        let d = catalog_dir();
        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let (r, _) = exec(d.path(), &["plan", "--select", "wiki_nopic"], &down, u64::MAX);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(
            msg.contains("cannot determine the size") && msg.contains("wiki_nopic"),
            "{msg}"
        );
    }

    #[test]
    fn plan_static_only_selection_needs_no_network() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "a.yaml",
            &format!(
                "id: a\nname: A\ndescription: d\ncategory: c\ntype: files\nsource: {{url: 'https://e.org/a'}}\n\
                 sha256: {SHA}\nsize_bytes: 10\nlicense: {{spdx: CC0-1.0}}\n"
            ),
        );
        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let (r, _) = exec(d.path(), &["plan", "--select", "a"], &down, u64::MAX);
        assert_eq!(r.unwrap(), Outcome::Ok);
        assert_eq!(down.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn plan_unknown_profile_and_package_are_errors() {
        let d = catalog_dir();
        let (r, _) = exec(d.path(), &["plan", "--profile", "nope"], &http(), u64::MAX);
        assert!(format!("{:#}", r.unwrap_err()).contains("unknown profile"));
        let (r, _) = exec(d.path(), &["plan", "--select", "nope"], &http(), u64::MAX);
        assert!(format!("{:#}", r.unwrap_err()).contains("unknown package"));
    }

    #[test]
    fn rate_parsing() {
        assert_eq!(parse_rate("1048576"), Ok(1_048_576));
        assert_eq!(parse_rate("500k"), Ok(500_000));
        assert_eq!(parse_rate("5M"), Ok(5_000_000));
        assert_eq!(parse_rate("2g"), Ok(2_000_000_000));
        for bad in ["", "0", "fast", "-5", "5MB", "99999999999999999999g"] {
            assert!(parse_rate(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn download_refuses_without_enough_space_and_fetches_nothing() {
        let d = catalog_dir();
        let (r, out) = exec(
            d.path(),
            &["download", "--profile", "small", "--data-dir", "/data"],
            &http(),
            1000,
        );
        assert_eq!(r.unwrap(), Outcome::InsufficientSpace);
        assert!(
            out.contains("INSUFFICIENT DISK SPACE") && out.contains("Nothing was downloaded"),
            "{out}"
        );
    }

    #[test]
    fn download_arguments_are_validated() {
        assert!(Cli::try_parse_from(["kit", "download"]).is_err());
        assert!(Cli::try_parse_from(["kit", "download", "--select", "a", "--jobs", "0"]).is_err());
        assert!(
            Cli::try_parse_from([
                "kit",
                "download",
                "--select",
                "a",
                "--limit-rate",
                "5M",
                "--jobs",
                "8"
            ])
            .is_ok()
        );
    }

    #[test]
    fn plan_needs_exactly_one_of_profile_or_select() {
        assert!(Cli::try_parse_from(["kit", "plan"]).is_err());
        assert!(Cli::try_parse_from(["kit", "plan", "--profile", "a", "--select", "b"]).is_err());
        assert!(Cli::try_parse_from(["kit", "plan", "--select", "a,b"]).is_ok());
    }

    #[test]
    fn list_shows_packages_and_filters_by_category() {
        let d = catalog_dir();
        let (r, out) = exec(d.path(), &["list"], &http(), 0);
        r.unwrap();
        assert!(
            out.contains("wiki_nopic") && out.contains("game") && out.contains("live"),
            "{out}"
        );
        assert!(out.contains("1.0 GB"), "static size shown: {out}");
        let (_, out) = exec(d.path(), &["list", "--category", "games"], &http(), 0);
        assert!(out.contains("game") && !out.contains("wiki_nopic"), "{out}");
        let (_, out) = exec(d.path(), &["list", "--json"], &http(), 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn show_prints_live_data() {
        let d = catalog_dir();
        let (r, out) = exec(d.path(), &["show", "wiki_nopic"], &http(), 0);
        r.unwrap();
        assert!(
            out.contains("59af34df2f21e1824d96a534cfef0a7776b589ee215ad2a7f7bb3b6328ca30ba"),
            "{out}"
        );
        assert!(out.contains("18585337585 bytes, live"), "{out}");
        assert!(out.contains("CC-BY-SA-4.0"));
    }

    #[test]
    fn show_degrades_gracefully_without_network() {
        let d = catalog_dir();
        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let (r, out) = exec(d.path(), &["show", "wiki_nopic"], &down, 0);
        r.unwrap();
        assert!(out.contains("size:       unavailable"), "{out}");
        let (r, _) = exec(d.path(), &["show", "ghost"], &down, 0);
        assert!(r.is_err());
    }

    #[test]
    fn profiles_listing() {
        let d = catalog_dir();
        let (r, out) = exec(d.path(), &["profiles"], &http(), 0);
        r.unwrap();
        assert!(out.contains("small") && out.contains("10.0 GB"), "{out}");
    }

    #[test]
    fn broken_catalog_is_reported() {
        let d = catalog_dir();
        write(d.path(), "bad.yaml", "id: bad\n");
        let (r, _) = exec(d.path(), &["list"], &http(), 0);
        assert!(format!("{:#}", r.unwrap_err()).contains("bad.yaml"));
    }
}
