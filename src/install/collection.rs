//! Installer for `collection` packages: a whole public catalog fetched book by book. Standard
//! Ebooks publishes no checksums (its bulk zips are for Patrons), so there is nothing to compare
//! against beforehand. kit lists the books from the public sitemap, downloads each EPUB over
//! HTTPS at a gentle pace, accepts only files that are really EPUBs, and records the SHA-256 of
//! every file at its first download (trust on first use) for `kit verify --deep`.
//!
//! A run can be interrupted at any time: finished books stay in the pack directory (they are
//! renamed into place only after the check) and the next run fetches only the missing ones. The
//! package counts as installed once every book is there.

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use super::state::{FileRecord, Record, StateGuard};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::Provider;
use crate::download;

pub const SITE: &str = "https://standardebooks.org";
/// Pause between two books; the site bans clients that hammer it.
pub const POLITE_DELAY: Duration = Duration::from_millis(500);
/// Give up when this many books in a row fail (blocked, offline): more requests would not help.
const MAX_STREAK: u32 = 3;
const TRIES: u32 = 3;

pub struct CollectionInstaller;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Book {
    /// `author/title[/contributor]`
    pub slug: String,
    /// File name in the pack directory.
    pub file: String,
    pub url: String,
}

fn is_slug_part(p: &str) -> bool {
    !p.is_empty()
        && p.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The books of the sitemap. A book page `…/ebooks/<author>/<title>[/<contributor>]` is the one
/// that has a `…/text` page; the author, collection and reading pages around it are not books.
pub fn books_from_sitemap(xml: &str) -> Result<Vec<Book>, InstallError> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| InstallError::Corrupt(format!("{SITE}/sitemap: not valid XML: {e}")))?;
    let prefix = format!("{SITE}/ebooks/");
    let mut books = BTreeSet::new();
    for loc in doc.descendants().filter(|n| n.has_tag_name("loc")) {
        let Some(path) = loc.text().and_then(|t| t.trim().strip_prefix(prefix.as_str())) else {
            continue;
        };
        let Some(slug) = path.strip_suffix("/text") else {
            continue;
        };
        let parts: Vec<&str> = slug.split('/').collect();
        if parts.len() < 2 || !parts.iter().all(|p| is_slug_part(p)) {
            continue;
        }
        let file = format!("{}.epub", parts.join("_"));
        books.insert(Book {
            slug: slug.to_string(),
            url: format!("{SITE}/ebooks/{slug}/downloads/{file}?source=download"),
            file,
        });
    }
    if books.is_empty() {
        return Err(InstallError::Corrupt(format!(
            "{SITE}/sitemap lists no books; the site layout may have changed"
        )));
    }
    Ok(books.into_iter().collect())
}

/// An EPUB is a zip whose first entry is `mimetype` with exactly this content.
pub fn is_epub(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return false;
    };
    let Ok(entry) = zip.by_name("mimetype") else {
        return false;
    };
    let mut s = String::new();
    entry.take(64).read_to_string(&mut s).is_ok() && s.trim() == "application/epub+zip"
}

fn provider_of(input: &InstallInput) -> Result<Provider, InstallError> {
    input
        .collection
        .map(|c| c.provider)
        .ok_or_else(|| InstallError::Corrupt(format!("{}: the manifest has no source.collection", input.id)))
}

fn media_of(input: &InstallInput) -> Result<&str, InstallError> {
    input
        .content
        .as_ref()
        .map(|c| c.media.as_str())
        .ok_or_else(|| InstallError::Corrupt(format!("{}: the manifest has no content block", input.id)))
}

fn epubs_in(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".epub"))
                .collect()
        })
        .unwrap_or_default()
}

