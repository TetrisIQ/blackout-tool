//! `kit net enable | disable | status`: reconciles the CA, Caddy, hosts, the 443 redirect and the
//! trust stores with the installed packages. Every step is idempotent; the list of what was done
//! is kept in `<data-dir>/net/state.json` so `disable` undoes exactly that.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::ca::Ca;
use super::caddy::{self, CaddySpec, Mirror};
use super::trust::{self, SystemStore};
use super::{NetError, NetPaths, hosts, io_err, nft};
use crate::install::Check;
use crate::install::portal::PORTAL_HOST;
use crate::install::runner::CommandRunner;
use crate::install::tools::{find_in_path, s};

pub struct NetCtx<'a> {
    pub data_dir: PathBuf,
    /// systemd user units (`~/.config/systemd/user`).
    pub unit_dir: PathBuf,
    pub home: PathBuf,
    pub hosts_path: PathBuf,
    /// Runs commands as the current user.
    pub runner: &'a dyn CommandRunner,
    /// Runs commands as root (through `sudo` unless kit already runs as root).
    pub root: &'a dyn CommandRunner,
    /// `sudo` is needed (and must exist) because kit does not run as root.
    pub needs_sudo: bool,
    pub search_path: Vec<PathBuf>,
    pub now: u64,
    pub listen_port: u16,
    pub kiwix_port: u16,
    /// How long to wait for Caddy to become active.
    pub start_wait: Duration,
}

impl NetCtx<'_> {
    fn paths(&self) -> NetPaths {
        NetPaths::new(&self.data_dir)
    }
    fn unit_file(&self) -> PathBuf {
        self.unit_dir.join(caddy::UNIT_NAME)
    }
    fn has(&self, tool: &str) -> bool {
        find_in_path(tool, &self.search_path).is_some()
    }
    fn require(&self, tool: &str, hint: &str) -> Result<String, NetError> {
        find_in_path(tool, &self.search_path)
            .map(|p| s(&p).to_string())
            .ok_or_else(|| NetError::MissingTool {
                tool: tool.into(),
                hint: hint.into(),
            })
    }
}

/// What the installed packages ask for.
#[derive(Debug, Clone, Default)]
pub struct Desired {
    pub mirrors: Vec<Mirror>,
    /// Installed container services.
    pub services: Vec<caddy::Service>,
    /// Port of the local Ollama server when a language model is installed.
    pub ollama_port: Option<u16>,
    /// Every host any catalog package may mirror. Becomes the CA's name constraint, so installing
    /// another package later does not force a new CA.
    pub permitted: Vec<String>,
}

