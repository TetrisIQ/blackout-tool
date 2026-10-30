//! The per-machine local CA. It is created on this machine only, its key never leaves
//! `<data-dir>/net/ca/` (mode 0600), and X.509 name constraints limit it to the mirrored hosts,
//! so a stolen key cannot impersonate any other site.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, GeneralSubtree,
    IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints, SanType,
};
use time::{Duration, OffsetDateTime};

use super::{NetError, io_err};
use crate::catalog::manifest::is_valid_host;

pub const CA_COMMON_NAME: &str = "kit local CA (this machine only)";
const CA_YEARS: i64 = 5;
const LEAF_DAYS: i64 = 365;
/// A leaf with less validity than this is re-issued.
pub const RENEW_BEFORE_DAYS: i64 = 30;

pub struct CaFiles {
    pub dir: PathBuf,
}

impl CaFiles {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
    pub fn cert(&self) -> PathBuf {
        self.dir.join("ca.pem")
    }
    pub fn key(&self) -> PathBuf {
        self.dir.join("ca.key")
    }
    /// Hosts the CA may issue for (one per line); the name constraint, readable without parsing X.509.
    pub fn permitted(&self) -> PathBuf {
        self.dir.join("permitted.txt")
    }
    pub fn leaf_cert(&self) -> PathBuf {
        self.dir.join("leaf.pem")
    }
    pub fn leaf_key(&self) -> PathBuf {
        self.dir.join("leaf.key")
    }
    /// SANs of the current leaf, one per line.
    pub fn leaf_hosts(&self) -> PathBuf {
        self.dir.join("leaf-hosts.txt")
    }
    pub fn leaf_expiry(&self) -> PathBuf {
        self.dir.join("leaf-expires.txt")
    }
}

fn cert_err(e: rcgen::Error) -> NetError {
    NetError::Cert(e.to_string())
}

fn ca_dn() -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, CA_COMMON_NAME);
    dn
}

fn ca_params(permitted: &[String], now: u64) -> Result<CertificateParams, NetError> {
    let mut p = CertificateParams::default();
    p.distinguished_name = ca_dn();
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    p.name_constraints = Some(NameConstraints {
        permitted_subtrees: permitted.iter().cloned().map(GeneralSubtree::DnsName).collect(),
        excluded_subtrees: vec![],
    });
    let start = at(now)? - Duration::days(1);
    p.not_before = start;
    p.not_after = start + Duration::days(365 * CA_YEARS);
    Ok(p)
}

fn at(secs: u64) -> Result<OffsetDateTime, NetError> {
    i64::try_from(secs)
        .ok()
        .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok())
        .ok_or_else(|| NetError::Invalid("clock out of range".into()))
}

fn write_private(path: &Path, content: &str) -> Result<(), NetError> {
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
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

fn write_public(path: &Path, content: &str) -> Result<(), NetError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(io_err("cannot write", path))
}

fn normalized(hosts: &[String]) -> Result<Vec<String>, NetError> {
    let mut v: Vec<String> = hosts.to_vec();
    v.sort();
    v.dedup();
    if v.is_empty() {
        return Err(NetError::Invalid(
            "no mirrored domains: nothing to issue a certificate for".into(),
        ));
    }
    if let Some(bad) = v.iter().find(|h| !is_valid_host(h)) {
        return Err(NetError::Invalid(format!("{bad:?} is not a valid host name")));
    }
    Ok(v)
}

