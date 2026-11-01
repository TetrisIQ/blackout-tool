//! Installer for `container` packages: one rootless Podman container per service, defined by a
//! Quadlet file (systemd turns it into `kit-<id>.service`), listening on 127.0.0.1 only. The
//! local HTTPS front (`kit net`) puts a host name in front of it. The image is pinned by digest,
//! which is how its content is verified.

use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::reconcile::{cmd_failed, systemctl, write_if_changed};
use super::state::{Record, ServiceRecord, State, StateGuard};
use super::tools::{require, s};
use super::{Check, Ctx, InstallError, InstallInput, Installer, Paths, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::{MountKind, ServiceSpec};

pub const MARKER: &str = "# Managed by kit (emergency-kit). Remove with: kit uninstall <package>";

pub fn unit_name(id: &str) -> String {
    format!("kit-{id}.service")
}

/// Login created for services that need one; stored with mode 0600 next to the service data.
#[derive(Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

pub fn read_credentials(paths: &Paths, id: &str) -> Option<Credentials> {
    serde_json::from_str(&fs::read_to_string(paths.credentials_file(id)).ok()?).ok()
}

fn quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

fn check_path(p: &Path) -> Result<&str, InstallError> {
    let t = s(p);
    if t.is_empty() || t.contains([':', '\n', '\r']) {
        return Err(InstallError::BadPath(format!(
            "{} (containers cannot mount paths with ':' or line breaks)",
            p.display()
        )));
    }
    Ok(t)
}

/// The Quadlet file for one service.
pub fn render_quadlet(
    id: &str,
    name: &str,
    image: &str,
    svc: &ServiceSpec,
    paths: &Paths,
) -> Result<String, InstallError> {
    let mut c = format!(
        "{MARKER}\n[Unit]\nDescription={name} (emergency-kit)\n\n[Container]\n\
         Image={image}\nContainerName=kit-{id}\n\
         PublishPort=127.0.0.1:{port}:{port}\n",
        port = svc.port
    );
    for (k, v) in &svc.env {
        c.push_str(&format!("Environment={}\n", quote(&format!("{k}={v}"))));
    }
    for m in &svc.mounts {
        let src = match m.kind {
            MountKind::Data => paths.service_data_dir(id),
            MountKind::Media => paths.media_dir(m.media.as_deref().unwrap_or("")),
        };
        c.push_str(&format!(
            "Volume={}:{}{}\n",
            check_path(&src)?,
            m.target,
            if m.read_only { ":ro" } else { "" }
        ));
    }
    c.push_str("\n[Service]\nRestart=on-failure\nRestartSec=3\nTimeoutStartSec=300\n\n[Install]\nWantedBy=default.target\n");
    Ok(c)
}

fn need<'a, T>(v: &'a Option<T>, id: &str, what: &str) -> Result<&'a T, InstallError> {
    v.as_ref()
        .ok_or_else(|| InstallError::Corrupt(format!("{id}: the manifest has no {what}")))
}

fn url(svc: &ServiceSpec, path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", svc.port)
}

fn wait_healthy(ctx: &Ctx, svc: &ServiceSpec) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if ctx.probe.get(&url(svc, &svc.health_path)).is_ok() {
            return true;
        }
        if waited >= ctx.start_wait {
            return false;
        }
        let step = Duration::from_millis(500);
        thread::sleep(step);
        waited += step;
    }
}

