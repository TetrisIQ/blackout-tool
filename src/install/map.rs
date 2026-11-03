//! Installer for `map` packages: a region cut out of the newest Protomaps planet build with
//! `pmtiles extract` (HTTP range requests, so only the region is transferred). The planet files
//! are dated and have no stable URL, so there is no checksum to compare beforehand; kit checks the
//! result with `pmtiles verify` and records its own SHA-256 for `kit verify --deep`.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use super::state::{FileRecord, MapRecord, Record, State, StateGuard};
use super::tools::{require, s};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::MapRef;
use crate::download;

/// Lists the published planet builds (`key`, `size`, `uploaded`).
pub const BUILDS_URL: &str = "https://build-metadata.protomaps.dev/builds.json";
/// Planet builds are downloaded from here, by key.
pub const BUILD_BASE: &str = "https://build.protomaps.com";

pub struct MapInstaller;

#[derive(Deserialize)]
struct Build {
    key: String,
    uploaded: String,
}

/// The key of the newest build, e.g. `20261026.pmtiles`.
fn newest_build(ctx: &Ctx) -> Result<String, InstallError> {
    let body = ctx.probe.get(BUILDS_URL).map_err(|e| {
        InstallError::Unsupported(format!(
            "cannot list the Protomaps builds (needs the internet): {e}"
        ))
    })?;
    let builds: Vec<Build> = serde_json::from_str(&body)
        .map_err(|e| InstallError::Corrupt(format!("{BUILDS_URL}: unexpected answer: {e}")))?;
    builds
        .into_iter()
        .filter(|b| b.key.ends_with(".pmtiles"))
        .max_by(|a, b| a.uploaded.cmp(&b.uploaded))
        .map(|b| b.key)
        .ok_or_else(|| InstallError::Corrupt(format!("{BUILDS_URL} lists no builds")))
}

fn need_map(input: &InstallInput) -> Result<&MapRef, InstallError> {
    input
        .map
        .as_ref()
        .ok_or_else(|| InstallError::Corrupt(format!("{}: the manifest has no source.map", input.id)))
}

fn file_name(id: &str) -> String {
    format!("{id}.pmtiles")
}

/// File name of the place-name index next to the map (see `geocode`).
pub fn places_name(id: &str) -> String {
    format!("{id}.places.json")
}

/// Builds the place-name index of an installed map unless it is there and newer than the map.
/// The map works without it (only the search box is missing), so a failure is a note, not an error.
fn ensure_places(ctx: &Ctx, id: &str) -> Option<String> {
    let dir = ctx.paths.maps_dir();
    let (map, index) = (dir.join(file_name(id)), dir.join(places_name(id)));
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    if let (Some(m), Some(i)) = (modified(&map), modified(&index))
        && i >= m
    {
        return None;
    }
    let result = crate::geocode::places_json(&map)
        .map_err(|e| e.to_string())
        .and_then(|json| {
            let part = dir.join(format!("{}.part", places_name(id)));
            fs::write(&part, json)
                .and_then(|()| fs::rename(&part, &index))
                .map_err(|e| format!("cannot write {}: {e}", index.display()))
        });
    result
        .err()
        .map(|e| format!("{id}: the place search is not available ({e}); the map itself works"))
}

/// `--bbox=west,south,east,north`; `{}` on an `f64` prints the shortest exact form.
fn bbox_arg(m: &MapRef) -> String {
    let [w, s, e, n] = m.bbox;
    format!("--bbox={w},{s},{e},{n}")
}

fn run(ctx: &Ctx, tool: &str, args: &[&str]) -> Result<(), InstallError> {
    let out = ctx
        .runner
        .run(tool, args)
        .map_err(io_err("cannot run", Path::new(tool)))?;
    if out.success {
        Ok(())
    } else {
        Err(InstallError::Command {
            command: format!("{tool} {}", args.join(" ")),
            stderr: out.stderr.trim().to_string(),
        })
    }
}

