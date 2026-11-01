//! Installer for `files` packages that feed a service: the content lands in
//! `<data-dir>/media/<kind>/<id>/`, which the service mounts read-only. A zip is unpacked into a
//! temporary directory first and renamed into place, so a crash never leaves a half-unpacked pack.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::state::{FileRecord, Record, StateGuard};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::{ContentSpec, Extract};
use crate::download;

/// Upper bound for what a zip may unpack to, so a hostile archive cannot fill the disk.
const MAX_UNPACKED: u64 = 200_000_000_000;

pub struct MediaInstaller;

fn pack_dir(ctx: &Ctx, content: &ContentSpec, id: &str) -> PathBuf {
    ctx.paths.media_dir(&content.media).join(id)
}

fn remove_dir(p: &Path) -> Result<(), InstallError> {
    match fs::remove_dir_all(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("cannot remove", p)(e)),
    }
}

fn unzip(source: &Path, dest: &Path) -> Result<u64, InstallError> {
    let file = fs::File::open(source).map_err(io_err("cannot open", source))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| InstallError::Corrupt(format!("{}: not a valid zip: {e}", source.display())))?;
    let declared: u64 = (0..archive.len())
        .filter_map(|i| archive.by_index_raw(i).ok().map(|f| f.size()))
        .sum();
    if declared > MAX_UNPACKED {
        return Err(InstallError::Corrupt(format!(
            "{}: unpacks to {declared} bytes, refusing (limit {MAX_UNPACKED})",
            source.display()
        )));
    }
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| InstallError::Corrupt(format!("{}: {e}", source.display())))?;
        // `enclosed_name` rejects absolute paths and `..` (zip slip).
        let Some(rel) = entry.enclosed_name() else {
            return Err(InstallError::Corrupt(format!(
                "{}: unsafe path {:?} in the archive",
                source.display(),
                entry.name()
            )));
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(io_err("cannot create", &out))?;
            continue;
        }
        if let Some(dir) = out.parent() {
            fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
        }
        let mut f = fs::File::create(&out).map_err(io_err("cannot create", &out))?;
        total += io::copy(&mut entry, &mut f).map_err(io_err("cannot write", &out))?;
        if total > MAX_UNPACKED {
            return Err(InstallError::Corrupt(format!(
                "{}: unpacks to too much data",
                source.display()
            )));
        }
    }
    Ok(total)
}

