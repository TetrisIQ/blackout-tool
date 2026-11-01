//! Installer for `flatpak` packages: local programs (games) installed per user from Flathub.
//! Flatpak verifies the signed commits itself; kit adds the Flathub remote to the user's
//! installation only when no `flathub` remote exists, and removes only what it installed.

use super::reconcile::cmd_failed;
use super::runner::CmdOutput;
use super::state::{AppRecord, Record, State, StateGuard};
use super::tools::{require, s};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::FLATPAK_REMOTE;

const REMOTE_URL: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

pub struct FlatpakInstaller;

fn need<'a, T>(v: &'a Option<T>, id: &str, what: &str) -> Result<&'a T, InstallError> {
    v.as_ref()
        .ok_or_else(|| InstallError::Corrupt(format!("{id}: the manifest has no {what}")))
}

fn flatpak(ctx: &Ctx, tool: &str, args: &[&str]) -> Result<CmdOutput, InstallError> {
    ctx.runner
        .run(tool, args)
        .map_err(io_err("cannot run", std::path::Path::new(tool)))
}

/// The app is installed in any installation (user or system).
fn is_installed(ctx: &Ctx, tool: &str, app: &str) -> Result<bool, InstallError> {
    Ok(flatpak(ctx, tool, &["info", app])?.success)
}

