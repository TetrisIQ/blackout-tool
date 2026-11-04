//! Catalogs that live in their own repositories: the list of sources (`<config>/kit/catalogs.json`,
//! order = priority) and fetching one as a `.tar.gz` whose SHA-256 is checked before it is unpacked.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::Catalog;
use super::http::{Http, HttpError};

pub const SOURCES_FILE: &str = "catalogs.json";
/// Metadata file at the top of a catalog (not a package manifest).
pub const META_FILE: &str = "catalog.yaml";
/// Written next to an unpacked catalog: the SHA-256 of the archive it came from.
pub const STAMP_FILE: &str = ".kit-sha256";

/// Name and location of the catalog used when none was added.
pub const DEFAULT_NAME: &str = "main";
pub const DEFAULT_URL: &str = "https://github.com/TetrisIQ/katalog/releases/latest/download/catalog.tar.gz";

/// Upper bound for what an archive may unpack to.
const MAX_UNPACKED: u64 = 200_000_000;

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    Checksum {
        url: String,
        expected: String,
        actual: String,
    },
    #[error("the catalog in the archive is invalid: {0}")]
    Catalog(String),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
}

/// One catalog source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub url: String,
    /// Fixed checksum of the archive; without it `<url>.sha256` is read on every update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceList {
    #[serde(default)]
    catalogs: Vec<Source>,
}

/// The catalog's own description (`catalog.yaml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub format: u32,
}

/// The only catalog format this kit reads.
pub const FORMAT: u32 = 1;

/// Reads `<dir>/catalog.yaml` if there is one.
pub fn read_meta(dir: &Path) -> Result<Option<Meta>, RemoteError> {
    let path = dir.join(META_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(RemoteError::Io { path, source }),
    };
    let meta: Meta = serde_yaml_ng::from_str(&text)
        .map_err(|e| RemoteError::Catalog(format!("{}: {e}", path.display())))?;
    if meta.format != FORMAT {
        return Err(RemoteError::Catalog(format!(
            "{}: format {} is not supported (this kit reads format {FORMAT})",
            path.display(),
            meta.format
        )));
    }
    Ok(Some(meta))
}

/// The saved sources in priority order. A missing file is an empty list; a damaged one is an error.
pub fn load(config_dir: &Path) -> io::Result<Vec<Source>> {
    let path = config_dir.join(SOURCES_FILE);
    match fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str::<SourceList>(&t)
            .map(|l| l.catalogs)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Atomic, like `datadir::save`.
pub fn save(config_dir: &Path, sources: &[Source]) -> io::Result<()> {
    fs::create_dir_all(config_dir)?;
    let path = config_dir.join(SOURCES_FILE);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(&SourceList {
        catalogs: sources.to_vec(),
    })
    .map_err(io::Error::other)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)
}

/// Catalog names are directory names: `[a-z0-9][a-z0-9_-]*`.
pub fn validate_name(name: &str) -> Result<(), RemoteError> {
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(RemoteError::Invalid(format!(
            "catalog name {name:?} must match [a-z0-9][a-z0-9_-]*"
        )))
    }
}

/// `https://` only; plain `http://` is accepted for the local machine (tests, a local mirror).
pub fn validate_url(url: &str) -> Result<(), RemoteError> {
    let local = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.strip_prefix(p).is_some_and(|r| r.starts_with([':', '/'])));
    if url.starts_with("https://") || local {
        Ok(())
    } else {
        Err(RemoteError::Invalid(format!(
            "catalog url {url:?} must be https://"
        )))
    }
}

pub fn validate_sha256(hex: &str) -> Result<(), RemoteError> {
    if hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(RemoteError::Invalid(format!(
            "sha256 {hex:?} must be 64 lowercase hex characters"
        )))
    }
}

impl Source {
    pub fn new(name: &str, url: &str, sha256: Option<&str>) -> Result<Self, RemoteError> {
        validate_name(name)?;
        validate_url(url)?;
        if let Some(h) = sha256 {
            validate_sha256(h)?;
        }
        Ok(Self {
            name: name.into(),
            url: url.into(),
            sha256: sha256.map(str::to_string),
            enabled: true,
        })
    }
}

/// Where a fetched catalog is unpacked: `<data>/catalogs/<name>`.
pub fn catalogs_root(data_dir: &Path) -> PathBuf {
    data_dir.join("catalogs")
}

/// The directory of a fetched catalog if it is there.
pub fn fetched_dir(data_dir: &Path, name: &str) -> Option<PathBuf> {
    let dir = catalogs_root(data_dir).join(name);
    dir.is_dir().then_some(dir)
}

