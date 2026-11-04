//! `kit archive`: crawl a website and turn it into a local ZIM (like the Wayback Machine, but
//! served by Kiwix). The crawl is done by `zimit` (Browsertrix + warc2zim) in a rootless Podman
//! container; the finished ZIM is installed like any downloaded book.

use std::fs;
use std::path::{Path, PathBuf};

use crate::download;
use crate::install::state::StateGuard;
use crate::install::tools::require;
use crate::install::zim::ZimInstaller;
use crate::install::{Ctx, InstallError, InstallInput, Installer};

pub const DEFAULT_IMAGE: &str = "ghcr.io/openzim/zimit:3.1.3";

#[derive(Debug, Clone)]
pub struct ArchiveSpec {
    pub url: String,
    pub id: String,
    pub name: String,
    pub description: String,
    pub license: String,
    pub image: String,
    pub workers: u32,
    pub page_limit: Option<u32>,
    /// Extra Browsertrix scope regexes to skip (search pages, logins, ...).
    pub exclude: Vec<String>,
    /// Static sites crawl much faster with `domcontentloaded`.
    pub wait_until: String,
}

impl ArchiveSpec {
    /// Rule 6 (license) and a sane id/url, checked before a crawl that can take hours.
    pub fn validate(&self) -> Result<(), InstallError> {
        let bad = |m: String| Err(InstallError::Unsupported(m));
        let id_ok = self
            .id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !id_ok {
            return bad(format!("id `{}` must match [a-z0-9][a-z0-9_-]*", self.id));
        }
        if !(self.url.starts_with("https://") || self.url.starts_with("http://"))
            || self.url.contains(char::is_whitespace)
        {
            return bad(format!("`{}` is not an http(s) URL", self.url));
        }
        if self.license.trim().is_empty() {
            return bad(
                "a license is required (e.g. --license CC-BY-4.0, or --license \"personal copy\" \
                        for your own use); kit does not archive content without one"
                    .into(),
            );
        }
        if self.name.trim().is_empty() || self.workers == 0 {
            return bad("name must not be empty and workers must be at least 1".into());
        }
        Ok(())
    }

    /// Arguments for `podman run`; `work` is mounted as `/output`.
    pub fn podman_args(&self, work: &Path) -> Vec<String> {
        let mut a: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-v".into(),
            format!("{}:/output:Z", work.display()),
            self.image.clone(),
            "zimit".into(),
            "--seeds".into(),
            self.url.clone(),
            "--name".into(),
            self.id.clone(),
            "--title".into(),
            self.name.clone(),
            "--description".into(),
            self.description.clone(),
            "--scopeType".into(),
            "prefix".into(),
            "--workers".into(),
            self.workers.to_string(),
            "--waitUntil".into(),
            self.wait_until.clone(),
        ];
        if let Some(n) = self.page_limit {
            a.extend(["--pageLimit".into(), n.to_string()]);
        }
        for rx in &self.exclude {
            a.extend(["--scopeExcludeRx".into(), rx.clone()]);
        }
        a
    }
}

/// The ZIM zimit left in `work` (it names it `<name>_<date>.zim`).
pub fn find_zim(work: &Path) -> Result<PathBuf, InstallError> {
    let io = |e| InstallError::Io {
        context: format!("cannot read {}", work.display()),
        source: e,
    };
    let mut zims: Vec<PathBuf> = fs::read_dir(work)
        .map_err(io)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "zim"))
        .collect();
    zims.sort();
    zims.pop().ok_or_else(|| {
        InstallError::Corrupt(format!(
            "the crawl produced no .zim in {} (WARC files are kept there for inspection)",
            work.display()
        ))
    })
}