impl CollectionInstaller {
    /// `install` with the pause between books and the progress report injectable.
    pub fn install_with(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
        delay: Duration,
        progress: &dyn Fn(usize, usize),
    ) -> Result<Vec<String>, InstallError> {
        let Provider::StandardEbooks = provider_of(input)?;
        let media = media_of(input)?;
        let dir = ctx.paths.media_dir(media).join(&input.id);
        fs::create_dir_all(&dir).map_err(io_err("cannot create", &dir))?;

        let sitemap = ctx.probe.get(&format!("{SITE}/sitemap")).map_err(|e| {
            InstallError::Unsupported(format!("cannot list the books (needs the internet): {e}"))
        })?;
        let books = books_from_sitemap(&sitemap)?;

        // Leftovers of an interrupted run: half-written files and anything that is not an EPUB.
        for name in fs::read_dir(&dir)
            .map_err(io_err("cannot read", &dir))?
            .filter_map(|e| e.ok())
        {
            let path = name.path();
            let is_part = path.extension().is_some_and(|e| e == "part");
            if is_part || (path.extension().is_some_and(|e| e == "epub") && !is_epub(&path)) {
                let _ = fs::remove_file(&path);
            }
        }
        let have: BTreeSet<String> = epubs_in(&dir).into_iter().collect();
        let missing: Vec<&Book> = books.iter().filter(|b| !have.contains(&b.file)).collect();
        let total = missing.len();

        let (mut failed, mut streak) = (Vec::new(), 0u32);
        for (i, book) in missing.iter().enumerate() {
            progress(i, total);
            if i > 0 {
                std::thread::sleep(delay);
            }
            let part = dir.join(format!("{}.part", book.file));
            let mut last = None;
            for attempt in 1..=TRIES {
                let r = ctx
                    .probe
                    .get_file(&book.url, &part)
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        if is_epub(&part) {
                            fs::rename(&part, dir.join(&book.file)).map_err(|e| e.to_string())
                        } else {
                            Err(format!("{} is not an EPUB", book.url))
                        }
                    });
                match r {
                    Ok(()) => {
                        last = None;
                        break;
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&part);
                        last = Some(e);
                        std::thread::sleep(delay * 4 * attempt);
                    }
                }
            }
            match last {
                None => streak = 0,
                Some(e) => {
                    failed.push(format!("{}: {e}", book.slug));
                    streak += 1;
                    if streak >= MAX_STREAK {
                        return Err(InstallError::Corrupt(format!(
                            "{}: stopped after {} books in a row failed (the site may be blocking \
                             or you are offline); the {} books fetched so far are kept, run the \
                             same command again later. Last error: {}",
                            input.id,
                            MAX_STREAK,
                            epubs_in(&dir).len(),
                            failed.last().map(String::as_str).unwrap_or_default()
                        )));
                    }
                }
            }
        }
        if !failed.is_empty() {
            return Err(InstallError::Corrupt(format!(
                "{}: {} of {} books could not be fetched and are skipped for now; run the same \
                 command again to retry them. First error: {}",
                input.id,
                failed.len(),
                total,
                failed[0]
            )));
        }

        // Every book is there: hash them all (fetched now or in an earlier run) and record.
        let mut files = Vec::new();
        for book in &books {
            let path = dir.join(&book.file);
            let sha256 = download::hash_file(&path).map_err(|e| InstallError::Corrupt(e.to_string()))?;
            let size = fs::metadata(&path).map_err(io_err("cannot read", &path))?.len();
            files.push(FileRecord {
                name: format!("{media}/{}/{}", input.id, book.file),
                size,
                sha256,
                source: None,
                moved: false,
            });
        }
        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        let count = files.len();
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Collection,
                version: Some(format!("{count} books")),
                license: input.license.clone(),
                installed_at,
                category: None,
                files,
                service: None,
                app: None,
                map: None,
                model: None,
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(vec![format!(
            "{}: {count} books ({total} new) are in {}; services mounting the {media} directory pick \
             them up on their next scan. Checksums were recorded at the first download (the site \
             publishes none)",
            input.id,
            dir.display()
        )])
    }
}