impl Desired {
    pub fn hosts(&self) -> Vec<String> {
        let mut h: Vec<String> = self.mirrors.iter().map(|m| m.host.clone()).collect();
        h.extend(self.services.iter().map(|s| s.host.clone()));
        h.sort();
        h.dedup();
        h
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    /// Create a new CA even though one exists (needed when the permitted hosts grew).
    pub renew_ca: bool,
    /// `disable`: also delete the CA and its key.
    pub remove_ca: bool,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub what: String,
    pub root: bool,
    /// Why root is needed, shown to the user.
    pub why: Option<String>,
    /// Filled in when executed: a warning instead of a failure.
    pub note: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetState {
    pub hosts: Vec<String>,
    pub listen_port: u16,
    pub system_store: Option<String>,
    pub nss_dbs: Vec<PathBuf>,
    pub enabled_at: u64,
}

fn state_file(ctx: &NetCtx) -> PathBuf {
    ctx.paths().dir.join("state.json")
}

pub fn load_state(ctx: &NetCtx) -> Result<Option<NetState>, NetError> {
    let path = state_file(ctx);
    match fs::read_to_string(&path) {
        Ok(t) => serde_json::from_str(&t)
            .map(Some)
            .map_err(|e| NetError::Invalid(format!("{} is corrupt: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("cannot read", &path)(e)),
    }
}

fn save_state(ctx: &NetCtx, st: &NetState) -> Result<(), NetError> {
    let path = state_file(ctx);
    write_if_changed(
        &path,
        &(serde_json::to_string_pretty(st).unwrap_or_default() + "\n"),
    )?;
    Ok(())
}

fn write_if_changed(path: &Path, content: &str) -> Result<bool, NetError> {
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

fn run(
    r: &dyn CommandRunner,
    prog: &str,
    args: &[&str],
) -> Result<crate::install::runner::CmdOutput, NetError> {
    r.run(prog, args).map_err(|e| NetError::Command {
        command: format!("{prog} {}", args.join(" ")),
        stderr: e.to_string(),
    })
}

fn run_ok(r: &dyn CommandRunner, prog: &str, args: &[&str]) -> Result<(), NetError> {
    let out = run(r, prog, args)?;
    if out.success {
        Ok(())
    } else {
        Err(NetError::Command {
            command: format!("{prog} {}", args.join(" ")),
            stderr: out.stderr.trim().to_string(),
        })
    }
}

struct Steps<'o> {
    opts: &'o Options,
    list: Vec<Step>,
}

impl Steps<'_> {
    /// Records the step and, unless this is a dry run, executes it.
    fn go(
        &mut self,
        what: &str,
        root: Option<&str>,
        f: impl FnOnce() -> Result<Option<String>, NetError>,
    ) -> Result<(), NetError> {
        let mut step = Step {
            what: what.into(),
            root: root.is_some(),
            why: root.map(str::to_string),
            note: None,
        };
        if !self.opts.dry_run {
            step.note = f()?;
        }
        self.list.push(step);
        Ok(())
    }
}

const NO_NOTE: Result<Option<String>, NetError> = Ok(None);

fn systemctl(ctx: &NetCtx, tool: &str, args: &[&str]) -> Result<crate::install::runner::CmdOutput, NetError> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    run(ctx.runner, tool, &full)
}

fn wait_active(ctx: &NetCtx, systemctl_tool: &str) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if systemctl(ctx, systemctl_tool, &["is-active", "--quiet", caddy::UNIT_NAME])
            .is_ok_and(|o| o.success)
        {
            return true;
        }
        if waited >= ctx.start_wait {
            return false;
        }
        let step = Duration::from_millis(200);
        thread::sleep(step);
        waited += step;
    }
}

fn caddy_hint() -> String {
    let hint =
        crate::install::tools::install_hint(&fs::read_to_string("/etc/os-release").unwrap_or_default());
    hint.replace("kiwix-tools", "caddy")
}

/// Brings the network layer in line with `desired`. With `dry_run` nothing is changed.
pub fn enable(ctx: &NetCtx, desired: &Desired, opts: &Options) -> Result<Vec<Step>, NetError> {
    let mut hosts_list = desired.hosts();
    let have_mirrors = !hosts_list.is_empty();
    hosts_list.push(PORTAL_HOST.to_string());
    hosts_list.sort();
    if !have_mirrors {
        return Err(NetError::Invalid(
            "no installed package mirrors a domain or runs a service; install one first, e.g. `kit install --select wikipedia_de_nopic`".into(),
        ));
    }
    let caddy_bin = ctx.require("caddy", &format!("Install it with: {}", caddy_hint()))?;
    let nft_bin = ctx.require("nft", "Install the nftables package of your distribution.")?;
    let systemctl_bin = ctx.require("systemctl", "kit runs Caddy as a systemd user service.")?;
    if ctx.needs_sudo {
        ctx.require(
            "sudo",
            "Root is needed for the steps listed by `kit net enable --dry-run`.",
        )?;
    }
    let store = SystemStore::detect(|t| ctx.has(t)).ok_or_else(|| NetError::MissingTool {
        tool: "trust / update-ca-certificates".into(),
        hint: "Install p11-kit (`trust`) or ca-certificates.".into(),
    })?;
    let unit_path = ctx.unit_file();
    if let Ok(text) = fs::read_to_string(&unit_path)
        && !caddy::is_managed(&text)
    {
        return Err(NetError::Foreign(unit_path));
    }

    let paths = ctx.paths();
    let ca_dir = paths.ca_dir();
    let mut steps = Steps { opts, list: vec![] };
    let mut ca: Option<Ca> = Ca::load(&ca_dir)?;
    if let Some(c) = &ca
        && !opts.renew_ca
    {
        let missing = c.uncovered(&hosts_list);
        if !missing.is_empty() {
            return Err(NetError::Invalid(format!(
                "the local CA cannot issue for {}; its name constraint is fixed when it is created. \
                 Recreate it with `kit net enable --renew-ca` (the new CA is trusted again automatically)",
                missing.join(", ")
            )));
        }
    }
    let fresh_ca = ca.is_none() || opts.renew_ca;

    if opts.renew_ca && ca.is_some() {
        let old_pem = super::ca::CaFiles::new(ca_dir.clone()).cert();
        steps.go(
            "remove the previous local CA from the system certificate store",
            Some("the system certificate store belongs to root"),
            || {
                let _ = trust::remove_system(store, ctx.root, &old_pem);
                NO_NOTE
            },
        )?;
    }

    // 1. CA and server certificate (own files only).
    let mut permitted = desired.permitted.clone();
    permitted.extend(hosts_list.iter().cloned());
    permitted.push(PORTAL_HOST.to_string());
    steps.go(
        &format!(
            "{} the local CA in {} (name-constrained to {} mirrored hosts)",
            if fresh_ca { "create" } else { "keep" },
            ca_dir.display(),
            permitted.iter().collect::<std::collections::BTreeSet<_>>().len()
        ),
        None,
        || {
            if fresh_ca {
                ca = Some(Ca::create(&ca_dir, &permitted, ctx.now)?);
            }
            NO_NOTE
        },
    )?;
    steps.go(
        "issue the server certificate for the mirrored hosts",
        None,
        || {
            if let Some(c) = &ca {
                c.ensure_leaf(&hosts_list, ctx.now)?;
            }
            NO_NOTE
        },
    )?;

    // 2. Caddy.
    let caddyfile = paths.dir.join("Caddyfile");
    let mirrors = desired.mirrors.clone();
    let services = desired.services.clone();
    let mut caddy_changed = false;
    steps.go(
        &format!(
            "write {} and the user service {}",
            caddyfile.display(),
            caddy::UNIT_NAME
        ),
        None,
        || {
            let files = super::ca::CaFiles::new(ca_dir.clone());
            let text = caddy::render_caddyfile(&CaddySpec {
                mirrors: &mirrors,
                services: &services,
                cert: files.leaf_cert(),
                key: files.leaf_key(),
                listen_port: ctx.listen_port,
                http_port: caddy::http_port_for(ctx.listen_port),
                kiwix_port: ctx.kiwix_port,
                portal_dir: ctx.data_dir.join("portal"),
                portal_host: PORTAL_HOST.to_string(),
                ollama_port: desired.ollama_port,
            });
            caddy_changed |= write_if_changed(&caddyfile, &text)?;
            // Never start Caddy with a config it rejects.
            run_ok(
                ctx.runner,
                &caddy_bin,
                &["validate", "--config", s(&caddyfile), "--adapter", "caddyfile"],
            )?;
            let unit = caddy::render_unit(&caddy::UnitSpec {
                caddy: PathBuf::from(&caddy_bin),
                caddyfile: caddyfile.clone(),
            });
            if write_if_changed(&unit_path, &unit)? {
                caddy_changed = true;
                run_ok(ctx.runner, &systemctl_bin, &["--user", "daemon-reload"])?;
            }
            NO_NOTE
        },
    )?;
    steps.go(
        &format!("start {} on 127.0.0.1:{}", caddy::UNIT_NAME, ctx.listen_port),
        None,
        || {
            let active = systemctl(ctx, &systemctl_bin, &["is-active", "--quiet", caddy::UNIT_NAME])?.success;
            if active && caddy_changed {
                run_ok(
                    ctx.runner,
                    &systemctl_bin,
                    &["--user", "restart", caddy::UNIT_NAME],
                )?;
            } else if !active {
                run_ok(
                    ctx.runner,
                    &systemctl_bin,
                    &["--user", "enable", "--now", caddy::UNIT_NAME],
                )?;
            }
            if wait_active(ctx, &systemctl_bin) {
                NO_NOTE
            } else {
                Err(NetError::Command {
                    command: format!("systemctl --user enable --now {}", caddy::UNIT_NAME),
                    stderr: format!(
                        "the service did not start; see: journalctl --user -u {}",
                        caddy::UNIT_NAME
                    ),
                })
            }
        },
    )?;

    // Remember what is about to be changed, so `disable` can undo it even after a crash.
    let nss_dbs = trust::find_nss_dbs(&ctx.home);
    let mut state = load_state(ctx)?.unwrap_or_default();
    state.hosts = hosts_list.clone();
    state.listen_port = ctx.listen_port;
    state.system_store = Some(store.as_str().into());
    state.nss_dbs = nss_dbs.clone();
    state.enabled_at = ctx.now;
    steps.go("record the changes in the net state file", None, || {
        save_state(ctx, &state)?;
        NO_NOTE
    })?;

    // 3. Trust the CA.
    let ca_pem = super::ca::CaFiles::new(ca_dir.clone()).cert();
    let trust_now = fresh_ca || !system_trusted(ctx, store);
    if trust_now {
        steps.go(
            "trust the local CA in the system certificate store",
            Some("the system certificate store belongs to root"),
            || {
                trust::add_system(store, ctx.root, &ca_pem)?;
                NO_NOTE
            },
        )?;
    }
    for db in &nss_dbs {
        if fresh_ca || !trust::has_nss(ctx.runner, db) {
            if !ctx.has("certutil") {
                continue;
            }
            steps.go(&format!("trust the local CA in {}", db.display()), None, || {
                trust::add_nss(ctx.runner, db, &ca_pem)?;
                NO_NOTE
            })?;
        }
    }
    if !ctx.has("certutil") {
        steps.list.push(Step {
            what: "skip Firefox/Chromium: `certutil` (nss-tools) is not installed".into(),
            root: false,
            why: None,
            note: Some("install certutil and run `kit net enable` again".into()),
        });
    }

    // 4. Names and port.
    let hosts_text = fs::read_to_string(&ctx.hosts_path).unwrap_or_default();
    let new_hosts = hosts::apply(&hosts_text, &hosts_list);
    if new_hosts != hosts_text {
        steps.go(
            &format!(
                "add the marked block to {} (127.77.0.1 for each mirrored host and {PORTAL_HOST})",
                ctx.hosts_path.display()
            ),
            Some("/etc/hosts belongs to root"),
            || {
                let staged = paths.dir.join("hosts.new");
                write_if_changed(&staged, &new_hosts)?;
                run_ok(ctx.root, "cp", &[s(&staged), s(&ctx.hosts_path)])?;
                let _ = fs::remove_file(&staged);
                NO_NOTE
            },
        )?;
    }
    steps.go(
        &format!(
            "redirect {}:{} and :{} to :{} / :{} (nftables table `{} {}`)",
            hosts::MIRROR_ADDR,
            nft::HTTPS_PORT,
            nft::HTTP_PORT,
            ctx.listen_port,
            caddy::http_port_for(ctx.listen_port),
            nft::FAMILY,
            nft::TABLE
        ),
        Some("binding or redirecting port 443 needs root"),
        || {
            let file = paths.dir.join("kit_net.nft");
            write_if_changed(
                &file,
                &nft::ruleset(ctx.listen_port, caddy::http_port_for(ctx.listen_port)),
            )?;
            run_ok(ctx.root, &nft_bin, &["-f", s(&file)])?;
            Ok(Some(
                "not kept across a reboot; run `kit net enable` again after boot".into(),
            ))
        },
    )?;
    Ok(steps.list)
}

fn system_trusted(ctx: &NetCtx, store: SystemStore) -> bool {
    match store {
        SystemStore::P11Kit => run(ctx.runner, "trust", &["list", "--filter=ca-anchors"])
            .is_ok_and(|o| o.success && o.stdout.contains(super::ca::CA_COMMON_NAME)),
        SystemStore::Debian => Path::new("/usr/local/share/ca-certificates/kit-local-ca.crt").exists(),
    }
}

/// Undoes everything `enable` did. Safe to run when nothing is enabled.
pub fn disable(ctx: &NetCtx, opts: &Options) -> Result<Vec<Step>, NetError> {
    let paths = ctx.paths();
    let ca_dir = paths.ca_dir();
    let ca_pem = super::ca::CaFiles::new(ca_dir.clone()).cert();
    let state = load_state(ctx)?;
    let mut steps = Steps { opts, list: vec![] };
    let have = |t: &str| ctx.has(t);

    // Traffic first: names and redirect, then the service, then trust.
    let text = fs::read_to_string(&ctx.hosts_path).unwrap_or_default();
    let cleaned = hosts::apply(&text, &[]);
    if cleaned != text {
        steps.go(
            &format!("remove the kit block from {}", ctx.hosts_path.display()),
            Some("/etc/hosts belongs to root"),
            || {
                let staged = paths.dir.join("hosts.new");
                write_if_changed(&staged, &cleaned)?;
                run_ok(ctx.root, "cp", &[s(&staged), s(&ctx.hosts_path)])?;
                let _ = fs::remove_file(&staged);
                NO_NOTE
            },
        )?;
    }
    if let Some(nft_bin) = find_in_path("nft", &ctx.search_path).map(|p| s(&p).to_string())
        && (state.is_some() || opts.dry_run || nft_table_present(ctx, &nft_bin))
    {
        steps.go(
            &format!("delete the nftables table `{} {}`", nft::FAMILY, nft::TABLE),
            Some("nftables rules belong to root"),
            || {
                // An absent table is fine: the goal is that it is gone.
                if let Ok(out) = run(ctx.root, &nft_bin, &nft::delete_args())
                    && !out.success
                    && !out.stderr.contains("No such file")
                {
                    return Err(NetError::Command {
                        command: format!("nft delete table {} {}", nft::FAMILY, nft::TABLE),
                        stderr: out.stderr.trim().to_string(),
                    });
                }
                NO_NOTE
            },
        )?;
    }

    let unit_path = ctx.unit_file();
    if let Ok(text) = fs::read_to_string(&unit_path) {
        if caddy::is_managed(&text) {
            steps.go(&format!("stop and remove {}", caddy::UNIT_NAME), None, || {
                if let Some(sc) = find_in_path("systemctl", &ctx.search_path).map(|p| s(&p).to_string()) {
                    let _ = systemctl(ctx, &sc, &["disable", "--now", caddy::UNIT_NAME]);
                    fs::remove_file(&unit_path).map_err(io_err("cannot remove", &unit_path))?;
                    run_ok(ctx.runner, &sc, &["--user", "daemon-reload"])?;
                    NO_NOTE
                } else {
                    fs::remove_file(&unit_path).map_err(io_err("cannot remove", &unit_path))?;
                    Ok(Some(format!(
                        "systemctl not found: run `systemctl --user disable --now {}`",
                        caddy::UNIT_NAME
                    )))
                }
            })?;
        } else {
            steps.list.push(Step {
                what: format!("leave {} alone", unit_path.display()),
                root: false,
                why: None,
                note: Some("it was not written by kit".into()),
            });
        }
    }

    if ca_pem.exists() {
        let store = state
            .as_ref()
            .and_then(|s| s.system_store.as_deref())
            .and_then(SystemStore::parse)
            .or_else(|| SystemStore::detect(have));
        if let Some(store) = store {
            steps.go(
                "remove the local CA from the system certificate store",
                Some("the system certificate store belongs to root"),
                || {
                    // Removing something that is not there is not an error worth stopping for.
                    if let Err(e) = trust::remove_system(store, ctx.root, &ca_pem) {
                        return Ok(Some(format!("could not remove it (probably not installed): {e}")));
                    }
                    NO_NOTE
                },
            )?;
        }
    }
    if ctx.has("certutil") {
        let mut dbs = trust::find_nss_dbs(&ctx.home);
        for d in state.iter().flat_map(|s| s.nss_dbs.iter()) {
            if !dbs.contains(d) && d.join("cert9.db").exists() {
                dbs.push(d.clone());
            }
        }
        for db in dbs {
            steps.go(
                &format!("remove the local CA from {}", db.display()),
                None,
                || {
                    trust::remove_nss(ctx.runner, &db)?;
                    NO_NOTE
                },
            )?;
        }
    }

    steps.go("remove the Caddyfile and net state", None, || {
        for f in ["Caddyfile", "state.json", "kit_net.nft", "hosts.new"] {
            let _ = fs::remove_file(paths.dir.join(f));
        }
        NO_NOTE
    })?;
    if opts.remove_ca {
        steps.go(
            &format!("delete the local CA and its key ({})", ca_dir.display()),
            None,
            || {
                Ca::remove(&ca_dir)?;
                NO_NOTE
            },
        )?;
    }
    Ok(steps.list)
}

fn nft_table_present(ctx: &NetCtx, nft_bin: &str) -> bool {
    run(ctx.root, nft_bin, &nft::list_args()).is_ok_and(|o| o.success)
}

/// Read-only health checks; never asks for root.
pub fn status(ctx: &NetCtx) -> Vec<Check> {
    let paths = ctx.paths();
    let mut checks = Vec::new();
    let mut add = |name: &str, ok: bool, detail: String| {
        checks.push(Check {
            name: name.into(),
            ok,
            detail,
        })
    };
    let state = load_state(ctx).ok().flatten();
    let Some(state) = state else {
        add("net", false, "not enabled (run `kit net enable`)".into());
        return checks;
    };
    add(
        "hosts",
        true,
        format!("{} hosts incl. {PORTAL_HOST}", state.hosts.len()),
    );

    match Ca::load(&paths.ca_dir()) {
        Ok(Some(ca)) => {
            add(
                "ca",
                true,
                format!("{} (name-constrained)", ca.files.cert().display()),
            );
            add(
                "certificate",
                ca.leaf_current(&state.hosts, ctx.now),
                "server certificate covers all hosts and is not near expiry".into(),
            );
        }
        _ => add("ca", false, "local CA missing; run `kit net enable`".into()),
    }
    let text = fs::read_to_string(&ctx.hosts_path).unwrap_or_default();
    let mut want = state.hosts.clone();
    want.sort();
    add(
        "hosts file",
        hosts::current(&text) == want,
        format!("marked block in {}", ctx.hosts_path.display()),
    );
    let active = find_in_path("systemctl", &ctx.search_path)
        .map(|p| s(&p).to_string())
        .and_then(|sc| systemctl(ctx, &sc, &["is-active", "--quiet", caddy::UNIT_NAME]).ok())
        .is_some_and(|o| o.success);
    add(
        "caddy",
        active,
        format!("{} on 127.0.0.1:{}", caddy::UNIT_NAME, state.listen_port),
    );
    match find_in_path("nft", &ctx.search_path).and_then(|p| run(ctx.runner, s(&p), &nft::list_args()).ok()) {
        Some(o) if o.success => add("redirect", true, format!("443 -> {}", state.listen_port)),
        Some(o) if o.stderr.contains("Operation not permitted") || o.stderr.contains("Permission") => add(
            "redirect",
            true,
            format!(
                "cannot be read without root; check with: sudo nft list table {} {}",
                nft::FAMILY,
                nft::TABLE
            ),
        ),
        _ => add(
            "redirect",
            false,
            "missing (not kept across reboot); run `kit net enable`".into(),
        ),
    }
    if let Some(store) = state.system_store.as_deref().and_then(SystemStore::parse) {
        add(
            "system trust",
            system_trusted(ctx, store),
            format!("system store ({})", store.as_str()),
        );
    }
    for db in &state.nss_dbs {
        add(
            "browser trust",
            trust::has_nss(ctx.runner, db),
            db.display().to_string(),
        );
    }
    add(
        "limits",
        true,
        "Flatpak/Snap browsers keep their own certificate store and are not covered".into(),
    );
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::runner::CmdOutput;
    use crate::install::runner::testing::FakeRunner;
    use crate::install::tools::fake_exe;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    const NOW: u64 = 1_788_000_000;
    const HOSTS: &str = "127.0.0.1 localhost\n10.0.0.5 nas\n";

    struct Env {
        _dir: tempfile::TempDir,
        root_dir: PathBuf,
        bin: PathBuf,
        data: PathBuf,
        units: PathBuf,
        home: PathBuf,
        hosts: PathBuf,
        user: FakeRunner,
        root: FakeRunner,
    }

    fn mirror(h: &str, b: &str) -> Mirror {
        Mirror {
            host: h.into(),
            book: b.into(),
        }
    }

    fn desired() -> Desired {
        Desired {
            services: vec![],
            ollama_port: None,
            mirrors: vec![
                mirror("de.wikipedia.org", "wikipedia_de_all_nopic_2026-10"),
                mirror("de.m.wikipedia.org", "wikipedia_de_all_nopic_2026-10"),
            ],
            permitted: vec![
                "de.wikipedia.org".into(),
                "de.m.wikipedia.org".into(),
                "en.wikipedia.org".into(),
            ],
        }
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root_dir = dir.path().to_path_buf();
        let bin = root_dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for t in ["caddy", "nft", "systemctl", "trust", "certutil", "sudo"] {
            fake_exe(&bin, t);
        }
        let home = root_dir.join("home");
        std::fs::create_dir_all(home.join(".pki/nssdb")).unwrap();
        std::fs::write(home.join(".pki/nssdb/cert9.db"), "").unwrap();
        let hosts = root_dir.join("hosts");
        std::fs::write(&hosts, HOSTS).unwrap();

        let active = Rc::new(Cell::new(false));
        let nss = Rc::new(RefCell::new(false));
        let trusted = Rc::new(Cell::new(false));
        let (a, n, t) = (active.clone(), nss.clone(), trusted.clone());
        let user = FakeRunner::new(move |prog, args| {
            Ok(match prog {
                "systemctl" if args.contains(&"enable") => {
                    a.set(true);
                    CmdOutput::ok()
                }
                "systemctl" if args.contains(&"disable") => {
                    a.set(false);
                    CmdOutput::ok()
                }
                "systemctl" if args.contains(&"is-active") => {
                    if a.get() {
                        CmdOutput::ok()
                    } else {
                        CmdOutput::failed("")
                    }
                }
                "certutil" if args.contains(&"-A") => {
                    *n.borrow_mut() = true;
                    CmdOutput::ok()
                }
                "certutil" if args.contains(&"-D") => {
                    *n.borrow_mut() = false;
                    CmdOutput::ok()
                }
                "certutil" if args.contains(&"-L") => {
                    if *n.borrow() {
                        CmdOutput::ok()
                    } else {
                        CmdOutput::failed("not found")
                    }
                }
                "trust" if args.contains(&"list") => CmdOutput {
                    success: true,
                    stdout: if t.get() {
                        format!("label: {}\n", super::super::ca::CA_COMMON_NAME)
                    } else {
                        String::new()
                    },
                    stderr: String::new(),
                },
                "nft" => CmdOutput::failed("Operation not permitted"),
                _ => CmdOutput::ok(),
            })
        });
        let root = FakeRunner::new(move |prog, args| {
            if prog == "cp" {
                std::fs::copy(args[0], args[1]).unwrap();
            }
            if prog == "trust" {
                trusted.set(!args.contains(&"--remove"));
            }
            Ok(CmdOutput::ok())
        });
        Env {
            data: root_dir.join("data"),
            units: root_dir.join("units"),
            _dir: dir,
            root_dir,
            bin,
            home,
            hosts,
            user,
            root,
        }
    }

    impl Env {
        fn ctx(&self) -> NetCtx<'_> {
            NetCtx {
                data_dir: self.data.clone(),
                unit_dir: self.units.clone(),
                home: self.home.clone(),
                hosts_path: self.hosts.clone(),
                runner: &self.user,
                root: &self.root,
                needs_sudo: true,
                search_path: vec![self.bin.clone()],
                now: NOW,
                listen_port: 8443,
                kiwix_port: 6589,
                start_wait: Duration::ZERO,
            }
        }
    }