/// Crawls, then installs. Re-running replaces the book with a fresh crawl.
pub fn archive(spec: &ArchiveSpec, ctx: &Ctx, state: &mut StateGuard) -> Result<Vec<String>, InstallError> {
    spec.validate()?;
    let podman = require("podman", &ctx.search_path)?;
    let podman = podman.to_string_lossy().into_owned();

    let work = ctx.paths.downloads_dir().join(&spec.id);
    let io = |c: &str, p: &Path| {
        let (c, p) = (c.to_string(), p.to_path_buf());
        move |e| InstallError::Io {
            context: format!("{c} {}", p.display()),
            source: e,
        }
    };
    // A leftover ZIM from an earlier crawl must not be mistaken for the new one.
    if work.exists() {
        fs::remove_dir_all(&work).map_err(io("cannot clear", &work))?;
    }
    fs::create_dir_all(&work).map_err(io("cannot create", &work))?;

    let args = spec.podman_args(&work);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let ok = ctx
        .runner
        .run_visible(&podman, &refs)
        .map_err(io("cannot run", Path::new(&podman)))?;
    if !ok {
        return Err(InstallError::Command {
            command: format!("podman run {} zimit --seeds {}", spec.image, spec.url),
            stderr: format!("the crawl failed; partial output is in {}", work.display()),
        });
    }

    let found = find_zim(&work)?;
    let file_name = format!("{}.zim", spec.id);
    let source = work.join(&file_name);
    if found != source {
        fs::rename(&found, &source).map_err(io("cannot rename", &found))?;
    }
    let size = fs::metadata(&source).map_err(io("cannot stat", &source))?.len();
    let sha256 = download::hash_file(&source).map_err(|e| InstallError::Corrupt(e.to_string()))?;
    let input = InstallInput {
        id: spec.id.clone(),
        name: spec.name.clone(),
        license: spec.license.clone(),
        version: None,
        source,
        file_name,
        size,
        sha256,
        verified: true,
        image: None,
        service: None,
        content: None,
        flatpak: None,
        group: None,
        collection: None,
        ollama: None,
        translation: None,
        model_spec: None,
        category: Some("reference".into()),
    };
    let notes = ZimInstaller.install(&input, ctx, state)?;
    // Only the finished ZIM is worth keeping; the library holds a hard link to it.
    if let Ok(rd) = fs::read_dir(&work) {
        for e in rd.flatten().filter(|e| e.file_name() != input.file_name.as_str()) {
            let _ = fs::remove_dir_all(e.path()).or_else(|_| fs::remove_file(e.path()));
        }
    }
    Ok(notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ArchiveSpec {
        ArchiveSpec {
            url: "https://projekt-gutenberg.org/bibliothek/".into(),
            id: "gutenberg_de".into(),
            name: "Projekt Gutenberg-DE".into(),
            description: "Bibliothek".into(),
            license: "personal copy".into(),
            image: DEFAULT_IMAGE.into(),
            workers: 2,
            page_limit: Some(500),
            exclude: vec!["\\?q=".into()],
            wait_until: "domcontentloaded".into(),
        }
    }

    #[test]
    fn validation_rejects_missing_license_bad_id_and_bad_url() {
        assert!(spec().validate().is_ok());
        assert!(
            ArchiveSpec {
                license: " ".into(),
                ..spec()
            }
            .validate()
            .is_err()
        );
        assert!(
            ArchiveSpec {
                id: "Bad Id".into(),
                ..spec()
            }
            .validate()
            .is_err()
        );
        assert!(
            ArchiveSpec {
                url: "ftp://x".into(),
                ..spec()
            }
            .validate()
            .is_err()
        );
        assert!(ArchiveSpec { workers: 0, ..spec() }.validate().is_err());
    }

    #[test]
    fn podman_args_stay_inside_the_url_prefix_and_mount_the_work_dir() {
        let a = spec().podman_args(Path::new("/d/downloads/gutenberg_de"));
        let j = a.join(" ");
        assert!(j.contains("-v /d/downloads/gutenberg_de:/output:Z"));
        assert!(j.contains("--seeds https://projekt-gutenberg.org/bibliothek/"));
        assert!(j.contains("--scopeType prefix"));
        assert!(j.contains("--pageLimit 500"));
        assert!(j.contains("--scopeExcludeRx \\?q="));
    }

    #[test]
    fn archive_crawls_installs_and_a_failed_crawl_changes_nothing() {
        use crate::catalog::http::testing::FakeHttp;
        use crate::install::Paths;
        use crate::install::runner::CmdOutput;
        use crate::install::runner::testing::{FakeRunner, system};
        use crate::install::tools::fake_exe;

        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for t in ["podman", "kiwix-manage", "kiwix-serve", "systemctl"] {
            fake_exe(&bin, t);
        }
        let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        let host = system(true);
        let crash = std::rc::Rc::new(std::cell::Cell::new(false));
        let c = crash.clone();
        let runner = FakeRunner::new(move |prog, args| {
            if prog == "podman" {
                if c.get() {
                    return Ok(CmdOutput::failed("boom"));
                }
                let mount = args[args.iter().position(|a| *a == "-v").unwrap() + 1];
                let work = mount.trim_end_matches(":/output:Z");
                fs::write(Path::new(work).join("gutenberg_de_2026-11.zim"), b"zimdata").unwrap();
                fs::write(Path::new(work).join("crawl.log"), b"log").unwrap();
                return Ok(CmdOutput::ok());
            }
            use crate::install::runner::CommandRunner;
            host.run(prog, args)
        });
        let probe = FakeHttp::default();
        let ctx = Ctx {
            paths: paths.clone(),
            runner: &runner,
            probe: &probe,
            search_path: vec![bin],
            port: 6589,
            user: Some("tester".into()),
            start_wait: std::time::Duration::ZERO,
            now: 1_790_000_000,
            check_port: false,
        };

        crash.set(true);
        let mut st = StateGuard::open(&paths).unwrap();
        assert!(archive(&spec(), &ctx, &mut st).is_err());
        assert!(st.packages.is_empty());

        crash.set(false);
        archive(&spec(), &ctx, &mut st).unwrap();
        let r = &st.packages["gutenberg_de"];
        assert_eq!(r.license, "personal copy");
        assert_eq!(r.files[0].name, "gutenberg_de.zim");
        assert!(paths.library_dir().join("gutenberg_de.zim").exists());
        assert!(!paths.downloads_dir().join("gutenberg_de/crawl.log").exists());
        assert!(runner.count("--seeds https://projekt-gutenberg.org/bibliothek/") >= 1);
    }

    #[test]
    fn find_zim_takes_the_zim_and_errors_without_one() {
        let d = tempfile::tempdir().unwrap();
        assert!(find_zim(d.path()).is_err());
        fs::write(d.path().join("x_2026-11.zim"), b"z").unwrap();
        fs::write(d.path().join("log.txt"), b"l").unwrap();
        assert_eq!(find_zim(d.path()).unwrap(), d.path().join("x_2026-11.zim"));
    }
}