impl Installer for MediaInstaller {
    fn plan(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &super::state::State,
    ) -> Result<Vec<String>, InstallError> {
        let content = content_of(input)?;
        let dir = pack_dir(ctx, content, &input.id);
        if state
            .packages
            .get(&input.id)
            .is_some_and(|r| r.files.first().is_some_and(|f| f.sha256 == input.sha256))
            && dir.exists()
        {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!(
                "{} {} into {}",
                if content.extract == Extract::Zip {
                    "unpack"
                } else {
                    "link"
                },
                input.source.display(),
                dir.display()
            ),
            format!("record {} in {}", input.id, ctx.paths.state_file().display()),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let content = content_of(input)?;
        let dir = pack_dir(ctx, content, &input.id);
        let current = state
            .packages
            .get(&input.id)
            .is_some_and(|r| r.files.first().is_some_and(|f| f.sha256 == input.sha256))
            && dir.exists();
        if !current {
            let meta = fs::metadata(&input.source).map_err(|_| {
                InstallError::Corrupt(format!(
                    "{}: the downloaded file {} is missing; run `kit download` first",
                    input.id,
                    input.source.display()
                ))
            })?;
            if meta.len() != input.size {
                return Err(InstallError::Corrupt(format!(
                    "{}: {} has {} bytes, expected {}",
                    input.id,
                    input.source.display(),
                    meta.len(),
                    input.size
                )));
            }
            if !input.verified {
                let actual =
                    download::hash_file(&input.source).map_err(|e| InstallError::Corrupt(e.to_string()))?;
                if actual != input.sha256 {
                    return Err(InstallError::Corrupt(format!(
                        "{}: checksum of {} does not match the catalog; not installing it",
                        input.id,
                        input.source.display()
                    )));
                }
            }
            let parent = dir.parent().unwrap_or(&dir).to_path_buf();
            fs::create_dir_all(&parent).map_err(io_err("cannot create", &parent))?;
            let tmp = parent.join(format!(".{}.unpacking", input.id));
            remove_dir(&tmp)?;
            fs::create_dir_all(&tmp).map_err(io_err("cannot create", &tmp))?;
            let result = match content.extract {
                Extract::Zip => unzip(&input.source, &tmp).map(|_| ()),
                Extract::None => {
                    let dest = tmp.join(&input.file_name);
                    fs::hard_link(&input.source, &dest)
                        .or_else(|_| fs::copy(&input.source, &dest).map(|_| ()))
                        .map_err(io_err("cannot place", &dest))
                }
            };
            if let Err(e) = result {
                let _ = remove_dir(&tmp);
                return Err(e);
            }
            remove_dir(&dir)?;
            fs::rename(&tmp, &dir).map_err(io_err("cannot move into place:", &dir))?;
        }
        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Files,
                version: input.version.clone(),
                license: input.license.clone(),
                installed_at,
                files: vec![FileRecord {
                    name: format!("{}/{}", content.media, input.id),
                    size: input.size,
                    sha256: input.sha256.clone(),
                    source: Some(input.source.clone()),
                    moved: false,
                }],
                service: None,
                app: None,
                map: None,
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(vec![format!(
            "{}: {} is in {}; services mounting the {} directory pick it up on their next scan",
            input.id,
            input.name,
            dir.display(),
            content.media
        )])
    }

    fn verify(&self, record: &Record, ctx: &Ctx, _deep: bool) -> Vec<Check> {
        record
            .files
            .iter()
            .map(|f| {
                let dir = ctx.paths.data_dir.join("media").join(&f.name);
                let n = fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
                Check {
                    name: format!("{}: {} present", record.id, f.name),
                    ok: n > 0,
                    detail: dir.display().to_string(),
                }
            })
            .collect()
    }

    fn uninstall(
        &self,
        id: &str,
        ctx: &Ctx,
        state: &mut StateGuard,
        purge: bool,
    ) -> Result<Vec<String>, InstallError> {
        let Some(record) = state.packages.remove(id) else {
            return Ok(vec![format!("{id} is not installed")]);
        };
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        for f in &record.files {
            remove_dir(&ctx.paths.data_dir.join("media").join(&f.name))?;
            if purge && let Some(src) = &f.source {
                let _ = fs::remove_file(src);
                let _ = fs::remove_file(src.with_extension("lock"));
                if let Some(dir) = src.parent() {
                    let _ = fs::remove_dir(dir);
                }
            }
        }
        Ok(vec![])
    }
}

fn content_of(input: &InstallInput) -> Result<&ContentSpec, InstallError> {
    input.content.as_ref().ok_or_else(|| {
        InstallError::Unsupported(format!(
            "{}: packages of type files without a `content` block cannot be installed yet",
            input.id
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            z.start_file(*name, opts).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    use crate::catalog::http::testing::FakeHttp;
    use crate::install::runner::testing::FakeRunner;
    use crate::install::{Ctx, Paths};
    use sha2::{Digest, Sha256};
    use std::time::Duration;

    struct Env {
        _d: tempfile::TempDir,
        paths: Paths,
        runner: FakeRunner,
        probe: FakeHttp,
    }

    fn env() -> Env {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path().join("data"), d.path().join("units")).unwrap();
        Env {
            _d: d,
            paths,
            runner: FakeRunner::new(|_, _| Ok(crate::install::runner::CmdOutput::ok())),
            probe: FakeHttp::default(),
        }
    }

    impl Env {
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                paths: self.paths.clone(),
                runner: &self.runner,
                probe: &self.probe,
                search_path: vec![],
                port: 6589,
                user: None,
                start_wait: Duration::ZERO,
                now: 1_790_000_000,
                check_port: false,
            }
        }

        fn pack(&self, entries: &[(&str, &[u8])]) -> InstallInput {
            let dir = self.paths.downloads_dir().join("music_pack");
            fs::create_dir_all(&dir).unwrap();
            let zp = dir.join("pack.zip");
            make_zip(&zp, entries);
            let bytes = fs::read(&zp).unwrap();
            InstallInput {
                id: "music_pack".into(),
                name: "Music pack".into(),
                license: "CC0-1.0".into(),
                version: None,
                source: zp,
                file_name: "pack.zip".into(),
                size: bytes.len() as u64,
                sha256: Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                verified: false,
                image: None,
                service: None,
                content: Some(ContentSpec {
                    media: "music".into(),
                    extract: Extract::Zip,
                }),
                flatpak: None,
                group: None,
                map: None,
            }
        }
    }

    #[test]
    fn install_unpacks_verifies_and_is_idempotent_then_uninstalls() {
        let e = env();
        let ctx = e.ctx();
        let input = e.pack(&[("a/01.flac", b"abc"), ("a/02.flac", b"de")]);
        let mut g = StateGuard::open(&e.paths).unwrap();
        MediaInstaller.install(&input, &ctx, &mut g).unwrap();
        let dir = e.paths.media_dir("music").join("music_pack");
        assert_eq!(fs::read(dir.join("a/01.flac")).unwrap(), b"abc");
        assert_eq!(g.packages["music_pack"].files[0].name, "music/music_pack");
        assert!(
            g.library_files().is_empty(),
            "media is not part of the Kiwix library"
        );
        assert!(MediaInstaller.verify(&g.packages["music_pack"].clone(), &ctx, false)[0].ok);

        // A second run changes nothing, even if the download is gone.
        fs::write(dir.join("a/marker"), "kept").unwrap();
        MediaInstaller.install(&input, &ctx, &mut g).unwrap();
        assert!(dir.join("a/marker").exists());

        MediaInstaller
            .uninstall("music_pack", &ctx, &mut g, true)
            .unwrap();
        assert!(!dir.exists() && g.packages.is_empty());
        assert!(!input.source.exists(), "--purge removes the download");
    }

    #[test]
    fn corrupt_download_is_refused_and_leaves_nothing() {
        let e = env();
        let ctx = e.ctx();
        let mut input = e.pack(&[("a.flac", b"x")]);
        input.sha256 = "0".repeat(64);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = MediaInstaller
            .install(&input, &ctx, &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match the catalog"), "{err}");
        assert!(!e.paths.media_dir("music").exists() && g.packages.is_empty());
    }

    #[test]
    fn evil_zip_leaves_no_half_unpacked_pack() {
        let e = env();
        let ctx = e.ctx();
        let input = e.pack(&[("ok.flac", b"x"), ("../../evil", b"y")]);
        let mut g = StateGuard::open(&e.paths).unwrap();
        assert!(MediaInstaller.install(&input, &ctx, &mut g).is_err());
        let media = e.paths.media_dir("music");
        assert_eq!(
            fs::read_dir(&media).map(|d| d.count()).unwrap_or(0),
            0,
            "temporary directory removed"
        );
        assert!(g.packages.is_empty());
    }

    #[test]
    fn unzips_nested_files() {
        let d = tempfile::tempdir().unwrap();
        let zp = d.path().join("a.zip");
        make_zip(&zp, &[("album/01.mp3", b"abc"), ("album/02.mp3", b"de")]);
        let out = d.path().join("out");
        fs::create_dir_all(&out).unwrap();
        assert_eq!(unzip(&zp, &out).unwrap(), 5);
        assert_eq!(fs::read(out.join("album/01.mp3")).unwrap(), b"abc");
    }

    #[test]
    fn zip_slip_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let zp = d.path().join("evil.zip");
        make_zip(&zp, &[("../escape.txt", b"x")]);
        let out = d.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let e = unzip(&zp, &out).unwrap_err().to_string();
        assert!(e.contains("unsafe path"), "{e}");
        assert!(!d.path().join("escape.txt").exists());
    }

    #[test]
    fn not_a_zip_is_corrupt() {
        let d = tempfile::tempdir().unwrap();
        let zp = d.path().join("x.zip");
        fs::write(&zp, b"hello").unwrap();
        assert!(matches!(unzip(&zp, d.path()), Err(InstallError::Corrupt(_))));
    }
}
