//! Brings library XML, systemd unit and portal page in line with the installed state.
//! Safe to run any number of times; also repairs the leftovers of an interrupted run.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::time::{Duration, Instant};

use super::service::{self, UNIT_NAME, UnitSpec};
use super::tools::{require, s};
use super::{Check, Ctx, InstallError, io_err, portal, state::State};

pub(super) fn cmd_failed(what: &str, stderr: &str) -> InstallError {
    InstallError::Command {
        command: what.to_string(),
        stderr: stderr.trim().to_string(),
    }
}

pub(super) fn write_if_changed(path: &Path, content: &str) -> Result<bool, InstallError> {
    if fs::read_to_string(path).ok().as_deref() == Some(content) {
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(io_err("cannot write", path))?;
    Ok(true)
}

pub(super) fn systemctl(
    ctx: &Ctx,
    tool: &str,
    args: &[&str],
) -> Result<super::runner::CmdOutput, InstallError> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    ctx.runner
        .run(tool, &full)
        .map_err(io_err("cannot run", Path::new(tool)))
}

/// Rebuilds `library.xml` from scratch so it never holds stale or duplicate entries.
/// Returns whether the file changed.
fn rebuild_library(ctx: &Ctx, kiwix_manage: &str, zims: &[String]) -> Result<bool, InstallError> {
    let xml = ctx.paths.library_xml();
    let tmp = xml.with_extension("xml.tmp");
    let _ = fs::remove_file(&tmp);
    if let Some(dir) = xml.parent() {
        fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
    }
    for zim in zims {
        let out = ctx
            .runner
            .run(kiwix_manage, &[s(&tmp), "add", zim])
            .map_err(io_err("cannot run", Path::new(kiwix_manage)))?;
        if !out.success {
            let _ = fs::remove_file(&tmp);
            return Err(cmd_failed(&format!("kiwix-manage add {zim}"), &out.stderr));
        }
    }
    let new = fs::read(&tmp)
        .map_err(|_| cmd_failed("kiwix-manage", "it reported success but wrote no library file"))?;
    if fs::read(&xml).ok().as_deref() == Some(new.as_slice()) {
        let _ = fs::remove_file(&tmp);
        return Ok(false);
    }
    fs::rename(&tmp, &xml).map_err(io_err("cannot write", &xml))?;
    Ok(true)
}

pub(super) fn ensure_port_free(port: u16) -> Result<(), InstallError> {
    TcpListener::bind(("127.0.0.1", port))
        .map(drop)
        .map_err(|_| InstallError::PortInUse(port))
}

fn supports_monitor(ctx: &Ctx, kiwix_serve: &str) -> bool {
    ctx.runner
        .run(kiwix_serve, &["--help"])
        .is_ok_and(|o| o.stdout.contains("monitorLibrary") || o.stderr.contains("monitorLibrary"))
}

