//! Installer for `zim` packages: hard-link into the library, list in `library.xml`, serve.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::reconcile::{ensure_tools, reconcile};
use super::state::{FileRecord, Record, State, StateGuard};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::download;

pub struct ZimInstaller;

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

fn remove_if_exists(p: &Path) -> Result<(), InstallError> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("cannot remove", p)(e)),
    }
}

/// Hard-links `source` to `dest`; if the filesystem cannot, moves it. Returns whether it moved.
fn place(source: &Path, dest: &Path) -> Result<bool, InstallError> {
    if fs::hard_link(source, dest).is_ok() {
        return Ok(false);
    }
    fs::rename(source, dest).map_err(io_err("cannot place into the library:", dest))?;
    Ok(true)
}

fn up_to_date(input: &InstallInput, ctx: &Ctx, state: &State) -> bool {
    let dest = ctx.paths.library_dir().join(&input.file_name);
    state.packages.get(&input.id).is_some_and(|r| {
        r.files.len() == 1
            && r.files[0].name == input.file_name
            && r.files[0].sha256 == input.sha256
            && r.category == input.category
    }) && fs::metadata(&dest).is_ok_and(|m| m.len() == input.size)
}

impl Installer for ZimInstaller {
    fn plan(&self, input: &InstallInput, ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
        if up_to_date(input, ctx, state) {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        let books = state.library_files().len() + usize::from(!state.packages.contains_key(&input.id));
        Ok(vec![
            format!(
                "link {} -> {}",
                input.source.display(),
                ctx.paths.library_dir().join(&input.file_name).display()
            ),
            format!("record {} in {}", input.id, ctx.paths.state_file().display()),
            format!(
                "rebuild {} with kiwix-manage ({books} book(s))",
                ctx.paths.library_xml().display()
            ),
            format!(
                "write and start systemd user unit {} on 127.0.0.1:{}",
                ctx.paths.unit_file().display(),
                ctx.port
            ),
            format!("update the landing page in {}", ctx.paths.portal_dir().display()),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        ensure_tools(ctx)?;
        let dest = ctx.paths.library_dir().join(&input.file_name);
        let prior = state.packages.get(&input.id).cloned();
        let current = up_to_date(input, ctx, state);

        let (moved, installed_at) = if current {
            let r = prior.as_ref().expect("up_to_date implies a record");
            (r.files[0].moved, r.installed_at)
        } else {
            fs::create_dir_all(ctx.paths.library_dir())
                .map_err(io_err("cannot create", &ctx.paths.library_dir()))?;
            let source_exists = input.source.exists();
            // After an interrupted install the data may already be in the library only.
            let data = if source_exists { &input.source } else { &dest };
            let meta = fs::metadata(data).map_err(|_| {
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
                    data.display(),
                    meta.len(),
                    input.size
                )));
            }
            if !input.verified {
                let actual = download::hash_file(data).map_err(|e| InstallError::Corrupt(e.to_string()))?;
                if actual != input.sha256 {
                    return Err(InstallError::Corrupt(format!(
                        "{}: checksum of {} does not match the catalog; not installing it",
                        input.id,
                        data.display()
                    )));
                }
            }
            let moved = if source_exists && !same_file(&input.source, &dest) {
                remove_if_exists(&dest)?;
                place(&input.source, &dest)?
            } else {
                !source_exists
            };
            (moved, ctx.now)
        };

        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Zim,
                version: input.version.clone(),
                license: input.license.clone(),
                installed_at,
                category: input.category.clone(),
                files: vec![FileRecord {
                    name: input.file_name.clone(),
                    size: input.size,
                    sha256: input.sha256.clone(),
                    source: (!moved).then(|| input.source.clone()),
                    moved,
                }],
                service: None,
                app: None,
                model: None,
            },
        );
        state.port = Some(ctx.port);
        state.save()?;