impl Installer for FlatpakInstaller {
    fn plan(&self, input: &InstallInput, _ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
        let app = need(&input.flatpak, &input.id, "source.flatpak")?;
        if state.packages.get(&input.id).is_some_and(|r| r.app.is_some()) {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!("add the {FLATPAK_REMOTE} remote to your user Flatpak installation if it is missing"),
            format!(
                "flatpak install --user {FLATPAK_REMOTE} {app} (needs the internet once, with its runtime; signed by Flathub)"
            ),
            format!("list it on the portal; start it with: flatpak run {app}"),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let app = need(&input.flatpak, &input.id, "source.flatpak")?.clone();
        let group = need(&input.group, &input.id, "group")?.clone();
        let tool = require("flatpak", &ctx.search_path)?;
        let tool = s(&tool);
        let mut notes = Vec::new();

        let mut by_kit = state
            .packages
            .get(&input.id)
            .and_then(|r| r.app.as_ref())
            .is_some_and(|a| a.by_kit);
        if !is_installed(ctx, tool, &app)? {
            let remotes = flatpak(ctx, tool, &["remotes", "--user", "--columns=name"])?;
            if !remotes.stdout.lines().any(|l| l.trim() == FLATPAK_REMOTE) {
                let out = flatpak(
                    ctx,
                    tool,
                    &[
                        "remote-add",
                        "--user",
                        "--if-not-exists",
                        FLATPAK_REMOTE,
                        REMOTE_URL,
                    ],
                )?;
                if !out.success {
                    return Err(cmd_failed(
                        &format!(
                            "flatpak remote-add {FLATPAK_REMOTE} (the first install needs the internet)"
                        ),
                        &out.stderr,
                    ));
                }
                notes.push(format!(
                    "added the {FLATPAK_REMOTE} remote to your user Flatpak installation \
                     (remove it with: flatpak remote-delete --user {FLATPAK_REMOTE})"
                ));
            }
            let out = flatpak(
                ctx,
                tool,
                &[
                    "install",
                    "--user",
                    "--noninteractive",
                    "--assumeyes",
                    FLATPAK_REMOTE,
                    &app,
                ],
            )?;
            if !out.success {
                return Err(cmd_failed(
                    &format!("flatpak install {app} (the first install needs the internet)"),
                    &out.stderr,
                ));
            }
            by_kit = true;
        } else if !by_kit {
            notes.push(format!(
                "{app} was already installed; kit leaves it in place on uninstall"
            ));
        }

        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Flatpak,
                version: input.version.clone(),
                license: input.license.clone(),
                installed_at,
                files: vec![],
                service: None,
                app: Some(AppRecord {
                    app_id: app.clone(),
                    group,
                    by_kit,
                }),
                map: None,
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        notes.push(format!(
            "{}: start it from the desktop menu or with: flatpak run {app}",
            input.id
        ));
        Ok(notes)
    }

    fn verify(&self, record: &Record, ctx: &Ctx, _deep: bool) -> Vec<Check> {
        let id = &record.id;
        let Some(a) = &record.app else {
            return vec![Check {
                name: format!("{id}: app record"),
                ok: false,
                detail: "no app in the state".into(),
            }];
        };
        let present = require("flatpak", &ctx.search_path)
            .ok()
            .and_then(|t| is_installed(ctx, s(&t), &a.app_id).ok())
            .unwrap_or(false);
        vec![Check {
            name: format!("{id}: Flatpak {} installed", a.app_id),
            ok: present,
            detail: if present {
                format!("flatpak run {}", a.app_id)
            } else {
                format!("reinstall with: kit install --select {id}")
            },
        }]
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
        let mut notes = Vec::new();
        let Some(a) = record.app else {
            return Ok(notes);
        };
        if !a.by_kit {
            notes.push(format!(
                "{}: left in place, it was installed before kit",
                a.app_id
            ));
            return Ok(notes);
        }
        let Ok(tool) = require("flatpak", &ctx.search_path) else {
            notes.push(format!(
                "flatpak not found: remove it with `flatpak uninstall --user {}`",
                a.app_id
            ));
            return Ok(notes);
        };
        let mut args = vec!["uninstall", "--user", "--noninteractive", "--assumeyes"];
        if purge {
            args.push("--delete-data");
        }
        args.push(&a.app_id);
        let out = flatpak(ctx, s(&tool), &args)?;
        if !out.success {
            return Err(cmd_failed(
                &format!("flatpak uninstall {}", a.app_id),
                &out.stderr,
            ));
        }
        if !purge {
            notes.push(format!(
                "{id}: saved games and settings kept (use --purge to delete them)"
            ));
        }
        notes.push(
            "runtimes stay for other apps; remove unused ones with: flatpak uninstall --user --unused".into(),
        );
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::testing::FakeRunner;
    use super::super::tools::fake_exe;
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    const APP: &str = "org.luanti.luanti";

    struct Env {
        _dir: tempfile::TempDir,
        paths: super::super::Paths,
        bin: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            fake_exe(&bin, "flatpak");
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
            id: "luanti".into(),
            name: "Luanti".into(),
            license: "LGPL-2.1-or-later".into(),
            version: None,
            source: PathBuf::new(),
            file_name: String::new(),
            size: 0,
            sha256: String::new(),
            verified: false,
            image: None,
            service: None,
            content: None,
            flatpak: Some(APP.into()),
            group: Some("sandbox".into()),
            map: None,
        }
    }

    /// `info` succeeds once the app is installed (or from the start with `present`); `remotes`
    /// lists `remote_names`.
    fn host(present: bool, remote_names: &'static str) -> FakeRunner {
        let installed = Rc::new(Cell::new(present));
        FakeRunner::new(move |_, args| {
            Ok(match args.first().copied() {
                Some("info") if installed.get() => CmdOutput::ok(),
                Some("info") => CmdOutput::failed("not installed"),
                Some("install") => {
                    installed.set(true);
                    CmdOutput::ok()
                }
                Some("uninstall") => {
                    installed.set(false);
                    CmdOutput::ok()
                }
                Some("remotes") => CmdOutput {
                    success: true,
                    stdout: remote_names.into(),
                    stderr: String::new(),
                },
                _ => CmdOutput::ok(),
            })
        })
    }

    #[test]
    fn install_adds_the_remote_installs_per_user_and_lists_the_game() {
        let e = Env::new();
        let (r, p) = (host(false, ""), FakeHttp::default());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let notes = FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        // Only the user installation counts: a system-wide flathub remote cannot serve `--user`.
        assert_eq!(r.count("remotes --user"), 1);
        assert_eq!(r.count("remote-add --user --if-not-exists flathub"), 1);
        assert_eq!(
            r.count(&format!(
                "install --user --noninteractive --assumeyes flathub {APP}"
            )),
            1
        );
        assert!(notes.iter().any(|n| n.contains("flatpak run org.luanti.luanti")));
        let rec = &g.packages["luanti"];
        assert_eq!(rec.kind, PackageType::Flatpak);
        assert_eq!(rec.app.as_ref().unwrap().group, "sandbox");
        assert!(rec.app.as_ref().unwrap().by_kit);
        let html = std::fs::read_to_string(e.paths.portal_dir().join("kit.html")).unwrap();
        assert!(html.contains("flatpak run org.luanti.luanti"));
    }

    #[test]
    fn reinstall_does_nothing_and_an_existing_remote_is_kept() {
        let e = Env::new();
        let (r, p) = (host(false, "flathub\n"), FakeHttp::default());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        assert_eq!(r.count("remote-add"), 0);
        FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        assert_eq!(r.count("install --user"), 1, "{:?}", r.calls());
        assert!(g.packages["luanti"].app.as_ref().unwrap().by_kit);
        let plan = FlatpakInstaller.plan(&input(), &ctx, &g).unwrap();
        assert!(plan[0].contains("already installed"));
    }

    #[test]
    fn an_app_that_was_there_before_is_never_removed() {
        let e = Env::new();
        let (r, p) = (host(true, "flathub\n"), FakeHttp::default());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let notes = FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        assert_eq!(r.count("install --user"), 0);
        assert!(notes.iter().any(|n| n.contains("already installed")));
        let notes = FlatpakInstaller.uninstall("luanti", &ctx, &mut g, true).unwrap();
        assert_eq!(r.count("uninstall"), 0);
        assert!(notes[0].contains("left in place"));
        assert!(g.packages.is_empty());
    }

    #[test]
    fn failed_install_is_explained_and_records_nothing() {
        let e = Env::new();
        let r = FakeRunner::new(|_, args| {
            Ok(match args.first().copied() {
                Some("install") => CmdOutput::failed("Unable to connect to dl.flathub.org"),
                Some("info") => CmdOutput::failed("not installed"),
                _ => CmdOutput {
                    success: true,
                    stdout: "flathub\n".into(),
                    stderr: String::new(),
                },
            })
        });
        let p = FakeHttp::default();
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = FlatpakInstaller
            .install(&input(), &ctx, &mut g)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("needs the internet") && err.contains("dl.flathub.org"),
            "{err}"
        );
        assert!(g.packages.is_empty());
    }