impl Installer for MapInstaller {
    fn plan(&self, input: &InstallInput, ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
        let m = need_map(input)?;
        let dest = ctx.paths.maps_dir().join(file_name(&input.id));
        if state.packages.contains_key(&input.id) && dest.exists() {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!(
                "pmtiles extract the newest build from {BUILD_BASE} ({}, maxzoom {}) into {} \
                 (needs the internet once; only the region is transferred)",
                bbox_arg(m),
                m.maxzoom,
                dest.display()
            ),
            "check the file with `pmtiles verify` and record its SHA-256".into(),
            "link it into the portal's map viewer (served by `kit net enable`)".into(),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let m = need_map(input)?;
        let dir = ctx.paths.maps_dir();
        let dest = dir.join(file_name(&input.id));
        if state.packages.contains_key(&input.id) && dest.exists() {
            let note = ensure_places(ctx, &input.id);
            super::portal::write(&ctx.paths, state)?;
            return Ok(note.into_iter().collect());
        }
        let tool = require("pmtiles", &ctx.search_path)?;
        let tool = s(&tool);
        let build = newest_build(ctx)?;
        fs::create_dir_all(&dir).map_err(io_err("cannot create", &dir))?;
        let part = dir.join(format!("{}.part", file_name(&input.id)));
        let _ = fs::remove_file(&part);
        let result = (|| {
            let (url, part_s) = (format!("{BUILD_BASE}/{build}"), s(&part));
            run(
                ctx,
                tool,
                &[
                    "extract",
                    &url,
                    part_s,
                    &bbox_arg(m),
                    &format!("--maxzoom={}", m.maxzoom),
                ],
            )?;
            run(ctx, tool, &["verify", part_s])?;
            let size = fs::metadata(&part).map_err(io_err("cannot read", &part))?.len();
            if size == 0 {
                return Err(InstallError::Corrupt(format!(
                    "{}: the extract is empty",
                    input.id
                )));
            }
            let sha256 = download::hash_file(&part).map_err(|e| InstallError::Corrupt(e.to_string()))?;
            fs::rename(&part, &dest).map_err(io_err("cannot move into place:", &dest))?;
            Ok((size, sha256))
        })();
        let (size, sha256) = result.inspect_err(|_| {
            let _ = fs::remove_file(&part);
        })?;
        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Map,
                version: Some(build.trim_end_matches(".pmtiles").to_string()),
                license: input.license.clone(),
                installed_at,
                category: None,
                files: vec![FileRecord {
                    name: file_name(&input.id),
                    size,
                    sha256,
                    source: None,
                    moved: false,
                }],
                service: None,
                app: None,
                map: Some(MapRecord {
                    bbox: m.bbox,
                    maxzoom: m.maxzoom,
                }),
                model: None,
            },
        );
        state.save()?;
        let note = ensure_places(ctx, &input.id);
        super::portal::write(&ctx.paths, state)?;
        let mut notes = vec![
            "maps are shown by the portal at https://kit.local/maps/ (run `kit net enable`); \
             a file:// page cannot read them"
                .into(),
        ];
        notes.extend(note);
        Ok(notes)
    }

    fn verify(&self, record: &Record, ctx: &Ctx, deep: bool) -> Vec<Check> {
        record
            .files
            .iter()
            .map(|f| {
                let path = ctx.paths.maps_dir().join(&f.name);
                let (ok, detail) = match fs::metadata(&path) {
                    Err(_) => (
                        false,
                        format!(
                            "{} is missing; reinstall with: kit install --select {}",
                            path.display(),
                            record.id
                        ),
                    ),
                    Ok(meta) if meta.len() != f.size => (
                        false,
                        format!("{} has {} bytes, expected {}", path.display(), meta.len(), f.size),
                    ),
                    Ok(_) if deep => match download::hash_file(&path) {
                        Ok(h) if h == f.sha256 => (true, "checksum matches".into()),
                        Ok(_) => (false, format!("{} was changed since the install", path.display())),
                        Err(e) => (false, e.to_string()),
                    },
                    Ok(_) => (true, path.display().to_string()),
                };
                Check {
                    name: format!("{}: {}", record.id, f.name),
                    ok,
                    detail,
                }
            })
            .collect()
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
        for f in &record.files {
            let path = ctx.paths.maps_dir().join(&f.name);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("cannot remove", &path)(e)),
            }
        }
        let _ = fs::remove_file(ctx.paths.maps_dir().join(places_name(id)));
        let _ = fs::remove_dir(ctx.paths.maps_dir());
        super::portal::write(&ctx.paths, state)?;
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::CmdOutput;
    use super::super::runner::testing::FakeRunner;
    use super::super::tools::fake_exe;
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use std::path::PathBuf;
    use std::time::Duration;

    const BUILDS: &str = r#"[
        {"key":"20261019.pmtiles","size":1,"uploaded":"2026-10-19T10:00:00.000Z"},
        {"key":"20261026.pmtiles","size":1,"uploaded":"2026-10-26T10:00:00.000Z"},
        {"key":"20261012.pmtiles","size":1,"uploaded":"2026-10-12T10:00:00.000Z"}]"#;

    struct Env {
        _dir: tempfile::TempDir,
        paths: super::super::Paths,
        bin: PathBuf,
    }

    impl Env {
        fn new(with_tool: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            if with_tool {
                fake_exe(&bin, "pmtiles");
            }
            let paths = super::super::Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
            Self {
                _dir: dir,
                paths,
                bin,
            }
        }

        fn ctx<'a>(&self, runner: &'a FakeRunner, probe: &'a FakeHttp) -> Ctx<'a> {
            Ctx {
                paths: self.paths.clone(),
                runner,
                probe,
                search_path: vec![self.bin.clone()],
                port: 6589,
                user: None,
                start_wait: Duration::ZERO,
                now: 1_790_000_000,
                check_port: false,
            }
        }
    }

    fn input() -> InstallInput {
        InstallInput {
            id: "map_test".into(),
            name: "Test region".into(),
            license: "ODbL-1.0".into(),
            version: None,
            source: PathBuf::new(),
            file_name: String::new(),
            size: 0,
            sha256: String::new(),
            verified: false,
            image: None,
            service: None,
            content: None,
            flatpak: None,
            group: None,
            map: Some(MapRef {
                bbox: [5.86, 47.27, 15.04, 55.06],
                maxzoom: 12,
            }),
            collection: None,
            ollama: None,
            model_spec: None,
            category: None,
        }
    }

    /// `extract` writes the destination file; `verify` succeeds unless `verify_ok` is false.
    fn pmtiles(verify_ok: bool) -> FakeRunner {
        FakeRunner::new(move |_, args| {
            Ok(match args.first().copied() {
                Some("extract") => {
                    fs::write(args[2], b"PMTiles-bytes").unwrap();
                    CmdOutput::ok()
                }
                Some("verify") if !verify_ok => CmdOutput::failed("bad directory"),
                _ => CmdOutput::ok(),
            })
        })
    }

    fn http() -> FakeHttp {
        FakeHttp::default().with(BUILDS_URL, BUILDS)
    }

    #[test]
    fn extracts_the_newest_build_verifies_and_records_the_checksum() {
        let e = Env::new(true);
        let (r, p) = (pmtiles(true), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        MapInstaller.install(&input(), &ctx, &mut g).unwrap();
        assert_eq!(
            r.count("extract https://build.protomaps.com/20261026.pmtiles"),
            1,
            "{:?}",
            r.calls()
        );
        assert_eq!(r.count("--bbox=5.86,47.27,15.04,55.06 --maxzoom=12"), 1);
        assert_eq!(r.count("verify"), 1);
        let rec = &g.packages["map_test"];
        assert_eq!(rec.kind, PackageType::Map);
        assert_eq!(rec.version.as_deref(), Some("20261026"));
        assert_eq!(rec.files[0].size, 13);
        assert_eq!(rec.files[0].sha256.len(), 64);
        assert!(e.paths.maps_dir().join("map_test.pmtiles").exists());
        assert!(!e.paths.maps_dir().join("map_test.pmtiles.part").exists());
        // The viewer and the data link are on the portal.
        let maps = e.paths.portal_dir().join("maps");
        assert!(maps.join("index.html").exists() && maps.join("regions.json").exists());
        assert!(maps.join("data/map_test.pmtiles").exists());
    }

    #[test]
    fn a_second_run_does_nothing() {
        let e = Env::new(true);
        let (r, p) = (pmtiles(true), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        MapInstaller.install(&input(), &ctx, &mut g).unwrap();
        MapInstaller.install(&input(), &ctx, &mut g).unwrap();
        assert_eq!(r.count("extract"), 1);
        let plan = MapInstaller.plan(&input(), &ctx, &g).unwrap();
        assert!(plan[0].contains("already installed"));
    }

    #[test]
    fn the_place_index_is_built_once_and_removed_with_the_map() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/maps/mitte.pmtiles");
        let e = Env::new(true);
        let (r, p) = (pmtiles(true), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        // The fake `pmtiles` writes junk: the install works, the search is reported as missing.
        let notes = MapInstaller.install(&input(), &ctx, &mut g).unwrap();
        let index = e.paths.maps_dir().join("map_test.places.json");
        assert!(
            notes.iter().any(|n| n.contains("place search is not available")),
            "{notes:?}"
        );
        assert!(!index.exists());
        // A real map file: the next run (also a no-op install) builds the index and links it.
        fs::copy(fixture, e.paths.maps_dir().join("map_test.pmtiles")).unwrap();
        assert!(MapInstaller.install(&input(), &ctx, &mut g).unwrap().is_empty());
        assert!(fs::read_to_string(&index).unwrap().contains("\"Mitte\""));
        let regions = fs::read_to_string(e.paths.portal_dir().join("maps/regions.json")).unwrap();
        assert!(regions.contains("/maps/data/map_test.places.json"), "{regions}");
        MapInstaller.uninstall("map_test", &ctx, &mut g, false).unwrap();
        assert!(!index.exists());
    }

    #[test]
    fn missing_tool_is_explained_and_nothing_is_touched() {
        let e = Env::new(false);
        let (r, p) = (pmtiles(true), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = MapInstaller
            .install(&input(), &ctx, &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pmtiles") && err.contains("go-pmtiles"), "{err}");
        assert!(g.packages.is_empty() && r.calls().is_empty());
    }

    #[test]
    fn a_failed_verify_leaves_no_file_and_no_record() {
        let e = Env::new(true);
        let (r, p) = (pmtiles(false), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = MapInstaller
            .install(&input(), &ctx, &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad directory"), "{err}");
        assert!(g.packages.is_empty());
        let left: Vec<_> = fs::read_dir(e.paths.maps_dir()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn offline_is_a_clear_error() {
        let e = Env::new(true);
        let (r, p) = (
            pmtiles(true),
            FakeHttp {
                fail: true,
                ..FakeHttp::default()
            },
        );
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = MapInstaller
            .install(&input(), &ctx, &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs the internet"), "{err}");
    }

    #[test]
    fn verify_detects_changes_and_uninstall_cleans_up() {
        let e = Env::new(true);
        let (r, p) = (pmtiles(true), http());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        MapInstaller.install(&input(), &ctx, &mut g).unwrap();
        let rec = g.packages["map_test"].clone();
        assert!(MapInstaller.verify(&rec, &ctx, true).iter().all(|c| c.ok));
        // Same size, different content: only the deep check notices.
        fs::write(e.paths.maps_dir().join("map_test.pmtiles"), b"PMTiles-BYTES").unwrap();
        assert!(MapInstaller.verify(&rec, &ctx, false).iter().all(|c| c.ok));
        assert!(!MapInstaller.verify(&rec, &ctx, true).iter().all(|c| c.ok));

        MapInstaller.uninstall("map_test", &ctx, &mut g, false).unwrap();
        assert!(g.packages.is_empty() && !e.paths.maps_dir().exists() && !e.paths.portal_dir().exists());
        assert!(!MapInstaller.verify(&rec, &ctx, false).iter().all(|c| c.ok));
        // Idempotent.
        MapInstaller.uninstall("map_test", &ctx, &mut g, false).unwrap();
    }
}
