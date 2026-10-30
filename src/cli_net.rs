//! `kit net enable | disable | status`.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::catalog::Catalog;
use crate::cli::{Env, Outcome};
use crate::install::DEFAULT_PORT;
use crate::install::Paths;
use crate::install::state::StateGuard;
use crate::netcfg::caddy::{self, Mirror};
use crate::netcfg::enable::{self, Desired, NetCtx, Options, Step};

#[derive(Debug, Args)]
pub struct NetArgs {
    #[command(subcommand)]
    pub action: NetAction,
}

#[derive(Debug, Subcommand)]
pub enum NetAction {
    /// Serve the installed mirrors under their real domains over HTTPS (changes /etc/hosts, the
    /// certificate stores and nftables; asks for root only for those steps)
    Enable {
        /// Show what would change and which steps need root, then stop
        #[arg(long)]
        dry_run: bool,
        /// Create a new CA (needed when packages with new domains were added since it was made)
        #[arg(long)]
        renew_ca: bool,
        /// Port Caddy listens on (443 is redirected to it)
        #[arg(long, default_value_t = caddy::DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1024..))]
        listen_port: u16,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Undo everything `enable` did
    Disable {
        #[arg(long)]
        dry_run: bool,
        /// Also delete the local CA and its private key
        #[arg(long)]
        remove_ca: bool,
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// Check the CA, certificate, hosts block, redirect, service and browser trust
    Status {
        #[arg(long, env = "KIT_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
}

/// Mirrors of the installed packages and the hosts the CA may ever cover.
fn desired(catalog: &Catalog, paths: &Paths) -> Result<(Desired, u16)> {
    let guard = StateGuard::open(paths)?;
    let mut d = Desired::default();
    for m in catalog.packages() {
        d.permitted.extend(m.mirror_domains.iter().cloned());
    }
    d.permitted.sort();
    d.permitted.dedup();
    for rec in guard.packages.values() {
        let Ok(manifest) = catalog.package(&rec.id) else {
            continue;
        };
        let Some(file) = rec.files.first() else { continue };
        let book = file.name.strip_suffix(".zim").unwrap_or(&file.name).to_string();
        for host in &manifest.mirror_domains {
            d.mirrors.push(Mirror {
                host: host.clone(),
                book: book.clone(),
            });
        }
    }
    Ok((d, guard.port.unwrap_or(DEFAULT_PORT)))
}

fn print_steps(out: &mut dyn Write, heading: &str, steps: &[Step]) -> Result<()> {
    writeln!(out, "{heading}")?;
    for s in steps {
        let tag = if s.root { "[root]" } else { "      " };
        writeln!(out, "  {tag} {}", s.what)?;
        if let Some(why) = &s.why {
            writeln!(out, "           why root: {why}")?;
        }
        if let Some(note) = &s.note {
            writeln!(out, "           note: {note}")?;
        }
    }
    Ok(())
}

pub fn net_cmd(args: &NetArgs, catalog: &Catalog, env: &Env, out: &mut dyn Write) -> Result<Outcome> {
    let data_dir = |d: &Option<PathBuf>| crate::cli::data_dir_of(d);
    let mk = |data_dir: PathBuf, listen_port: u16, kiwix_port: u16| NetCtx {
        data_dir,
        unit_dir: env.unit_dir.clone(),
        home: env.home.clone(),
        hosts_path: env.hosts_path.clone(),
        runner: env.runner,
        root: env.root,
        needs_sudo: env.needs_sudo,
        search_path: env.search_path.clone(),
        now: crate::cli::unix_now(),
        listen_port,
        kiwix_port,
        start_wait: env.start_wait,
    };
    match &args.action {
        NetAction::Enable {
            dry_run,
            renew_ca,
            listen_port,
            data_dir: dd,
        } => {
            let dir = data_dir(dd)?;
            let paths = Paths::new(dir.clone(), env.unit_dir.clone())?;
            let (want, kiwix_port) = desired(catalog, &paths)?;
            let ctx = mk(dir, *listen_port, kiwix_port);
            let opts = Options {
                dry_run: true,
                renew_ca: *renew_ca,
                ..Default::default()
            };
            // Always show the plan first: this is the place where root is explained.
            let plan = enable::enable(&ctx, &want, &opts)?;
            print_steps(out, if *dry_run { "Would do:" } else { "Doing:" }, &plan)?;
            if *dry_run {
                return Ok(Outcome::Ok);
            }
            if plan.iter().any(|s| s.root) && env.needs_sudo {
                writeln!(out, "\nsudo may ask for your password for the [root] steps.")?;
            }
            out.flush()?;
            let done = enable::enable(
                &ctx,
                &want,
                &Options {
                    dry_run: false,
                    ..opts
                },
            )?;
            for s in done.iter().filter(|s| s.note.is_some()) {
                writeln!(out, "note: {}: {}", s.what, s.note.as_deref().unwrap_or(""))?;
            }
            writeln!(out, "\nLocal HTTPS is on. Hosts: {}", want.hosts().join(", "))?;
            writeln!(out, "Check it with: kit net status. Undo with: kit net disable")?;
            Ok(Outcome::Ok)
        }
        NetAction::Disable {
            dry_run,
            remove_ca,
            data_dir: dd,
        } => {
            let ctx = mk(data_dir(dd)?, caddy::DEFAULT_PORT, DEFAULT_PORT);
            let opts = Options {
                dry_run: *dry_run,
                remove_ca: *remove_ca,
                ..Default::default()
            };
            if !dry_run {
                let plan = enable::disable(
                    &ctx,
                    &Options {
                        dry_run: true,
                        ..opts.clone()
                    },
                )?;
                print_steps(out, "Doing:", &plan)?;
                out.flush()?;
            }
            let steps = enable::disable(&ctx, &opts)?;
            if *dry_run {
                print_steps(out, "Would do:", &steps)?;
            } else {
                writeln!(
                    out,
                    "\nLocal HTTPS is off; the real internet names resolve normally again."
                )?;
            }
            Ok(Outcome::Ok)
        }
        NetAction::Status { data_dir: dd } => {
            let ctx = mk(data_dir(dd)?, caddy::DEFAULT_PORT, DEFAULT_PORT);
            let checks = enable::status(&ctx);
            let failed = checks.iter().filter(|c| !c.ok).count();
            for c in &checks {
                writeln!(
                    out,
                    "[{}] {} ({})",
                    if c.ok { " ok " } else { "FAIL" },
                    c.name,
                    c.detail
                )?;
            }
            Ok(if failed == 0 {
                Outcome::Ok
            } else {
                Outcome::ChecksFailed
            })
        }
    }
}