        // An update replaces the old file: drop it only now that the new state is on disk.
        if let Some(old) = prior {
            for f in old.files.iter().filter(|f| f.name != input.file_name) {
                remove_if_exists(&ctx.paths.library_dir().join(&f.name))?;
            }
        }
        reconcile(ctx, state)
    }

    fn verify(&self, record: &Record, ctx: &Ctx, deep: bool) -> Vec<Check> {
        let mut checks = Vec::new();
        for f in &record.files {
            let path = ctx.paths.library_dir().join(&f.name);
            let size = fs::metadata(&path).map(|m| m.len());
            let size_ok = size.as_ref().is_ok_and(|s| *s == f.size);
            checks.push(Check {
                name: format!("{}: {} present with the right size", record.id, f.name),
                ok: size_ok,
                detail: match size {
                    Ok(s) if s == f.size => path.display().to_string(),
                    Ok(s) => format!("{} has {s} bytes, expected {}", path.display(), f.size),
                    Err(e) => format!("{}: {e}", path.display()),
                },
            });
            if deep && size_ok {
                let (ok, detail) = match download::hash_file(&path) {
                    Ok(h) if h == f.sha256 => (true, "sha256 matches".to_string()),
                    Ok(h) => (false, format!("sha256 is {h}, expected {}", f.sha256)),
                    Err(e) => (false, e.to_string()),
                };
                checks.push(Check {
                    name: format!("{}: {} checksum", record.id, f.name),
                    ok,
                    detail,
                });
            }
        }
        checks
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
        // State first: if we crash afterwards, an unreferenced file is harmless, whereas a
        // record pointing at a deleted file would look like an installed package.
        state.save()?;

        let mut notes = Vec::new();
        for f in &record.files {
            remove_if_exists(&ctx.paths.library_dir().join(&f.name))?;
            match (&f.source, f.moved, purge) {
                (Some(src), _, true) => {
                    remove_if_exists(src)?;
                    // Leftovers of the download engine next to it.
                    for suffix in [".lock", ".part", ".part.state"] {
                        let mut extra = src.clone().into_os_string();
                        extra.push(suffix);
                        remove_if_exists(Path::new(&extra))?;
                    }
                    if let Some(dir) = src.parent() {
                        let _ = fs::remove_dir(dir); // only if empty
                    }
                }
                (Some(_), _, false) => notes.push(format!(
                    "{id}: the downloaded file is kept (use --purge to delete it) so a reinstall is instant"
                )),
                (None, true, false) => notes.push(format!(
                    "{id}: its file had been moved into the library, so removing it deleted the data; run `kit download` to fetch it again"
                )),
                (None, _, _) => {}
            }
        }
        notes.extend(reconcile(ctx, state)?);
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::CmdOutput;
    use super::super::runner::testing::{FakeRunner, system};
    use super::super::tools::fake_exe;
    use super::super::{Paths, service};
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use std::path::PathBuf;
    use std::time::Duration;

    const SHA: &str = "0e281a87af92a798297fc671f8b37004cb7874ac051e13876b7335be96649e59";

    struct Env {
        _dir: tempfile::TempDir,
        paths: Paths,
        bin: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            fs::create_dir_all(&bin).unwrap();
            for t in ["kiwix-manage", "kiwix-serve", "systemctl"] {
                fake_exe(&bin, t);
            }
            let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
            Self {
                _dir: dir,
                paths,
                bin,
            }
        }

        /// A downloaded file of `size` bytes whose recorded checksum is `SHA` (the engine verified it).
        fn download(&self, id: &str, name: &str, size: usize) -> InstallInput {
            let dir = self.paths.downloads_dir().join(id);
            fs::create_dir_all(&dir).unwrap();
            let source = dir.join(name);
            fs::write(&source, vec![7u8; size]).unwrap();
            InstallInput {
                id: id.into(),
                name: format!("Package {id}"),
                license: "CC-BY-SA-4.0".into(),
                version: Some("2026-10-01".into()),
                source,
                file_name: name.into(),
                size: size as u64,
                sha256: SHA.into(),
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
                category: None,
            }
        }

        fn ctx<'a>(&self, runner: &'a FakeRunner, probe: &'a FakeHttp) -> Ctx<'a> {
            Ctx {
                paths: self.paths.clone(),
                runner,
                probe,
                search_path: vec![self.bin.clone()],
                port: 6589,
                user: Some("tester".into()),
                start_wait: Duration::ZERO,
                now: 1_790_000_000,
                check_port: false,
            }
        }
    }

    fn healthy() -> FakeRunner {
        system(true)
    }

    fn install(env: &Env, runner: &FakeRunner, input: &InstallInput) -> Result<Vec<String>, InstallError> {
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths)?;
        ZimInstaller.install(input, &env.ctx(runner, &probe), &mut st)
    }

    #[test]
    fn install_links_records_lists_and_starts() {
        let env = Env::new();
        let input = env.download("wiki", "wiki_2026-10.zim", 1000);
        let runner = healthy();
        install(&env, &runner, &input).unwrap();

        let lib = env.paths.library_dir().join("wiki_2026-10.zim");
        assert!(same_file(&input.source, &lib), "hard link, not a copy");
        assert!(input.source.exists(), "download is kept");
        let xml = fs::read_to_string(env.paths.library_xml()).unwrap();
        assert!(xml.contains("wiki_2026-10.zim"));

        let unit = fs::read_to_string(env.paths.unit_file()).unwrap();
        assert!(
            service::is_managed(&unit) && unit.contains("--port=6589") && unit.contains("--monitorLibrary")
        );
        assert_eq!(runner.count("systemctl --user daemon-reload"), 1);
        assert_eq!(runner.count("systemctl --user enable --now kit-kiwix.service"), 1);
        assert_eq!(runner.count("restart"), 0);

        let st = StateGuard::open(&env.paths).unwrap();
        let rec = &st.packages["wiki"];
        assert_eq!(
            (rec.files[0].size, rec.files[0].moved, rec.installed_at),
            (1000, false, 1_790_000_000)
        );
        assert_eq!(st.port, Some(6589));
        assert!(env.paths.portal_dir().join("index.html").exists());
    }

    #[test]
    fn installing_twice_changes_nothing() {
        let env = Env::new();
        let input = env.download("wiki", "w.zim", 100);
        let runner = healthy();
        install(&env, &runner, &input).unwrap();
        let state_before = fs::read(env.paths.state_file()).unwrap();
        let unit_before = fs::read(env.paths.unit_file()).unwrap();
        let reloads = runner.count("daemon-reload");

        let mut again = input.clone();
        again.verified = false; // a no-op must not even need to hash
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths).unwrap();
        let mut ctx = env.ctx(&runner, &probe);
        ctx.now += 999; // time passes; the record must keep its original install time
        ZimInstaller.install(&again, &ctx, &mut st).unwrap();
        drop(st);

        assert_eq!(fs::read(env.paths.state_file()).unwrap(), state_before);
        assert_eq!(fs::read(env.paths.unit_file()).unwrap(), unit_before);
        assert_eq!(
            runner.count("daemon-reload"),
            reloads,
            "unit unchanged -> no reload"
        );
        assert_eq!(runner.count("restart"), 0);
    }

    #[test]
    fn second_package_extends_library_without_restart_when_monitoring() {
        let env = Env::new();
        let runner = healthy();
        install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        // The service is now "active".
        install(&env, &runner, &env.download("b", "b.zim", 20)).unwrap();
        let xml = fs::read_to_string(env.paths.library_xml()).unwrap();
        assert!(xml.contains("a.zim") && xml.contains("b.zim"));
        assert_eq!(runner.count("restart"), 0, "--monitorLibrary picks up the change");
    }

    #[test]
    fn without_monitor_support_the_service_is_restarted_on_library_change() {
        let env = Env::new();
        let runner = system(false);
        install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        install(&env, &runner, &env.download("b", "b.zim", 20)).unwrap();
        assert!(
            !fs::read_to_string(env.paths.unit_file())
                .unwrap()
                .contains("monitorLibrary")
        );
        assert_eq!(runner.count("restart"), 1);
    }

    #[test]
    fn changing_the_port_rewrites_the_unit_and_restarts() {
        let env = Env::new();
        let runner = healthy();
        let input = env.download("a", "a.zim", 10);
        install(&env, &runner, &input).unwrap();
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths).unwrap();
        let mut ctx = env.ctx(&runner, &probe);
        ctx.port = 6590;
        ZimInstaller.install(&input, &ctx, &mut st).unwrap();
        assert!(
            fs::read_to_string(env.paths.unit_file())
                .unwrap()
                .contains("--port=6590")
        );
        assert_eq!(runner.count("restart"), 1);
        assert_eq!(st.port, Some(6590));
    }

    #[test]
    fn corrupt_or_missing_downloads_are_refused_before_anything_changes() {
        let env = Env::new();
        let runner = healthy();
        let mut input = env.download("a", "a.zim", 10);
        input.size = 11;
        let err = install(&env, &runner, &input).unwrap_err();
        assert!(matches!(err, InstallError::Corrupt(_)), "{err}");

        let mut gone = env.download("b", "b.zim", 10);
        fs::remove_file(&gone.source).unwrap();
        gone.verified = true;
        let err = install(&env, &runner, &gone).unwrap_err().to_string();
        assert!(err.contains("kit download"), "{err}");

        assert!(!env.paths.unit_file().exists() && !env.paths.library_xml().exists());
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert!(StateGuard::open(&env.paths).unwrap().packages.is_empty());
    }

    #[test]
    fn unverified_input_is_hashed_and_a_wrong_checksum_is_rejected() {
        let env = Env::new();
        let runner = healthy();
        let mut input = env.download("a", "a.zim", 10);
        input.verified = false; // contents are ten 0x07 bytes, which do not hash to SHA
        let err = install(&env, &runner, &input).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        assert!(!env.paths.library_dir().join("a.zim").exists());
    }

    #[test]
    fn missing_tools_fail_early_with_a_hint() {
        let env = Env::new();
        fs::remove_file(env.bin.join("kiwix-serve")).unwrap();
        let runner = healthy();
        let err = install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap_err();
        assert!(
            matches!(&err, InstallError::MissingTool { tool, .. } if tool == "kiwix-serve"),
            "{err}"
        );
        assert!(
            !env.paths.library_dir().join("a.zim").exists(),
            "nothing was changed"
        );
    }

    #[test]
    fn busy_port_is_reported_when_the_service_is_not_running() {
        let env = Env::new();
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = blocker.local_addr().unwrap().port();
        let runner = FakeRunner::new(|prog, args| {
            if prog == "kiwix-manage" {
                fs::write(args[0], "x").unwrap();
            }
            if args.contains(&"is-active") {
                return Ok(CmdOutput::failed("inactive"));
            }
            Ok(CmdOutput::ok())
        });
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths).unwrap();
        let mut ctx = env.ctx(&runner, &probe);
        ctx.port = port;
        ctx.check_port = true;
        let err = ZimInstaller
            .install(&env.download("a", "a.zim", 10), &ctx, &mut st)
            .unwrap_err();
        assert!(matches!(err, InstallError::PortInUse(p) if p == port), "{err}");
        assert_eq!(runner.count("enable --now"), 0);
    }

    #[test]
    fn service_that_does_not_come_up_is_an_error() {
        let env = Env::new();
        let runner = FakeRunner::new(|prog, args| {
            if prog == "kiwix-manage" {
                fs::write(args[0], "x").unwrap();
            }
            if args.contains(&"is-active") {
                return Ok(CmdOutput::failed("failed"));
            }
            Ok(CmdOutput::ok())
        });
        let err = install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap_err();
        assert!(matches!(err, InstallError::ServiceFailed), "{err}");
    }

    #[test]
    fn kiwix_manage_failure_surfaces_its_message_and_keeps_the_old_library() {
        let env = Env::new();
        let runner = healthy();
        install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        let old = fs::read(env.paths.library_xml()).unwrap();
        let broken = FakeRunner::new(|prog, _| {
            Ok(if prog == "kiwix-manage" {
                CmdOutput::failed("not a zim file")
            } else {
                CmdOutput::ok()
            })
        });
        let err = install(&env, &broken, &env.download("b", "b.zim", 10))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a zim file"), "{err}");
        assert_eq!(fs::read(env.paths.library_xml()).unwrap(), old);
        assert!(!env.paths.library_xml().with_extension("xml.tmp").exists());
    }

    #[test]
    fn foreign_unit_is_never_overwritten() {
        let env = Env::new();
        fs::create_dir_all(&env.paths.unit_dir).unwrap();
        fs::write(env.paths.unit_file(), "[Unit]\nDescription=mine\n").unwrap();
        let err = install(&env, &healthy(), &env.download("a", "a.zim", 10)).unwrap_err();
        assert!(matches!(err, InstallError::ForeignUnit(_)), "{err}");
        assert_eq!(
            fs::read_to_string(env.paths.unit_file()).unwrap(),
            "[Unit]\nDescription=mine\n"
        );
    }

    #[test]
    fn hard_link_failure_falls_back_to_moving() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("s.zim");
        fs::write(&src, "data").unwrap();
        // A directory as destination makes hard_link fail (EEXIST); rename onto a file path works.
        let dest = d.path().join("d.zim");
        assert!(!place(&src, &dest).unwrap(), "same filesystem: linked");
        let src2 = d.path().join("s2.zim");
        fs::write(&src2, "data").unwrap();
        let dest2 = d.path().join("d2.zim");
        fs::write(&dest2, "occupied").unwrap();
        assert!(
            place(&src2, &dest2).unwrap(),
            "link impossible onto an existing file: moved"
        );
        assert!(!src2.exists() && fs::read_to_string(&dest2).unwrap() == "data");
    }

    #[test]
    fn interrupted_install_is_repaired_by_running_it_again() {
        let env = Env::new();
        let input = env.download("a", "a.zim", 10);
        // Crash after the file was moved into the library but before the state was written.
        fs::create_dir_all(env.paths.library_dir()).unwrap();
        fs::rename(&input.source, env.paths.library_dir().join("a.zim")).unwrap();
        let runner = healthy();
        install(&env, &runner, &input).unwrap();
        let st = StateGuard::open(&env.paths).unwrap();
        assert!(st.packages["a"].files[0].moved && st.packages["a"].files[0].source.is_none());
        assert!(
            fs::read_to_string(env.paths.library_xml())
                .unwrap()
                .contains("a.zim")
        );
    }

    #[test]
    fn update_replaces_the_old_file_after_the_new_state_is_saved() {
        let env = Env::new();
        let runner = healthy();
        install(&env, &runner, &env.download("wiki", "wiki_2026-09.zim", 10)).unwrap();
        install(&env, &runner, &env.download("wiki", "wiki_2026-10.zim", 12)).unwrap();
        assert!(!env.paths.library_dir().join("wiki_2026-09.zim").exists());
        assert!(env.paths.library_dir().join("wiki_2026-10.zim").exists());
        let xml = fs::read_to_string(env.paths.library_xml()).unwrap();
        assert!(xml.contains("2026-10") && !xml.contains("2026-09"));
    }

    #[test]
    fn uninstall_one_of_two_keeps_the_service() {
        let env = Env::new();
        let runner = healthy();
        let (a, b) = (env.download("a", "a.zim", 10), env.download("b", "b.zim", 20));
        install(&env, &runner, &a).unwrap();
        install(&env, &runner, &b).unwrap();
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths).unwrap();
        ZimInstaller
            .uninstall("a", &env.ctx(&runner, &probe), &mut st, false)
            .unwrap();
        assert!(!env.paths.library_dir().join("a.zim").exists() && a.source.exists());
        assert!(env.paths.unit_file().exists());
        let xml = fs::read_to_string(env.paths.library_xml()).unwrap();
        assert!(!xml.contains("a.zim") && xml.contains("b.zim"));
        assert_eq!(runner.count("disable"), 0);
    }

    #[test]
    fn uninstalling_the_last_package_removes_everything_of_ours() {
        let env = Env::new();
        let runner = healthy();
        let a = env.download("a", "a.zim", 10);
        install(&env, &runner, &a).unwrap();
        let probe = FakeHttp::default();
        let mut st = StateGuard::open(&env.paths).unwrap();
        let notes = ZimInstaller
            .uninstall("a", &env.ctx(&runner, &probe), &mut st, true)
            .unwrap();
        drop(st);

        assert_eq!(runner.count("disable --now kit-kiwix.service"), 1);
        assert!(!env.paths.unit_file().exists());
        assert!(!env.paths.library_xml().exists() && !env.paths.portal_dir().exists());
        assert!(!env.paths.library_dir().join("a.zim").exists());
        assert!(
            !a.source.exists() && !a.source.parent().unwrap().exists(),
            "purged, directory removed"
        );
        assert!(StateGuard::open(&env.paths).unwrap().packages.is_empty());
        assert!(notes.iter().all(|n| !n.contains("deleted the data")));
    }

    #[test]
    fn uninstall_is_idempotent_and_warns_when_the_library_copy_was_the_only_one() {
        let env = Env::new();
        let runner = healthy();
        let probe = FakeHttp::default();
        let input = env.download("a", "a.zim", 10);
        fs::create_dir_all(env.paths.library_dir()).unwrap();
        fs::rename(&input.source, env.paths.library_dir().join("a.zim")).unwrap(); // simulate a move
        install(&env, &runner, &input).unwrap();

        let mut st = StateGuard::open(&env.paths).unwrap();
        let notes = ZimInstaller
            .uninstall("a", &env.ctx(&runner, &probe), &mut st, false)
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("deleted the data")), "{notes:?}");
        let again = ZimInstaller
            .uninstall("a", &env.ctx(&runner, &probe), &mut st, false)
            .unwrap();
        assert_eq!(again, vec!["a is not installed"]);
    }

    #[test]
    fn linger_hint_is_given_when_the_service_would_stop_at_logout() {
        let env = Env::new();
        fake_exe(&env.bin, "loginctl");
        let runner = FakeRunner::new(|prog, args| {
            match prog {
                "kiwix-manage" => fs::write(args[0], "x").unwrap(),
                "loginctl" => {
                    return Ok(CmdOutput {
                        success: true,
                        stdout: "Linger=no\n".into(),
                        stderr: String::new(),
                    });
                }
                _ => {}
            }
            Ok(CmdOutput::ok())
        });
        let notes = install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        assert!(
            notes.iter().any(|n| n.contains("enable-linger tester")),
            "{notes:?}"
        );
    }

    #[test]
    fn verify_reports_what_is_wrong() {
        let env = Env::new();
        let runner = healthy();
        install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        let probe = FakeHttp::default();
        let ctx = env.ctx(&runner, &probe);
        let st = StateGuard::open(&env.paths).unwrap();

        let good = ZimInstaller.verify(&st.packages["a"], &ctx, false);
        assert!(good.iter().all(|c| c.ok), "{good:?}");

        fs::write(env.paths.library_dir().join("a.zim"), "short").unwrap();
        // The hard link means the download changed too; that is what a damaged disk looks like.
        let bad = ZimInstaller.verify(&st.packages["a"], &ctx, true);
        assert!(
            bad.iter().any(|c| !c.ok && c.detail.contains("expected 10")),
            "{bad:?}"
        );

        fs::remove_file(env.paths.library_dir().join("a.zim")).unwrap();
        assert!(
            ZimInstaller
                .verify(&st.packages["a"], &ctx, false)
                .iter()
                .any(|c| !c.ok)
        );
    }

    #[test]
    fn deep_verify_detects_flipped_bytes() {
        let env = Env::new();
        let runner = healthy();
        let mut input = env.download("a", "a.zim", 10);
        input.sha256 = {
            use sha2::{Digest, Sha256};
            crate::download::state::to_hex(&Sha256::digest([7u8; 10]))
        };
        install(&env, &runner, &input).unwrap();
        let probe = FakeHttp::default();
        let ctx = env.ctx(&runner, &probe);
        let st = StateGuard::open(&env.paths).unwrap();
        assert!(
            ZimInstaller
                .verify(&st.packages["a"], &ctx, true)
                .iter()
                .all(|c| c.ok)
        );
        fs::write(env.paths.library_dir().join("a.zim"), [8u8; 10]).unwrap();
        let checks = ZimInstaller.verify(&st.packages["a"], &ctx, true);
        assert!(
            checks.iter().any(|c| !c.ok && c.name.contains("checksum")),
            "{checks:?}"
        );
    }

    #[test]
    fn service_verification_checks_unit_service_and_served_books() {
        let env = Env::new();
        let runner = healthy();
        install(&env, &runner, &env.download("a", "a.zim", 10)).unwrap();
        let st = StateGuard::open(&env.paths).unwrap();
        let url = "http://127.0.0.1:6589/catalog/v2/entries?count=1";

        let probe = FakeHttp::default().with(url, "<feed><totalResults>1</totalResults></feed>");
        let checks = super::super::reconcile::verify_service(&env.ctx(&runner, &probe), &st);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");

        let probe = FakeHttp::default().with(url, "<feed><totalResults>0</totalResults></feed>");
        let checks = super::super::reconcile::verify_service(&env.ctx(&runner, &probe), &st);
        assert!(
            checks.iter().any(|c| !c.ok && c.name.contains("serves")),
            "{checks:?}"
        );

        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let checks = super::super::reconcile::verify_service(&env.ctx(&runner, &down), &st);
        assert!(
            checks.iter().any(|c| !c.ok && c.name == "server answers"),
            "{checks:?}"
        );

        let inactive = FakeRunner::new(|_, args| {
            Ok(if args.contains(&"is-active") {
                CmdOutput::failed("inactive")
            } else {
                CmdOutput::ok()
            })
        });
        let checks = super::super::reconcile::verify_service(&env.ctx(&inactive, &probe), &st);
        assert!(
            checks.iter().any(|c| !c.ok && c.name == "service is running"),
            "{checks:?}"
        );
    }

    #[test]
    fn plan_describes_the_steps_without_touching_anything() {
        let env = Env::new();
        let runner = healthy();
        let probe = FakeHttp::default();
        let input = env.download("a", "a.zim", 10);
        let st = StateGuard::open(&env.paths).unwrap();
        let steps = ZimInstaller.plan(&input, &env.ctx(&runner, &probe), &st).unwrap();
        assert_eq!(steps.len(), 5);
        assert!(steps.iter().any(|s| s.contains("127.0.0.1:6589")));
        assert!(runner.calls().is_empty() && !env.paths.library_dir().exists());
    }

    #[test]
    fn unsupported_types_are_rejected() {
        for t in [
            PackageType::Zim,
            PackageType::Files,
            PackageType::Container,
            PackageType::Model,
        ] {
            assert!(super::super::installer_for(t).is_ok());
        }
        let e = super::super::installer_for(PackageType::Apt)
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("cannot be installed yet"), "{e}");
    }
}