/// SHA-256 of the archive a fetched catalog was unpacked from.
pub fn stamp(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(STAMP_FILE))
        .ok()
        .map(|s| s.trim().to_string())
}

/// What `fetch` did.
#[derive(Debug, PartialEq, Eq)]
pub struct Fetched {
    pub dir: PathBuf,
    pub sha256: String,
    /// The same archive was already unpacked.
    pub unchanged: bool,
}

/// Downloads `source`, checks the archive against its SHA-256 and replaces
/// `<data>/catalogs/<name>`. The new catalog has to load; if anything fails the old one stays.
pub fn fetch(http: &dyn Http, source: &Source, data_dir: &Path) -> Result<Fetched, RemoteError> {
    validate_name(&source.name)?;
    validate_url(&source.url)?;
    let expected = match &source.sha256 {
        Some(h) => h.clone(),
        None => {
            let body = http.get(&format!("{}.sha256", source.url))?;
            let h = body.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            validate_sha256(&h).map_err(|_| {
                RemoteError::Invalid(format!("{}.sha256 does not hold a SHA-256", source.url))
            })?;
            h
        }
    };
    let root = catalogs_root(data_dir);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| RemoteError::Io { path, source }
    };
    fs::create_dir_all(&root).map_err(io(&root))?;
    let target = root.join(&source.name);
    if stamp(&target).as_deref() == Some(expected.as_str()) {
        return Ok(Fetched {
            dir: target,
            sha256: expected,
            unchanged: true,
        });
    }

    let work = root.join(format!(".fetch-{}-{}", source.name, std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(io(&work))?;
    let result = unpack_checked(http, source, &expected, &work);
    let unpacked = match result {
        Ok(d) => d,
        Err(e) => {
            let _ = fs::remove_dir_all(&work);
            return Err(e);
        }
    };
    fs::write(unpacked.join(STAMP_FILE), format!("{expected}\n")).map_err(io(&unpacked))?;

    let old = root.join(format!(".old-{}-{}", source.name, std::process::id()));
    let _ = fs::remove_dir_all(&old);
    let had_old = target.exists();
    if had_old {
        fs::rename(&target, &old).map_err(io(&target))?;
    }
    if let Err(source) = fs::rename(&unpacked, &target) {
        if had_old {
            let _ = fs::rename(&old, &target);
        }
        let _ = fs::remove_dir_all(&work);
        return Err(RemoteError::Io { path: target, source });
    }
    let _ = fs::remove_dir_all(&old);
    let _ = fs::remove_dir_all(&work);
    Ok(Fetched {
        dir: target,
        sha256: expected,
        unchanged: false,
    })
}

/// Download, verify, unpack and test-load inside `work`; returns the catalog's top directory.
fn unpack_checked(
    http: &dyn Http,
    source: &Source,
    expected: &str,
    work: &Path,
) -> Result<PathBuf, RemoteError> {
    let archive = work.join("catalog.tar.gz");
    let (_, actual) = http.get_file(&source.url, &archive)?;
    if actual != expected {
        return Err(RemoteError::Checksum {
            url: source.url.clone(),
            expected: expected.into(),
            actual,
        });
    }
    let out = work.join("unpacked");
    unpack(&archive, &out)?;
    let top = catalog_top(&out)?;
    read_meta(&top)?;
    Catalog::load(&top).map_err(|e| RemoteError::Catalog(e.to_string()))?;
    Ok(top)
}

/// A GitHub archive has one directory around everything; the catalog is inside it.
fn catalog_top(out: &Path) -> Result<PathBuf, RemoteError> {
    let has_catalog = |d: &Path| d.join("profiles").is_dir() || d.join(META_FILE).is_file();
    if has_catalog(out) {
        return Ok(out.to_path_buf());
    }
    let io = |source| RemoteError::Io {
        path: out.to_path_buf(),
        source,
    };
    let mut dirs = Vec::new();
    for e in fs::read_dir(out).map_err(io)? {
        let p = e.map_err(io)?.path();
        if p.is_dir() {
            dirs.push(p);
        }
    }
    match dirs.as_slice() {
        [one] if has_catalog(one) => Ok(one.clone()),
        _ => Err(RemoteError::Catalog(
            "the archive holds neither a profiles/ directory nor catalog.yaml".into(),
        )),
    }
}

/// Unpacks regular files and directories only; any path that leaves `dest` is an error.
fn unpack(archive: &Path, dest: &Path) -> Result<(), RemoteError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| RemoteError::Io { path, source }
    };
    fs::create_dir_all(dest).map_err(io(dest))?;
    let file = fs::File::open(archive).map_err(io(archive))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut total = 0u64;
    for entry in tar.entries().map_err(io(archive))? {
        let mut entry = entry.map_err(io(archive))?;
        let path = entry.path().map_err(io(archive))?.into_owned();
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            if kind.is_dir() {
                safe_relative(&path)?;
                fs::create_dir_all(dest.join(&path)).map_err(io(dest))?;
            }
            continue;
        }
        if !kind.is_file() {
            return Err(RemoteError::Catalog(format!(
                "{} is a link or special file; catalogs hold plain files only",
                path.display()
            )));
        }
        safe_relative(&path)?;
        total += entry.header().size().map_err(io(archive))?;
        if total > MAX_UNPACKED {
            return Err(RemoteError::Catalog(format!(
                "the archive unpacks to more than {MAX_UNPACKED} bytes"
            )));
        }
        if !entry.unpack_in(dest).map_err(io(dest))? {
            return Err(RemoteError::Catalog(format!(
                "{} leaves the catalog directory",
                path.display()
            )));
        }
    }
    Ok(())
}