impl Installer for CollectionInstaller {
    fn plan(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &super::state::State,
    ) -> Result<Vec<String>, InstallError> {
        provider_of(input)?;
        let dir = ctx.paths.media_dir(media_of(input)?).join(&input.id);
        if state.packages.contains_key(&input.id) && dir.exists() {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!(
                "list the books in {SITE}/sitemap and download each EPUB into {} (one request every \
                 {} ms; an interrupted run continues where it stopped)",
                dir.display(),
                POLITE_DELAY.as_millis()
            ),
            "accept only valid EPUB files and record the SHA-256 of each (the site publishes none)".into(),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        self.install_with(input, ctx, state, POLITE_DELAY, &|i, total| {
            if i % 25 == 0 {
                eprintln!("{}: book {} of {total} still to fetch", input.id, i + 1);
            }
        })
    }

    fn verify(&self, record: &Record, ctx: &Ctx, deep: bool) -> Vec<Check> {
        let media = ctx.paths.data_dir.join("media");
        let mut bad = Vec::new();
        for f in &record.files {
            let path = media.join(&f.name);
            match fs::metadata(&path) {
                Err(_) => bad.push(format!("{} is missing", f.name)),
                Ok(m) if m.len() != f.size => {
                    bad.push(format!("{} has {} bytes, expected {}", f.name, m.len(), f.size))
                }
                Ok(_) if deep => match download::hash_file(&path) {
                    Ok(h) if h == f.sha256 => {}
                    Ok(_) => bad.push(format!("{} was changed since the download", f.name)),
                    Err(e) => bad.push(e.to_string()),
                },
                Ok(_) => {}
            }
        }
        let detail = if bad.is_empty() {
            format!(
                "{} books{}",
                record.files.len(),
                if deep { ", checksums match" } else { "" }
            )
        } else {
            format!(
                "{} problems, first: {}; fix with: kit uninstall {id} && kit install --select {id}",
                bad.len(),
                bad[0],
                id = record.id
            )
        };
        vec![Check {
            name: format!("{}: books", record.id),
            ok: bad.is_empty(),
            detail,
        }]
    }

    fn uninstall(
        &self,
        id: &str,
        ctx: &Ctx,
        state: &mut StateGuard,
        _purge: bool,
    ) -> Result<Vec<String>, InstallError> {
        let Some(record) = state.packages.remove(id) else {
            return Ok(vec![format!("{id} is not installed")]);
        };
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        let media = ctx.paths.data_dir.join("media");
        let dirs: BTreeSet<_> = record
            .files
            .iter()
            .filter_map(|f| Path::new(&f.name).parent().map(|p| media.join(p)))
            .collect();
        for d in dirs {
            match fs::remove_dir_all(&d) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("cannot remove", &d)(e)),
            }
        }
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::http::{Http, HttpError};
    use crate::catalog::manifest::{CollectionRef, ContentSpec, Extract};
    use crate::install::runner::testing::FakeRunner;
    use crate::install::state::StateGuard;
    use crate::install::{Ctx, Paths};
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Mutex;

