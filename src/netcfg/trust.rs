//! Making the local CA trusted: the system store (root) and the NSS databases of Firefox and
//! Chromium (the user's own files). Flatpak and Snap browsers keep their own profiles and are not
//! handled; `kit net status` says so.

use std::path::{Path, PathBuf};

use super::NetError;
use crate::install::runner::CommandRunner;
use crate::install::tools::s;

pub const NSS_NICKNAME: &str = "kit local CA";
const DEBIAN_CERT: &str = "/usr/local/share/ca-certificates/kit-local-ca.crt";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemStore {
    /// `trust anchor` (Arch, Fedora, openSUSE).
    P11Kit,
    /// `update-ca-certificates` (Debian, Ubuntu).
    Debian,
}

impl SystemStore {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::P11Kit => "p11-kit",
            Self::Debian => "debian",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "p11-kit" => Some(Self::P11Kit),
            "debian" => Some(Self::Debian),
            _ => None,
        }
    }

    /// Prefers whatever the distro ships: `trust` first, then `update-ca-certificates`.
    pub fn detect(has: impl Fn(&str) -> bool) -> Option<Self> {
        if has("trust") {
            Some(Self::P11Kit)
        } else if has("update-ca-certificates") {
            Some(Self::Debian)
        } else {
            None
        }
    }
}

fn run(r: &dyn CommandRunner, prog: &str, args: &[&str]) -> Result<(), NetError> {
    let out = r.run(prog, args).map_err(|e| NetError::Command {
        command: format!("{prog} {}", args.join(" ")),
        stderr: e.to_string(),
    })?;
    if out.success {
        Ok(())
    } else {
        Err(NetError::Command {
            command: format!("{prog} {}", args.join(" ")),
            stderr: out.stderr.trim().to_string(),
        })
    }
}

/// `root` runs commands with root privileges.
pub fn add_system(store: SystemStore, root: &dyn CommandRunner, ca_pem: &Path) -> Result<(), NetError> {
    match store {
        SystemStore::P11Kit => run(root, "trust", &["anchor", s(ca_pem)]),
        SystemStore::Debian => {
            run(root, "install", &["-m", "0644", s(ca_pem), DEBIAN_CERT])?;
            run(root, "update-ca-certificates", &[])
        }
    }
}

pub fn remove_system(store: SystemStore, root: &dyn CommandRunner, ca_pem: &Path) -> Result<(), NetError> {
    match store {
        SystemStore::P11Kit => run(root, "trust", &["anchor", "--remove", s(ca_pem)]),
        SystemStore::Debian => {
            run(root, "rm", &["-f", DEBIAN_CERT])?;
            run(root, "update-ca-certificates", &["--fresh"])
        }
    }
}

/// NSS databases that exist: `~/.pki/nssdb` and every Firefox profile (`cert9.db`).
pub fn find_nss_dbs(home: &Path) -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    let shared = home.join(".pki/nssdb");
    if shared.join("cert9.db").exists() {
        dbs.push(shared);
    }
    for base in [
        home.join(".mozilla/firefox"),
        home.join(".config/mozilla/firefox"),
    ] {
        let Ok(rd) = std::fs::read_dir(&base) else {
            continue;
        };
        let mut profiles: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("cert9.db").exists())
            .collect();
        profiles.sort();
        dbs.extend(profiles);
    }
    dbs
}

fn db_arg(db: &Path) -> String {
    format!("sql:{}", db.display())
}

pub fn add_nss(r: &dyn CommandRunner, db: &Path, ca_pem: &Path) -> Result<(), NetError> {
    let d = db_arg(db);
    // Replace an older copy (a renewed CA has a new key) so it can never linger.
    let _ = r.run("certutil", &["-D", "-d", &d, "-n", NSS_NICKNAME]);
    run(
        r,
        "certutil",
        &["-A", "-d", &d, "-n", NSS_NICKNAME, "-t", "C,,", "-i", s(ca_pem)],
    )
}