    #[test]
    fn missing_flatpak_is_an_actionable_error() {
        let e = Env::new();
        std::fs::remove_file(e.bin.join("flatpak")).unwrap();
        let (r, p) = (host(false, ""), FakeHttp::default());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap_err();
        assert!(matches!(err, InstallError::MissingTool { .. }));
    }

    #[test]
    fn verify_and_uninstall() {
        let e = Env::new();
        let (r, p) = (host(false, "flathub\n"), FakeHttp::default());
        let ctx = e.ctx(&r, &p);
        let mut g = StateGuard::open(&e.paths).unwrap();
        FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        let rec = g.packages["luanti"].clone();
        assert!(FlatpakInstaller.verify(&rec, &ctx, false).iter().all(|c| c.ok));
        let notes = FlatpakInstaller.uninstall("luanti", &ctx, &mut g, false).unwrap();
        assert_eq!(
            r.count("uninstall --user --noninteractive --assumeyes org.luanti.luanti"),
            1
        );
        assert!(notes.iter().any(|n| n.contains("--purge")));
        assert!(g.packages.is_empty() && !e.paths.portal_dir().exists());
        assert!(!FlatpakInstaller.verify(&rec, &ctx, false).iter().all(|c| c.ok));
        // Idempotent; --purge also deletes the saved data.
        FlatpakInstaller.uninstall("luanti", &ctx, &mut g, true).unwrap();
        FlatpakInstaller.install(&input(), &ctx, &mut g).unwrap();
        FlatpakInstaller.uninstall("luanti", &ctx, &mut g, true).unwrap();
        assert_eq!(r.count("--delete-data"), 1);
    }
}