    fn epub_bytes(title: &str) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        z.start_file("mimetype", opts).unwrap();
        z.write_all(b"application/epub+zip").unwrap();
        z.start_file("book.txt", opts).unwrap();
        z.write_all(title.as_bytes()).unwrap();
        z.finish().unwrap();
        buf.into_inner()
    }

    const A: &str = "https://standardebooks.org/ebooks/a-author/first-book/downloads/a-author_first-book.epub?source=download";
    const B: &str = "https://standardebooks.org/ebooks/b-author/second/trans-lator/downloads/b-author_second_trans-lator.epub?source=download";

    fn sitemap() -> String {
        let urls = [
            "https://standardebooks.org/",
            "https://standardebooks.org/ebooks/a-author",
            "https://standardebooks.org/ebooks/a-author/first-book",
            "https://standardebooks.org/ebooks/a-author/first-book/text",
            "https://standardebooks.org/ebooks/a-author/first-book/text/single-page",
            "https://standardebooks.org/ebooks/b-author/second/trans-lator",
            "https://standardebooks.org/ebooks/b-author/second/trans-lator/text",
            "https://standardebooks.org/collections/some-series",
        ];
        let locs: String = urls
            .iter()
            .map(|u| format!("<url><loc>{u}</loc></url>"))
            .collect();
        format!(
            r#"<?xml version="1.0"?><urlset xmlns="https://www.sitemaps.org/schemas/sitemap/0.9">{locs}</urlset>"#
        )
    }

    /// Serves the sitemap and per-URL file bodies; `fail` lists URLs that always fail and
    /// `flaky` URLs that fail once.
    #[derive(Default)]
    struct Site {
        sitemap: String,
        files: HashMap<String, Vec<u8>>,
        fail: Vec<String>,
        flaky: Mutex<Vec<String>>,
        fetched: Mutex<Vec<String>>,
    }

    impl Http for Site {
        fn get(&self, _url: &str) -> Result<String, HttpError> {
            Ok(self.sitemap.clone())
        }
        fn get_file(&self, url: &str, dest: &Path) -> Result<(u64, String), HttpError> {
            let err = |m: &str| HttpError::Request {
                url: url.into(),
                message: m.into(),
            };
            if self.fail.iter().any(|u| u == url) {
                return Err(err("http status: 429"));
            }
            let mut flaky = self.flaky.lock().unwrap();
            if let Some(i) = flaky.iter().position(|u| u == url) {
                flaky.remove(i);
                return Err(err("connection lost"));
            }
            self.fetched.lock().unwrap().push(url.into());
            let body = self.files.get(url).ok_or_else(|| err("http status: 404"))?;
            fs::write(dest, body).unwrap();
            Ok((body.len() as u64, String::new()))
        }
    }

    struct Env {
        _d: tempfile::TempDir,
        paths: Paths,
        runner: FakeRunner,
        site: Site,
    }

    fn env() -> Env {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path().join("data"), d.path().join("units")).unwrap();
        let site = Site {
            sitemap: sitemap(),
            files: HashMap::from([(A.into(), epub_bytes("one")), (B.into(), epub_bytes("two"))]),
            ..Site::default()
        };
        Env {
            _d: d,
            paths,
            runner: FakeRunner::new(|_, _| Ok(crate::install::runner::CmdOutput::ok())),
            site,
        }
    }

    impl Env {
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                paths: self.paths.clone(),
                runner: &self.runner,
                probe: &self.site,
                search_path: vec![],
                port: 6589,
                user: None,
                start_wait: Duration::ZERO,
                now: 1_790_000_000,
                check_port: false,
            }
        }
        fn dir(&self) -> std::path::PathBuf {
            self.paths.media_dir("books").join("se_all")
        }
        fn run(&self, g: &mut StateGuard) -> Result<Vec<String>, InstallError> {
            CollectionInstaller.install_with(&input(), &self.ctx(), g, Duration::ZERO, &|_, _| {})
        }
    }

    fn input() -> InstallInput {
        InstallInput {
            id: "se_all".into(),
            name: "Standard Ebooks".into(),
            license: "CC0-1.0".into(),
            version: None,
            source: Default::default(),
            file_name: String::new(),
            size: 0,
            sha256: String::new(),
            verified: false,
            image: None,
            service: None,
            content: Some(ContentSpec {
                media: "books".into(),
                extract: Extract::None,
            }),
            flatpak: None,
            group: None,
            collection: Some(CollectionRef {
                provider: Provider::StandardEbooks,
            }),
            map: None,
            ollama: None,
            translation: None,
            model_spec: None,
            category: None,
        }
    }

    #[test]
    fn only_pages_with_a_text_page_are_books() {
        let books = books_from_sitemap(&sitemap()).unwrap();
        assert_eq!(
            books.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(),
            ["a-author/first-book", "b-author/second/trans-lator",]
        );
        assert_eq!(books[0].url, A);
        assert_eq!(books[1].url, B);
        assert_eq!(books[1].file, "b-author_second_trans-lator.epub");
        // Books by several authors have `_` in the slug and keep it in the file name.
        let multi = r#"<urlset><url><loc>https://standardebooks.org/ebooks/karl-marx_friedrich-engels/the-manifesto/text</loc></url></urlset>"#;
        let multi = books_from_sitemap(multi).unwrap();
        assert_eq!(multi[0].file, "karl-marx_friedrich-engels_the-manifesto.epub");
        assert!(books_from_sitemap("<urlset/>").is_err());
        assert!(books_from_sitemap("not xml").is_err());
    }

    #[test]
    fn fetches_every_book_records_checksums_and_is_idempotent() {
        let e = env();
        let mut g = StateGuard::open(&e.paths).unwrap();
        let notes = e.run(&mut g).unwrap();
        assert!(notes[0].contains("2 books (2 new)"), "{notes:?}");
        assert_eq!(epubs_in(&e.dir()).len(), 2);
        let rec = &g.packages["se_all"];
        assert_eq!(rec.kind, PackageType::Collection);
        assert_eq!(rec.files.len(), 2);
        assert!(rec.files.iter().all(|f| f.sha256.len() == 64 && f.size > 0));
        assert!(rec.files[0].name.starts_with("books/se_all/"));
        // A second run downloads nothing.
        let before = e.site.fetched.lock().unwrap().len();
        e.run(&mut g).unwrap();
        assert_eq!(e.site.fetched.lock().unwrap().len(), before);
        assert_eq!(g.packages["se_all"].files.len(), 2);
    }

    #[test]
    fn an_interrupted_run_resumes_and_junk_is_replaced() {
        let mut e = env();
        e.site.fail = vec![B.into()];
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = e.run(&mut g).unwrap_err();
        assert!(err.to_string().contains("1 of 2 books"), "{err}");
        assert!(
            g.packages.is_empty(),
            "nothing is recorded until every book is there"
        );
        assert_eq!(epubs_in(&e.dir()), ["a-author_first-book.epub"]);
        assert!(!e.dir().join("b-author_second_trans-lator.epub.part").exists());
        drop(g);
        // The site recovers; a corrupt file and a stale .part from a crash are cleaned up.
        e.site.fail.clear();
        fs::write(e.dir().join("junk.epub"), b"not a zip").unwrap();
        fs::write(e.dir().join("x.epub.part"), b"half").unwrap();
        let mut g = StateGuard::open(&e.paths).unwrap();
        e.run(&mut g).unwrap();
        assert_eq!(
            e.site.fetched.lock().unwrap().iter().filter(|u| *u == A).count(),
            1,
            "A was kept"
        );
        assert!(!e.dir().join("junk.epub").exists() && !e.dir().join("x.epub.part").exists());
        assert_eq!(g.packages["se_all"].files.len(), 2);
    }

    #[test]
    fn a_flaky_book_is_retried_and_a_non_epub_is_refused() {
        let mut e = env();
        e.site.flaky = Mutex::new(vec![A.into()]);
        let mut g = StateGuard::open(&e.paths).unwrap();
        e.run(&mut g).unwrap();
        assert_eq!(g.packages["se_all"].files.len(), 2);

        let mut e = env();
        e.site.files.insert(B.into(), b"<html>login</html>".to_vec());
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = e.run(&mut g).unwrap_err();
        assert!(err.to_string().contains("could not be fetched"), "{err}");
        assert_eq!(epubs_in(&e.dir()), ["a-author_first-book.epub"]);
    }

    #[test]
    fn stops_when_the_site_keeps_refusing() {
        let mut e = env();
        let many: Vec<String> = (0..6)
            .map(|i| format!("<url><loc>https://standardebooks.org/ebooks/x/b{i}/text</loc></url>"))
            .collect();
        e.site.sitemap = format!(
            r#"<urlset xmlns="https://www.sitemaps.org/schemas/sitemap/0.9">{}</urlset>"#,
            many.concat()
        );
        e.site.fail = (0..6)
            .map(|i| {
                format!("https://standardebooks.org/ebooks/x/b{i}/downloads/x_b{i}.epub?source=download")
            })
            .collect();
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = e.run(&mut g).unwrap_err();
        assert!(err.to_string().contains("3 books in a row"), "{err}");
    }

    #[test]
    fn verify_finds_changes_and_uninstall_removes_everything() {
        let e = env();
        let mut g = StateGuard::open(&e.paths).unwrap();
        e.run(&mut g).unwrap();
        let rec = g.packages["se_all"].clone();
        assert!(
            CollectionInstaller
                .verify(&rec, &e.ctx(), true)
                .iter()
                .all(|c| c.ok)
        );
        let victim = e.dir().join("a-author_first-book.epub");
        let mut bytes = fs::read(&victim).unwrap();
        let last = bytes.len() - 5;
        bytes[last] ^= 0xff;
        fs::write(&victim, bytes).unwrap();
        assert!(
            CollectionInstaller
                .verify(&rec, &e.ctx(), false)
                .iter()
                .all(|c| c.ok),
            "same size"
        );
        assert!(
            !CollectionInstaller
                .verify(&rec, &e.ctx(), true)
                .iter()
                .all(|c| c.ok)
        );
        fs::remove_file(&victim).unwrap();
        assert!(
            !CollectionInstaller
                .verify(&rec, &e.ctx(), false)
                .iter()
                .all(|c| c.ok)
        );

        CollectionInstaller
            .uninstall("se_all", &e.ctx(), &mut g, false)
            .unwrap();
        assert!(g.packages.is_empty() && !e.dir().exists());
        assert!(
            CollectionInstaller
                .uninstall("se_all", &e.ctx(), &mut g, false)
                .is_ok()
        );
    }

    #[test]
    fn plan_describes_the_run_without_touching_anything() {
        let e = env();
        let g = StateGuard::open(&e.paths).unwrap();
        let steps = CollectionInstaller.plan(&input(), &e.ctx(), &g).unwrap();
        assert!(steps[0].contains("sitemap") && !e.dir().exists());
    }
}