pub fn remove_nss(r: &dyn CommandRunner, db: &Path) -> Result<(), NetError> {
    let d = db_arg(db);
    let present = r
        .run("certutil", &["-L", "-d", &d, "-n", NSS_NICKNAME])
        .is_ok_and(|o| o.success);
    if present {
        run(r, "certutil", &["-D", "-d", &d, "-n", NSS_NICKNAME])?;
    }
    Ok(())
}

pub fn has_nss(r: &dyn CommandRunner, db: &Path) -> bool {
    r.run("certutil", &["-L", "-d", &db_arg(db), "-n", NSS_NICKNAME])
        .is_ok_and(|o| o.success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::runner::CmdOutput;
    use crate::install::runner::testing::FakeRunner;

    #[test]
    fn detects_store_by_available_tool() {
        assert_eq!(SystemStore::detect(|t| t == "trust"), Some(SystemStore::P11Kit));
        assert_eq!(
            SystemStore::detect(|t| t == "update-ca-certificates"),
            Some(SystemStore::Debian)
        );
        assert_eq!(SystemStore::detect(|_| false), None);
        assert_eq!(SystemStore::parse("debian"), Some(SystemStore::Debian));
    }

    #[test]
    fn system_commands() {
        let r = FakeRunner::new(|_, _| Ok(CmdOutput::ok()));
        add_system(SystemStore::P11Kit, &r, Path::new("/n/ca.pem")).unwrap();
        remove_system(SystemStore::P11Kit, &r, Path::new("/n/ca.pem")).unwrap();
        add_system(SystemStore::Debian, &r, Path::new("/n/ca.pem")).unwrap();
        remove_system(SystemStore::Debian, &r, Path::new("/n/ca.pem")).unwrap();
        assert_eq!(
            r.calls(),
            [
                "trust anchor /n/ca.pem",
                "trust anchor --remove /n/ca.pem",
                "install -m 0644 /n/ca.pem /usr/local/share/ca-certificates/kit-local-ca.crt",
                "update-ca-certificates ",
                "rm -f /usr/local/share/ca-certificates/kit-local-ca.crt",
                "update-ca-certificates --fresh",
            ]
        );
    }

    #[test]
    fn finds_only_existing_nss_dbs() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        for p in [
            ".pki/nssdb",
            ".mozilla/firefox/abc.default",
            ".mozilla/firefox/Crash Reports",
        ] {
            std::fs::create_dir_all(home.join(p)).unwrap();
        }
        std::fs::write(home.join(".pki/nssdb/cert9.db"), "").unwrap();
        std::fs::write(home.join(".mozilla/firefox/abc.default/cert9.db"), "").unwrap();
        assert_eq!(
            find_nss_dbs(home),
            [home.join(".pki/nssdb"), home.join(".mozilla/firefox/abc.default")]
        );
        assert!(find_nss_dbs(&home.join("nothing")).is_empty());
    }

    #[test]
    fn nss_add_replaces_and_remove_is_idempotent() {
        let r = FakeRunner::new(|_, args| {
            Ok(if args.contains(&"-L") {
                CmdOutput::failed("not found")
            } else {
                CmdOutput::ok()
            })
        });
        add_nss(&r, Path::new("/db"), Path::new("/n/ca.pem")).unwrap();
        assert_eq!(r.count("-D -d sql:/db -n kit local CA"), 1);
        assert_eq!(r.count("-A -d sql:/db -n kit local CA -t C,, -i /n/ca.pem"), 1);
        remove_nss(&r, Path::new("/db")).unwrap();
        assert_eq!(r.count("-D"), 1, "absent cert is not deleted again");
    }

    #[test]
    fn failing_command_reports_stderr() {
        let r = FakeRunner::new(|_, _| Ok(CmdOutput::failed("permission denied")));
        let e = add_system(SystemStore::P11Kit, &r, Path::new("/n/ca.pem")).unwrap_err();
        assert!(e.to_string().contains("permission denied"), "{e}");
    }
}