fn safe_relative(path: &Path) -> Result<(), RemoteError> {
    if path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        Ok(())
    } else {
        Err(RemoteError::Catalog(format!(
            "{} leaves the catalog directory",
            path.display()
        )))
    }
}

/// The SHA-256 of `bytes` as lowercase hex (used for pins in tests and by `kit catalog`).
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const ZIM: &str = "id: wiki\nname: Wiki\ndescription: d\ncategory: wikipedia\ntype: zim\n\
        source:\n  kiwix: {name: wikipedia_de_all, flavour: nopic, lang: deu}\n\
        license: {spdx: CC-BY-SA-4.0}\nlanguages: [de]\n";

    /// Serves canned archives and checksum files; counts archive downloads.
    #[derive(Default)]
    struct Fake {
        files: HashMap<String, Vec<u8>>,
        texts: HashMap<String, String>,
        downloads: std::sync::atomic::AtomicUsize,
    }

    impl Http for Fake {
        fn get(&self, url: &str) -> Result<String, HttpError> {
            self.texts.get(url).cloned().ok_or_else(|| HttpError::Request {
                url: url.into(),
                message: "404".into(),
            })
        }
        fn get_file(&self, url: &str, dest: &Path) -> Result<(u64, String), HttpError> {
            self.downloads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let bytes = self.files.get(url).ok_or_else(|| HttpError::Request {
                url: url.into(),
                message: "404".into(),
            })?;
            fs::write(dest, bytes).unwrap();
            Ok((bytes.len() as u64, sha256_hex(bytes)))
        }
    }

    /// `(path, content)` pairs as a `.tar.gz`; a path may be written raw to test hostile names.
    fn targz(files: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, content) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            let raw = &mut h.as_old_mut().name;
            raw[..path.len()].copy_from_slice(path.as_bytes());
            h.set_cksum();
            b.append(&h, content.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    const URL: &str = "https://example.org/catalog.tar.gz";

    fn served(archive: Vec<u8>) -> Fake {
        let mut f = Fake::default();
        f.texts.insert(
            format!("{URL}.sha256"),
            format!("{}  catalog.tar.gz\n", sha256_hex(&archive)),
        );
        f.files.insert(URL.into(), archive);
        f
    }

    fn source() -> Source {
        Source::new("extra", URL, None).unwrap()
    }

    #[test]
    fn sources_round_trip_and_a_missing_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_empty());
        let list = vec![source(), Source::new("two", URL, Some(&"a".repeat(64))).unwrap()];
        save(d.path(), &list).unwrap();
        assert_eq!(load(d.path()).unwrap(), list);
        fs::write(d.path().join(SOURCES_FILE), "{\"catalogs\": [{\"name\": \"x\"}]}").unwrap();
        assert!(load(d.path()).is_err());
    }

    #[test]
    fn names_urls_and_pins_are_validated() {
        assert!(Source::new("Bad Name", URL, None).is_err());
        assert!(Source::new("ok_1", "http://example.org/c.tar.gz", None).is_err());
        assert!(Source::new("ok", "http://127.0.0.1:8000/c.tar.gz", None).is_ok());
        assert!(Source::new("ok", "http://127.0.0.1.evil.org/c.tar.gz", None).is_err());
        assert!(Source::new("ok", URL, Some("ABC")).is_err());
    }

    #[test]
    fn fetch_unpacks_a_verified_archive_and_a_second_fetch_is_a_no_op() {
        let d = tempfile::tempdir().unwrap();
        let http = served(targz(&[
            ("catalog.yaml", "name: extra\nformat: 1\n"),
            ("wikipedia/wiki.yaml", ZIM),
        ]));
        let first = fetch(&http, &source(), d.path()).unwrap();
        assert!(!first.unchanged);
        assert!(first.dir.join("wikipedia/wiki.yaml").is_file());
        assert_eq!(stamp(&first.dir), Some(first.sha256.clone()));
        assert!(Catalog::load(&first.dir).unwrap().package("wiki").is_ok());
        let second = fetch(&http, &source(), d.path()).unwrap();
        assert!(second.unchanged);
        assert_eq!(http.downloads.load(std::sync::atomic::Ordering::SeqCst), 1);
        let leftovers: Vec<_> = fs::read_dir(catalogs_root(d.path())).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no work directories remain");
    }

    #[test]
    fn a_github_style_wrapper_directory_is_looked_through() {
        let d = tempfile::tempdir().unwrap();
        let http = served(targz(&[("katalog-main/wikipedia/wiki.yaml", ZIM)]));
        let r = fetch(&http, &source(), d.path());
        assert!(
            r.is_err(),
            "no profiles/ or catalog.yaml means it is not a catalog"
        );
        let http = served(targz(&[
            ("katalog-main/catalog.yaml", "name: x\nformat: 1\n"),
            ("katalog-main/wikipedia/wiki.yaml", ZIM),
        ]));
        let ok = fetch(&http, &source(), d.path()).unwrap();
        assert!(ok.dir.join("wikipedia/wiki.yaml").is_file());
    }

    #[test]
    fn a_wrong_checksum_is_refused_and_the_old_catalog_stays() {
        let d = tempfile::tempdir().unwrap();
        let good = served(targz(&[
            ("catalog.yaml", "name: x\nformat: 1\n"),
            ("wikipedia/wiki.yaml", ZIM),
        ]));
        let old = fetch(&good, &source(), d.path()).unwrap();

        let mut bad = served(targz(&[("catalog.yaml", "name: x\nformat: 1\n")]));
        bad.texts.insert(format!("{URL}.sha256"), "f".repeat(64));
        let err = fetch(&bad, &source(), d.path()).unwrap_err();
        assert!(matches!(err, RemoteError::Checksum { .. }), "{err}");
        assert_eq!(stamp(&old.dir), Some(old.sha256));
        assert!(old.dir.join("wikipedia/wiki.yaml").is_file());
    }

    #[test]
    fn a_pinned_hash_is_used_instead_of_the_published_one() {
        let d = tempfile::tempdir().unwrap();
        let archive = targz(&[
            ("catalog.yaml", "name: x\nformat: 1\n"),
            ("wikipedia/wiki.yaml", ZIM),
        ]);
        let mut http = Fake::default();
        http.files.insert(URL.into(), archive.clone());
        // no .sha256 is served at all
        let pinned = Source::new("extra", URL, Some(&sha256_hex(&archive))).unwrap();
        assert!(fetch(&http, &pinned, d.path()).is_ok());
        let wrong = Source::new("other", URL, Some(&"0".repeat(64))).unwrap();
        assert!(matches!(
            fetch(&http, &wrong, d.path()),
            Err(RemoteError::Checksum { .. })
        ));
    }

    #[test]
    fn hostile_paths_are_refused() {
        for path in ["../evil.yaml", "/etc/evil.yaml", "a/../../evil.yaml"] {
            let d = tempfile::tempdir().unwrap();
            let http = served(targz(&[("catalog.yaml", "name: x\nformat: 1\n"), (path, "x")]));
            assert!(fetch(&http, &source(), d.path()).is_err(), "{path}");
            assert!(!d.path().join("evil.yaml").exists());
            assert!(fetched_dir(d.path(), "extra").is_none());
        }
    }

    #[test]
    fn an_invalid_catalog_is_not_installed() {
        let d = tempfile::tempdir().unwrap();
        let http = served(targz(&[
            ("catalog.yaml", "name: x\nformat: 1\n"),
            (
                "wikipedia/wiki.yaml",
                &ZIM.replace("license: {spdx: CC-BY-SA-4.0}\n", ""),
            ),
        ]));
        assert!(matches!(
            fetch(&http, &source(), d.path()),
            Err(RemoteError::Catalog(_))
        ));
        assert!(fetched_dir(d.path(), "extra").is_none());
    }

    #[test]
    fn an_unknown_format_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let http = served(targz(&[
            ("catalog.yaml", "name: x\nformat: 2\n"),
            ("wikipedia/wiki.yaml", ZIM),
        ]));
        assert!(fetch(&http, &source(), d.path()).is_err());
    }
}