fn wait_active(ctx: &Ctx, systemctl_tool: &str) -> Result<(), InstallError> {
    let deadline = Instant::now() + ctx.start_wait;
    loop {
        if systemctl(ctx, systemctl_tool, &["is-active", "--quiet", UNIT_NAME])?.success {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(InstallError::ServiceFailed);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The programs a ZIM install needs; checked before anything is downloaded or changed.
pub fn ensure_tools(ctx: &Ctx) -> Result<(), InstallError> {
    for tool in ["kiwix-manage", "kiwix-serve", "systemctl"] {
        require(tool, &ctx.search_path)?;
    }
    Ok(())
}

/// Makes everything derived from `state` true. Returns hints for the user.
pub fn reconcile(ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
    let mut notes = Vec::new();
    portal::write(&ctx.paths, state)?;

    let zims: Vec<String> = state
        .library_files()
        .iter()
        .map(|f| s(&ctx.paths.library_dir().join(&f.name)).to_string())
        .collect();
    let unit_file = ctx.paths.unit_file();

    if zims.is_empty() {
        teardown(ctx, &mut notes)?;
        return Ok(notes);
    }

    let kiwix_manage = require("kiwix-manage", &ctx.search_path)?;
    let kiwix_serve = require("kiwix-serve", &ctx.search_path)?;
    let sysctl = require("systemctl", &ctx.search_path)?;
    let (kiwix_manage, kiwix_serve, sysctl) = (s(&kiwix_manage), s(&kiwix_serve).to_string(), s(&sysctl));

    if let Ok(existing) = fs::read_to_string(&unit_file)
        && !service::is_managed(&existing)
    {
        return Err(InstallError::ForeignUnit(unit_file));
    }

    let lib_changed = rebuild_library(ctx, kiwix_manage, &zims)?;
    let monitor = supports_monitor(ctx, &kiwix_serve);
    let unit = service::render_unit(&UnitSpec {
        kiwix_serve: kiwix_serve.clone().into(),
        library_xml: ctx.paths.library_xml(),
        port: ctx.port,
        monitor,
    });
    let unit_changed = write_if_changed(&unit_file, &unit)?;
    if unit_changed {
        let out = systemctl(ctx, sysctl, &["daemon-reload"])?;
        if !out.success {
            return Err(cmd_failed("systemctl --user daemon-reload", &out.stderr));
        }
    }

    let active = systemctl(ctx, sysctl, &["is-active", "--quiet", UNIT_NAME])?.success;
    if !active && ctx.check_port {
        ensure_port_free(ctx.port)?;
    }
    let out = systemctl(ctx, sysctl, &["enable", "--now", UNIT_NAME])?;
    if !out.success {
        return Err(cmd_failed(
            &format!("systemctl --user enable --now {UNIT_NAME}"),
            &out.stderr,
        ));
    }
    if active && (unit_changed || (lib_changed && !monitor)) {
        let out = systemctl(ctx, sysctl, &["restart", UNIT_NAME])?;
        if !out.success {
            return Err(cmd_failed(
                &format!("systemctl --user restart {UNIT_NAME}"),
                &out.stderr,
            ));
        }
    }
    wait_active(ctx, sysctl)?;

    notes.extend(linger_note(ctx, "The content server"));
    Ok(notes)
}

/// Hint for user services that stop at logout and do not start at boot (`Linger=no`).
pub(super) fn linger_note(ctx: &Ctx, what: &str) -> Option<String> {
    let user = ctx.user.as_ref()?;
    let loginctl = super::tools::find_in_path("loginctl", &ctx.search_path)?;
    let out = ctx
        .runner
        .run(s(&loginctl), &["show-user", user, "--property=Linger"])
        .ok()?;
    out.stdout.contains("Linger=no").then(|| {
        format!(
            "{what} stops when you log out and does not start at boot. To keep it running: sudo loginctl enable-linger {user}"
        )
    })
}

/// Nothing installed: stop and remove our unit and the library file.
fn teardown(ctx: &Ctx, notes: &mut Vec<String>) -> Result<(), InstallError> {
    let unit_file = ctx.paths.unit_file();
    if let Ok(existing) = fs::read_to_string(&unit_file) {
        if !service::is_managed(&existing) {
            return Err(InstallError::ForeignUnit(unit_file));
        }
        match require("systemctl", &ctx.search_path) {
            Ok(sysctl) => {
                let sysctl = s(&sysctl);
                // Not loaded / already stopped is fine; the unit file is removed either way.
                let _ = systemctl(ctx, sysctl, &["disable", "--now", UNIT_NAME]);
                fs::remove_file(&unit_file).map_err(io_err("cannot remove", &unit_file))?;
                let out = systemctl(ctx, sysctl, &["daemon-reload"])?;
                if !out.success {
                    return Err(cmd_failed("systemctl --user daemon-reload", &out.stderr));
                }
            }
            Err(_) => {
                fs::remove_file(&unit_file).map_err(io_err("cannot remove", &unit_file))?;
                notes.push(format!(
                    "systemctl not found: removed {} but could not stop the service; run `systemctl --user disable --now {UNIT_NAME}`",
                    unit_file.display()
                ));
            }
        }
    }
    let xml = ctx.paths.library_xml();
    match fs::remove_file(&xml) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("cannot remove", &xml)(e)),
    }
}

/// Checks shared by all packages: library, unit, service and what the server really serves.
pub fn verify_service(ctx: &Ctx, state: &State) -> Vec<Check> {
    let mut checks = Vec::new();
    let zims = state.library_files().len();
    if zims == 0 {
        return checks;
    }
    let check = |name: &str, ok: bool, detail: String| Check {
        name: name.to_string(),
        ok,
        detail,
    };

    let xml = ctx.paths.library_xml();
    let xml_text = fs::read_to_string(&xml).unwrap_or_default();
    let missing: Vec<&str> = state
        .library_files()
        .iter()
        .map(|f| f.name.as_str())
        .filter(|n| !xml_text.contains(n))
        .collect();
    checks.push(check(
        "library.xml lists every package",
        missing.is_empty() && !xml_text.is_empty(),
        if missing.is_empty() {
            xml.display().to_string()
        } else {
            format!("missing: {}", missing.join(", "))
        },
    ));

    let unit_file = ctx.paths.unit_file();
    let unit_ok = fs::read_to_string(&unit_file).is_ok_and(|t| service::is_managed(&t));
    checks.push(check(
        "systemd unit present",
        unit_ok,
        unit_file.display().to_string(),
    ));

    let active = require("systemctl", &ctx.search_path)
        .ok()
        .and_then(|t| systemctl(ctx, s(&t), &["is-active", "--quiet", UNIT_NAME]).ok())
        .is_some_and(|o| o.success);
    checks.push(check(
        "service is running",
        active,
        if active {
            UNIT_NAME.into()
        } else {
            format!("not active; see: journalctl --user -u {UNIT_NAME}")
        },
    ));

    let url = format!("http://127.0.0.1:{}/catalog/v2/entries?count=1", ctx.port);
    // Right after (re)starting, the server may need a moment before it accepts connections.
    let mut answer = ctx.probe.get(&url);
    for _ in 0..3 {
        if answer.is_ok() || ctx.start_wait.is_zero() {
            break;
        }
        std::thread::sleep(Duration::from_millis(400));
        answer = ctx.probe.get(&url);
    }
    match answer {
        Ok(body) => {
            let served = body
                .split("<totalResults>")
                .nth(1)
                .and_then(|r| r.split('<').next())
                .and_then(|n| n.trim().parse::<usize>().ok());
            checks.push(check(
                "server serves every package",
                served == Some(zims),
                format!(
                    "{url} reports {} book(s), expected {zims}",
                    served.map_or("?".into(), |n| n.to_string())
                ),
            ));
        }
        Err(e) => checks.push(check("server answers", false, e.to_string())),
    }
    checks
}