fn read_lines(path: &Path) -> Option<Vec<String>> {
    let s = fs::read_to_string(path).ok()?;
    Some(s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
}

pub struct Ca {
    pub files: CaFiles,
    key: KeyPair,
}

impl Ca {
    /// The existing CA, or `None` when none was created yet.
    pub fn load(dir: &Path) -> Result<Option<Self>, NetError> {
        let files = CaFiles::new(dir.to_path_buf());
        if !files.key().exists() || !files.cert().exists() {
            return Ok(None);
        }
        let pem = fs::read_to_string(files.key()).map_err(io_err("cannot read", &files.key()))?;
        let key = KeyPair::from_pem(&pem).map_err(cert_err)?;
        Ok(Some(Self { files, key }))
    }

    /// Creates a new CA that may issue for `permitted` only. Replaces an existing one.
    pub fn create(dir: &Path, permitted: &[String], now: u64) -> Result<Self, NetError> {
        let permitted = normalized(permitted)?;
        fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io_err("cannot protect", dir))?;
        let files = CaFiles::new(dir.to_path_buf());
        let key = KeyPair::generate().map_err(cert_err)?;
        let cert = ca_params(&permitted, now)?.self_signed(&key).map_err(cert_err)?;
        // Key first: a crash between the two leaves an unusable key, never a cert without one.
        write_private(&files.key(), &key.serialize_pem())?;
        write_public(&files.permitted(), &(permitted.join("\n") + "\n"))?;
        write_public(&files.cert(), &cert.pem())?;
        // Any leaf of the old CA no longer chains to this one.
        for stale in [
            files.leaf_cert(),
            files.leaf_key(),
            files.leaf_hosts(),
            files.leaf_expiry(),
        ] {
            let _ = fs::remove_file(stale);
        }
        Ok(Self { files, key })
    }

    /// Hosts this CA may issue for.
    pub fn permitted(&self) -> Vec<String> {
        read_lines(&self.files.permitted()).unwrap_or_default()
    }

    /// Hosts in `hosts` the CA cannot issue for (its name constraint is fixed at creation).
    pub fn uncovered(&self, hosts: &[String]) -> Vec<String> {
        let permitted = self.permitted();
        hosts.iter().filter(|h| !permitted.contains(h)).cloned().collect()
    }

    /// Whether the current leaf already has exactly these hosts and is not close to expiry.
    pub fn leaf_current(&self, hosts: &[String], now: u64) -> bool {
        let Ok(want) = normalized(hosts) else { return false };
        let have = read_lines(&self.files.leaf_hosts());
        let expiry = fs::read_to_string(self.files.leaf_expiry())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        have.as_deref() == Some(want.as_slice())
            && self.files.leaf_cert().exists()
            && self.files.leaf_key().exists()
            && expiry.is_some_and(|e| e > now + (RENEW_BEFORE_DAYS as u64) * 86_400)
    }

    /// Issues (or keeps, if still current) the server certificate for `hosts`.
    /// Returns whether a new certificate was written.
    pub fn ensure_leaf(&self, hosts: &[String], now: u64) -> Result<bool, NetError> {
        let hosts = normalized(hosts)?;
        let missing = self.uncovered(&hosts);
        if !missing.is_empty() {
            return Err(NetError::Invalid(format!(
                "the local CA does not cover {}; recreate it with `kit net enable --renew-ca`",
                missing.join(", ")
            )));
        }
        if self.leaf_current(&hosts, now) {
            return Ok(false);
        }
        let issuer = Issuer::new(ca_params(&self.permitted(), now)?, &self.key);
        let key = KeyPair::generate().map_err(cert_err)?;
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, hosts[0].clone());
        p.distinguished_name = dn;
        p.subject_alt_names = hosts
            .iter()
            .map(|h| h.clone().try_into().map(SanType::DnsName).map_err(cert_err))
            .collect::<Result<_, _>>()?;
        p.is_ca = IsCa::ExplicitNoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let start = at(now)? - Duration::days(1);
        p.not_before = start;
        p.not_after = start + Duration::days(LEAF_DAYS);
        let expires = p.not_after.unix_timestamp();
        let cert = p.signed_by(&key, &issuer).map_err(cert_err)?;
        write_private(&self.files.leaf_key(), &key.serialize_pem())?;
        write_public(&self.files.leaf_cert(), &cert.pem())?;
        write_public(&self.files.leaf_hosts(), &(hosts.join("\n") + "\n"))?;
        write_public(&self.files.leaf_expiry(), &format!("{expires}\n"))?;
        Ok(true)
    }

    /// Deletes the CA directory including the key.
    pub fn remove(dir: &Path) -> Result<(), NetError> {
        match fs::remove_dir_all(dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_err("cannot remove", dir)(e)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const NOW: u64 = 1_788_000_000; // 2026-08
    fn hosts(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn key_is_private_and_reload_works() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("ca");
        assert!(Ca::load(&dir).unwrap().is_none());
        let ca = Ca::create(&dir, &hosts(&["de.wikipedia.org"]), NOW).unwrap();
        let mode = fs::metadata(ca.files.key()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let again = Ca::load(&dir).unwrap().unwrap();
        assert_eq!(again.permitted(), hosts(&["de.wikipedia.org"]));
    }

    #[test]
    fn leaf_is_reused_until_hosts_change() {
        let d = tempfile::tempdir().unwrap();
        let ca = Ca::create(d.path(), &hosts(&["a.example.org", "b.example.org"]), NOW).unwrap();
        assert!(ca.ensure_leaf(&hosts(&["a.example.org"]), NOW).unwrap());
        assert!(!ca.ensure_leaf(&hosts(&["a.example.org"]), NOW).unwrap());
        assert!(
            ca.ensure_leaf(&hosts(&["a.example.org", "b.example.org"]), NOW)
                .unwrap()
        );
        // close to expiry: renewed
        assert!(
            ca.ensure_leaf(&hosts(&["a.example.org", "b.example.org"]), NOW + 340 * 86_400)
                .unwrap()
        );
    }

    #[test]
    fn refuses_hosts_outside_the_constraint() {
        let d = tempfile::tempdir().unwrap();
        let ca = Ca::create(d.path(), &hosts(&["a.example.org"]), NOW).unwrap();
        let e = ca.ensure_leaf(&hosts(&["evil.example.com"]), NOW).unwrap_err();
        assert!(e.to_string().contains("--renew-ca"), "{e}");
    }

    #[test]
    fn rejects_bad_hosts() {
        let d = tempfile::tempdir().unwrap();
        assert!(Ca::create(d.path(), &hosts(&["*.example.org"]), NOW).is_err());
        assert!(Ca::create(d.path(), &[], NOW).is_err());
    }

    #[test]
    fn renewing_the_ca_drops_the_old_leaf() {
        let d = tempfile::tempdir().unwrap();
        let ca = Ca::create(d.path(), &hosts(&["a.example.org"]), NOW).unwrap();
        ca.ensure_leaf(&hosts(&["a.example.org"]), NOW).unwrap();
        let ca = Ca::create(d.path(), &hosts(&["a.example.org"]), NOW).unwrap();
        assert!(!ca.files.leaf_cert().exists());
    }

    /// Checks the constraint with a real verifier. Skipped when `openssl` is missing.
    #[test]
    fn name_constraints_are_enforced_by_openssl() {
        if Command::new("openssl").arg("version").output().is_err() {
            eprintln!("openssl not found, skipping");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let ca = Ca::create(d.path(), &hosts(&["de.wikipedia.org"]), NOW).unwrap();
        ca.ensure_leaf(&hosts(&["de.wikipedia.org"]), NOW).unwrap();
        let at = NOW.to_string();
        let verify = |leaf: &Path| {
            let o = Command::new("openssl")
                .args(["verify", "-attime", &at, "-CAfile"])
                .arg(ca.files.cert())
                .arg(leaf)
                .output()
                .unwrap();
            (
                o.status.success(),
                String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
            )
        };
        let (ok, out) = verify(&ca.files.leaf_cert());
        assert!(ok, "allowed host must verify: {out}");

        // A leaf for a forbidden name, signed with the very same CA key, must be rejected.
        let issuer = Issuer::new(ca_params(&ca.permitted(), NOW).unwrap(), &ca.key);
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(vec!["example.com".to_string()]).unwrap();
        let start = at_secs(NOW) - Duration::days(1);
        p.not_before = start;
        p.not_after = start + Duration::days(30);
        let forged = p.signed_by(&key, &issuer).unwrap();
        let path = d.path().join("forged.pem");
        fs::write(&path, forged.pem()).unwrap();
        let (ok, out) = verify(&path);
        assert!(!ok, "forbidden host must not verify: {out}");
        assert!(
            out.to_lowercase().contains("permitted") || out.contains("47"),
            "{out}"
        );
    }

    fn at_secs(s: u64) -> OffsetDateTime {
        at(s).unwrap()
    }
}