fn random_hex(bytes: usize) -> Result<String, InstallError> {
    let mut buf = vec![0u8; bytes];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(io_err("cannot read", Path::new("/dev/urandom")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_private(path: &Path, content: &str) -> Result<(), InstallError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
    }
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().map_err(io_err("cannot write", path))
}

/// First-run setup, so nobody has to find a wizard while offline. Returns a hint for the user.
fn provision(kind: &str, id: &str, ctx: &Ctx, svc: &ServiceSpec) -> Result<Option<String>, InstallError> {
    match kind {
        "navidrome" => {
            if ctx.paths.credentials_file(id).exists() {
                return Ok(None);
            }
            let creds = Credentials {
                username: "admin".into(),
                password: random_hex(12)?,
            };
            let body =
                serde_json::json!({"username": creds.username, "password": creds.password}).to_string();
            match ctx.probe.post_json(&url(svc, "/auth/createAdmin"), &body) {
                Ok(_) => {
                    write_private(
                        &ctx.paths.credentials_file(id),
                        &(serde_json::to_string_pretty(&creds).unwrap_or_default() + "\n"),
                    )?;
                    Ok(Some(format!(
                        "{id}: created the login '{}'; `kit status` shows the password",
                        creds.username
                    )))
                }
                // Navidrome refuses the call once a user exists: someone set it up already.
                Err(e) => Ok(Some(format!(
                    "{id}: could not create the first login automatically ({e}); open https://{} and create it there",
                    svc.host
                ))),
            }
        }
        other => Err(InstallError::Unsupported(format!("unknown provisioner {other}"))),
    }
}

fn up_to_date(input: &InstallInput, ctx: &Ctx, state: &State) -> bool {
    state
        .packages
        .get(&input.id)
        .is_some_and(|r| r.service.as_ref().map(|s| s.image.as_str()) == input.image.as_deref())
        && ctx.paths.quadlet_file(&input.id).exists()
}

pub struct ContainerInstaller;

impl Installer for ContainerInstaller {
    fn plan(&self, input: &InstallInput, ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
        let svc = need(&input.service, &input.id, "service block")?;
        let image = need(&input.image, &input.id, "image")?;
        if up_to_date(input, ctx, state) {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!("pull {image} (needs the internet once; verified by its digest)"),
            format!(
                "write {} and start {}",
                ctx.paths.quadlet_file(&input.id).display(),
                unit_name(&input.id)
            ),
            format!(
                "listen on 127.0.0.1:{} and wait for {}",
                svc.port, svc.health_path
            ),
            format!("front it as https://{} (kit net enable)", svc.host),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let svc = need(&input.service, &input.id, "service block")?.clone();
        let image = need(&input.image, &input.id, "image")?.clone();
        let podman = require("podman", &ctx.search_path)?;
        let podman = s(&podman);
        let sysctl = require("systemctl", &ctx.search_path)?;
        let sysctl = s(&sysctl);
        let quadlet = ctx.paths.quadlet_file(&input.id);
        if let Ok(existing) = fs::read_to_string(&quadlet)
            && existing.lines().next() != Some(MARKER)
        {
            return Err(InstallError::ForeignUnit(quadlet));
        }

        let run = |args: &[&str]| {
            ctx.runner
                .run(podman, args)
                .map_err(io_err("cannot run", Path::new(podman)))
        };
        if !run(&["image", "exists", &image])?.success {
            let out = run(&["pull", &image])?;
            if !out.success {
                return Err(cmd_failed(
                    &format!("podman pull {image} (the first install needs the internet)"),
                    &out.stderr,
                ));
            }
        }

        for m in &svc.mounts {
            let dir = match m.kind {
                MountKind::Data => ctx.paths.service_data_dir(&input.id),
                MountKind::Media => ctx.paths.media_dir(m.media.as_deref().unwrap_or("")),
            };
            fs::create_dir_all(&dir).map_err(io_err("cannot create", &dir))?;
        }
        let text = render_quadlet(&input.id, &input.name, &image, &svc, &ctx.paths)?;

        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Container,
                version: input.version.clone(),
                license: input.license.clone(),
                installed_at,
                files: vec![],
                service: Some(ServiceRecord {
                    host: svc.host.clone(),
                    port: svc.port,
                    unit: unit_name(&input.id),
                    image: image.clone(),
                }),
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;

        let changed = write_if_changed(&quadlet, &text)?;
        if changed {
            let out = systemctl(ctx, sysctl, &["daemon-reload"])?;
            if !out.success {
                return Err(cmd_failed("systemctl --user daemon-reload", &out.stderr));
            }
        }
        let unit = unit_name(&input.id);
        let active = systemctl(ctx, sysctl, &["is-active", "--quiet", &unit])?.success;
        if !active && ctx.check_port {
            super::reconcile::ensure_port_free(svc.port)?;
        }
        let verb = if !active {
            "start"
        } else if changed {
            "restart"
        } else {
            ""
        };
        if !verb.is_empty() {
            let out = systemctl(ctx, sysctl, &[verb, &unit])?;
            if !out.success {
                return Err(cmd_failed(
                    &format!("systemctl --user {verb} {unit}"),
                    &format!("{} (see: journalctl --user -u {unit})", out.stderr.trim()),
                ));
            }
        }
        if !wait_healthy(ctx, &svc) {
            return Err(InstallError::Command {
                command: format!("wait for {}", url(&svc, &svc.health_path)),
                stderr: format!("{unit} did not become healthy; see: journalctl --user -u {unit}"),
            });
        }
        let mut notes = Vec::new();
        if let Some(kind) = &svc.provision {
            notes.extend(provision(kind, &input.id, ctx, &svc)?);
        }
        notes.push(format!(
            "{}: https://{} (after `kit net enable`), directly http://127.0.0.1:{}/",
            input.id, svc.host, svc.port
        ));
        Ok(notes)
    }

    fn verify(&self, record: &Record, ctx: &Ctx, _deep: bool) -> Vec<Check> {
        let id = &record.id;
        let Some(sv) = &record.service else {
            return vec![Check {
                name: format!("{id}: service record"),
                ok: false,
                detail: "no service in the state".into(),
            }];
        };
        let quadlet = ctx.paths.quadlet_file(id);
        let managed = fs::read_to_string(&quadlet).is_ok_and(|t| t.lines().next() == Some(MARKER));
        let active = require("systemctl", &ctx.search_path)
            .ok()
            .and_then(|t| systemctl(ctx, s(&t), &["is-active", "--quiet", &sv.unit]).ok())
            .is_some_and(|o| o.success);
        let healthy = ctx.probe.get(&format!("http://127.0.0.1:{}/", sv.port)).is_ok();
        vec![
            Check {
                name: format!("{id}: Quadlet file"),
                ok: managed,
                detail: quadlet.display().to_string(),
            },
            Check {
                name: format!("{id}: service {} active", sv.unit),
                ok: active,
                detail: if active {
                    "running".into()
                } else {
                    format!("start with: systemctl --user start {}", sv.unit)
                },
            },
            Check {
                name: format!("{id}: answers on 127.0.0.1:{}", sv.port),
                ok: healthy,
                detail: format!("http://127.0.0.1:{}/", sv.port),
            },
        ]
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
        let unit = record
            .service
            .as_ref()
            .map_or_else(|| unit_name(id), |s| s.unit.clone());
        let quadlet = ctx.paths.quadlet_file(id);
        if let Ok(existing) = fs::read_to_string(&quadlet) {
            if existing.lines().next() == Some(MARKER) {
                match require("systemctl", &ctx.search_path) {
                    Ok(t) => {
                        let _ = systemctl(ctx, s(&t), &["stop", &unit]);
                        fs::remove_file(&quadlet).map_err(io_err("cannot remove", &quadlet))?;
                        let out = systemctl(ctx, s(&t), &["daemon-reload"])?;
                        if !out.success {
                            return Err(cmd_failed("systemctl --user daemon-reload", &out.stderr));
                        }
                    }
                    Err(_) => {
                        fs::remove_file(&quadlet).map_err(io_err("cannot remove", &quadlet))?;
                        notes.push(format!(
                            "systemctl not found: stop it with `systemctl --user stop {unit}`"
                        ));
                    }
                }
            } else {
                notes.push(format!(
                    "left {} alone: it was not written by kit",
                    quadlet.display()
                ));
            }
        }
        if purge {
            let dir = ctx.paths.services_dir().join(id);
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_err("cannot remove", &dir)(e)),
            }
            if let (Some(sv), Ok(podman)) = (&record.service, require("podman", &ctx.search_path)) {
                let _ = ctx.runner.run(s(&podman), &["rmi", &sv.image]);
            }
        } else {
            notes.push(format!(
                "{id}: data kept in {} (use --purge to delete it, including the login)",
                ctx.paths.services_dir().join(id).display()
            ));
        }
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::CmdOutput;
    use super::super::runner::testing::FakeRunner;
    use super::super::tools::fake_exe;
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use crate::catalog::manifest::Mount;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;

    const IMAGE: &str = "docker.io/deluan/navidrome:0.64.2@sha256:38dc2727bfcfd5ede290f8ada114fc90368146f265ae4701ddddbcbe2a44ee52";

    fn spec() -> ServiceSpec {
        ServiceSpec {
            host: "navidrome.kit.local".into(),
            port: 4533,
            env: [("ND_MUSICFOLDER".to_string(), "/music".to_string())].into(),
            mounts: vec![
                Mount {
                    kind: MountKind::Data,
                    media: None,
                    target: "/data".into(),
                    read_only: false,
                },
                Mount {
                    kind: MountKind::Media,
                    media: Some("music".into()),
                    target: "/music".into(),
                    read_only: true,
                },
            ],
            health_path: "/ping".into(),
            provision: Some("navidrome".into()),
        }
    }

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
            for t in ["podman", "systemctl"] {
                fake_exe(&bin, t);
            }
            let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
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

        fn input(&self) -> InstallInput {
            InstallInput {
                id: "navidrome".into(),
                name: "Navidrome".into(),
                license: "GPL-3.0-only".into(),
                version: None,
                source: PathBuf::new(),
                file_name: String::new(),
                size: 0,
                sha256: String::new(),
                verified: false,
                image: Some(IMAGE.into()),
                service: Some(spec()),
                content: None,
            }
        }
    }

    /// podman: image known after `pull`; systemctl: unit active after `start`.
    fn host() -> FakeRunner {
        let pulled = Rc::new(Cell::new(false));
        let active = Rc::new(Cell::new(false));
        FakeRunner::new(move |prog, args| {
            Ok(match (prog, args.first().copied()) {
                ("podman", Some("image")) => {
                    if pulled.get() {
                        CmdOutput::ok()
                    } else {
                        CmdOutput::failed("no such image")
                    }
                }
                ("podman", Some("pull")) => {
                    pulled.set(true);
                    CmdOutput::ok()
                }
                ("systemctl", _) if args.contains(&"start") => {
                    active.set(true);
                    CmdOutput::ok()
                }
                ("systemctl", _) if args.contains(&"stop") => {
                    active.set(false);
                    CmdOutput::ok()
                }
                ("systemctl", _) if args.contains(&"is-active") => {
                    if active.get() {
                        CmdOutput::ok()
                    } else {
                        CmdOutput::failed("")
                    }
                }
                _ => CmdOutput::ok(),
            })
        })
    }

    fn probe() -> FakeHttp {
        FakeHttp::default()
            .with("http://127.0.0.1:4533/ping", "ok")
            .with("http://127.0.0.1:4533/", "ui")
    }

    #[test]
    fn quadlet_file_is_pinned_loopback_only_and_quotes_env() {
        let e = Env::new();
        let q = render_quadlet("navidrome", "Navidrome", IMAGE, &spec(), &e.paths).unwrap();
        assert!(q.starts_with(MARKER));
        assert!(q.contains(&format!("Image={IMAGE}")));
        assert!(q.contains("PublishPort=127.0.0.1:4533:4533"));
        assert!(q.contains("Environment=\"ND_MUSICFOLDER=/music\""));
        assert!(q.contains(&format!(
            "Volume={}:/data\n",
            e.paths.service_data_dir("navidrome").display()
        )));
        assert!(q.contains(&format!(
            "Volume={}:/music:ro\n",
            e.paths.media_dir("music").display()
        )));
        assert!(q.contains("WantedBy=default.target"));
    }

    #[test]
    fn paths_with_colons_are_refused() {
        let e = Env::new();
        let paths = Paths::new(PathBuf::from("/tmp/a:b"), e.paths.unit_dir.clone()).unwrap();
        assert!(matches!(
            render_quadlet("n", "N", IMAGE, &spec(), &paths),
            Err(InstallError::BadPath(_))
        ));
    }

    #[test]
    fn install_pulls_writes_starts_waits_and_provisions() {
        let e = Env::new();
        let (runner, probe) = (host(), probe());
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        let notes = ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();

        assert_eq!(runner.count(&format!("podman pull {IMAGE}")), 1);
        assert!(e.paths.quadlet_file("navidrome").exists());
        assert!(e.paths.service_data_dir("navidrome").is_dir() && e.paths.media_dir("music").is_dir());
        assert_eq!(runner.count("systemctl --user start kit-navidrome.service"), 1);
        let rec = &guard.packages["navidrome"];
        assert_eq!(rec.service.as_ref().unwrap().host, "navidrome.kit.local");

        let posted = probe.posted.lock().unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].0, "http://127.0.0.1:4533/auth/createAdmin");
        let creds = read_credentials(&e.paths, "navidrome").expect("credentials stored");
        assert_eq!(creds.username, "admin");
        assert_eq!(creds.password.len(), 24);
        assert!(posted[0].1.contains(&creds.password));
        let mode = fs::metadata(e.paths.credentials_file("navidrome"))
            .unwrap()
            .permissions();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777, 0o600);
        assert!(notes.iter().any(|n| n.contains("created the login")));
    }

    #[test]
    fn reinstall_is_a_no_op_that_keeps_the_login() {
        let e = Env::new();
        let (runner, probe) = (host(), probe());
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        let first = read_credentials(&e.paths, "navidrome").unwrap().password;
        ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        assert_eq!(runner.count("podman pull"), 1);
        assert_eq!(runner.count("systemctl --user start"), 1);
        assert_eq!(
            runner.count("restart"),
            0,
            "unchanged file, running service: nothing to do"
        );
        assert_eq!(probe.posted.lock().unwrap().len(), 1);
        assert_eq!(read_credentials(&e.paths, "navidrome").unwrap().password, first);
        assert!(up_to_date(&e.input(), &ctx, &guard));
    }

    #[test]
    fn changed_quadlet_restarts_a_running_service() {
        let e = Env::new();
        let (runner, probe) = (host(), probe());
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        let mut input = e.input();
        input
            .service
            .as_mut()
            .unwrap()
            .env
            .insert("ND_LOGLEVEL".into(), "debug".into());
        ContainerInstaller.install(&input, &ctx, &mut guard).unwrap();
        assert_eq!(runner.count("systemctl --user restart kit-navidrome.service"), 1);
    }

    #[test]
    fn pull_failure_is_explained_and_changes_nothing() {
        let e = Env::new();
        let runner = FakeRunner::new(|_, args| {
            Ok(
                if args.first() == Some(&"pull") || args.first() == Some(&"image") {
                    CmdOutput::failed("dial tcp: no route to host")
                } else {
                    CmdOutput::ok()
                },
            )
        });
        let probe = probe();
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        let err = ContainerInstaller
            .install(&e.input(), &ctx, &mut guard)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("needs the internet") && err.contains("no route to host"),
            "{err}"
        );
        assert!(guard.packages.is_empty() && !e.paths.quadlet_file("navidrome").exists());
    }

    #[test]
    fn unhealthy_service_is_an_error_and_failed_provisioning_a_hint() {
        let e = Env::new();
        let runner = host();
        let down = FakeHttp::default();
        let ctx = e.ctx(&runner, &down);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        let err = ContainerInstaller
            .install(&e.input(), &ctx, &mut guard)
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not become healthy"), "{err}");

        let mut busy = probe();
        busy.post_fails = true;
        let ctx = e.ctx(&runner, &busy);
        let notes = ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("could not create the first login")),
            "{notes:?}"
        );
        assert!(
            read_credentials(&e.paths, "navidrome").is_none(),
            "no credentials for a login that does not exist"
        );
    }

    #[test]
    fn foreign_quadlet_is_never_overwritten() {
        let e = Env::new();
        fs::create_dir_all(&e.paths.quadlet_dir).unwrap();
        fs::write(e.paths.quadlet_file("navidrome"), "[Container]\nImage=mine\n").unwrap();
        let (runner, probe) = (host(), probe());
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        assert!(matches!(
            ContainerInstaller.install(&e.input(), &ctx, &mut guard),
            Err(InstallError::ForeignUnit(_))
        ));
        assert_eq!(runner.count("podman pull"), 0);
    }

    #[test]
    fn verify_and_uninstall() {
        let e = Env::new();
        let (runner, probe) = (host(), probe());
        let ctx = e.ctx(&runner, &probe);
        let mut guard = StateGuard::open(&e.paths).unwrap();
        ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        let rec = guard.packages["navidrome"].clone();
        let checks = ContainerInstaller.verify(&rec, &ctx, false);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");

        let notes = ContainerInstaller
            .uninstall("navidrome", &ctx, &mut guard, false)
            .unwrap();
        assert!(guard.packages.is_empty());
        assert!(!e.paths.quadlet_file("navidrome").exists());
        assert_eq!(runner.count("systemctl --user stop kit-navidrome.service"), 1);
        assert!(
            e.paths.credentials_file("navidrome").exists(),
            "data and login are kept without --purge"
        );
        assert!(notes.iter().any(|n| n.contains("--purge")));
        assert!(
            ContainerInstaller
                .uninstall("navidrome", &ctx, &mut guard, true)
                .unwrap()[0]
                .contains("not installed")
        );

        ContainerInstaller.install(&e.input(), &ctx, &mut guard).unwrap();
        ContainerInstaller
            .uninstall("navidrome", &ctx, &mut guard, true)
            .unwrap();
        assert!(!e.paths.services_dir().join("navidrome").exists());
        assert_eq!(runner.count(&format!("podman rmi {IMAGE}")), 1);
    }
}