    #[test]
    fn enable_sets_everything_up_and_only_root_steps_use_root() {
        let e = env();
        let steps = enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        assert!(steps.iter().any(|s| s.root && s.why.is_some()));

        let net = e.data.join("net");
        assert!(net.join("ca/ca.pem").exists() && net.join("ca/leaf.pem").exists());
        let caddyfile = std::fs::read_to_string(net.join("Caddyfile")).unwrap();
        assert!(caddyfile.contains("https://de.m.wikipedia.org:8443, https://de.wikipedia.org:8443"));
        assert!(
            std::fs::read_to_string(e.units.join(caddy::UNIT_NAME))
                .unwrap()
                .contains("caddy")
        );
        let hosts = std::fs::read_to_string(&e.hosts).unwrap();
        assert!(hosts.starts_with(HOSTS) && hosts.contains("127.77.0.1 de.wikipedia.org"));
        // The CA may issue for the whole catalog, the certificate only for what is installed.
        assert!(
            std::fs::read_to_string(net.join("ca/permitted.txt"))
                .unwrap()
                .contains("en.wikipedia.org")
        );
        assert!(
            !std::fs::read_to_string(net.join("ca/leaf-hosts.txt"))
                .unwrap()
                .contains("en.wikipedia.org")
        );

        let root_calls = e.root.calls();
        assert!(
            root_calls.iter().any(|c| c.starts_with("trust anchor")),
            "{root_calls:?}"
        );
        assert!(
            root_calls.iter().any(|c| c.starts_with("nft -f")),
            "{root_calls:?}"
        );
        assert!(root_calls.iter().any(|c| c.starts_with("cp ")), "{root_calls:?}");
        // Nothing privileged ever goes through the unprivileged runner.
        for c in e.user.calls() {
            assert!(
                !c.starts_with("trust anchor") && !c.starts_with("nft -f") && !c.starts_with("cp "),
                "{c}"
            );
        }
        assert_eq!(e.user.count("caddy validate"), 1);
        assert_eq!(e.user.count("certutil -A"), 1);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let e = env();
        let steps = enable(
            &e.ctx(),
            &desired(),
            &Options {
                dry_run: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(steps.len() >= 6);
        assert!(!e.data.exists() && !e.units.exists());
        assert_eq!(std::fs::read_to_string(&e.hosts).unwrap(), HOSTS);
        assert!(e.root.calls().is_empty());
        assert_eq!(e.user.count("caddy validate"), 0);
    }

    #[test]
    fn enable_twice_does_not_repeat_privileged_work() {
        let e = env();
        enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        let root_before = e.root.calls().len();
        let restarts = e.user.count("restart");
        enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        let again: Vec<String> = e.root.calls()[root_before..].to_vec();
        // Only the redirect is re-loaded (it is cheap and replaces itself); no trust, no hosts.
        assert_eq!(again.len(), 1, "{again:?}");
        assert!(again[0].starts_with("nft -f"));
        assert_eq!(e.user.count("restart"), restarts);
        assert_eq!(
            std::fs::read_to_string(&e.hosts)
                .unwrap()
                .matches("# BEGIN kit")
                .count(),
            1
        );
    }

    #[test]
    fn disable_restores_the_host_and_keeps_the_ca_unless_asked() {
        let e = env();
        enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        disable(&e.ctx(), &Options::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&e.hosts).unwrap(), HOSTS);
        assert!(!e.units.join(caddy::UNIT_NAME).exists());
        assert!(!e.data.join("net/Caddyfile").exists() && !e.data.join("net/state.json").exists());
        assert!(e.data.join("net/ca/ca.pem").exists());
        assert!(
            e.root
                .calls()
                .iter()
                .any(|c| c.starts_with("nft delete table ip kit_net"))
        );
        assert!(
            e.root
                .calls()
                .iter()
                .any(|c| c.starts_with("trust anchor --remove"))
        );
        assert_eq!(
            e.user.count("certutil -D"),
            2,
            "replace-before-add plus the removal"
        );

        disable(
            &e.ctx(),
            &Options {
                remove_ca: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!e.data.join("net/ca").exists());
    }

    #[test]
    fn disable_without_enable_is_a_no_op() {
        let e = env();
        let steps = disable(&e.ctx(), &Options::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&e.hosts).unwrap(), HOSTS);
        assert!(steps.iter().all(|s| !s.what.contains("hosts")), "{steps:?}");
    }

    #[test]
    fn refuses_foreign_unit_missing_tools_and_empty_desire() {
        let e = env();
        std::fs::create_dir_all(&e.units).unwrap();
        std::fs::write(e.units.join(caddy::UNIT_NAME), "[Service]\n").unwrap();
        assert!(matches!(
            enable(&e.ctx(), &desired(), &Options::default()),
            Err(NetError::Foreign(_))
        ));
        std::fs::remove_file(e.units.join(caddy::UNIT_NAME)).unwrap();

        std::fs::remove_file(e.bin.join("caddy")).unwrap();
        let err = enable(&e.ctx(), &desired(), &Options::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("caddy"), "{err}");

        let err = enable(&env().ctx(), &Desired::default(), &Options::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("kit install"), "{err}");
        assert!(e.root_dir.exists());
    }

    #[test]
    fn new_hosts_outside_the_ca_need_an_explicit_renewal() {
        let e = env();
        enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        let mut more = desired();
        more.mirrors
            .push(mirror("fr.wikipedia.org", "wikipedia_fr_all_nopic_2026-10"));
        let err = enable(&e.ctx(), &more, &Options::default())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--renew-ca") && err.contains("fr.wikipedia.org"),
            "{err}"
        );

        let before = std::fs::read_to_string(e.data.join("net/ca/ca.pem")).unwrap();
        enable(
            &e.ctx(),
            &more,
            &Options {
                renew_ca: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(
            before,
            std::fs::read_to_string(e.data.join("net/ca/ca.pem")).unwrap()
        );
        assert!(
            std::fs::read_to_string(&e.hosts)
                .unwrap()
                .contains("fr.wikipedia.org")
        );
        assert!(
            e.root
                .calls()
                .iter()
                .any(|c| c.starts_with("trust anchor --remove")),
            "old CA is untrusted"
        );
    }

    #[test]
    fn status_reports_missing_and_healthy() {
        let e = env();
        assert!(!status(&e.ctx())[0].ok);
        enable(&e.ctx(), &desired(), &Options::default()).unwrap();
        let checks = status(&e.ctx());
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        std::fs::write(&e.hosts, HOSTS).unwrap();
        let checks = status(&e.ctx());
        assert!(!checks.iter().find(|c| c.name == "hosts file").unwrap().ok);
    }
}
