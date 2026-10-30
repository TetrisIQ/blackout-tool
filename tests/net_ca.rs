//! The local CA's name constraint, checked with a real X.509 verifier (`openssl verify`).
//! Lives in its own test binary because spawning a process from the library tests can briefly
//! hold inherited file locks of other tests. Skipped when `openssl` is not installed.

use std::path::Path;
use std::process::Command;

use emergency_kit::netcfg::ca::Ca;

const NOW: u64 = 1_788_000_000;

fn hosts(h: &[&str]) -> Vec<String> {
    h.iter().map(|s| s.to_string()).collect()
}

fn verify(ca_pem: &Path, leaf: &Path) -> (bool, String) {
    let o = Command::new("openssl")
        .args(["verify", "-attime", &NOW.to_string(), "-CAfile"])
        .arg(ca_pem)
        .arg(leaf)
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

#[test]
fn ca_only_vouches_for_the_permitted_hosts() {
    if Command::new("openssl").arg("version").output().is_err() {
        eprintln!("openssl not found, skipping");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let ca = Ca::create(d.path(), &hosts(&["de.wikipedia.org"]), NOW).unwrap();
    ca.ensure_leaf(&hosts(&["de.wikipedia.org"]), NOW).unwrap();
    let (ok, out) = verify(&ca.files.cert(), &ca.files.leaf_cert());
    assert!(ok, "allowed host must verify: {out}");

    // Signed with the very same CA key, but for a name outside the constraint.
    let (pem, _, _) = ca.sign_leaf(&hosts(&["example.com"]), NOW).unwrap();
    let forged = d.path().join("forged.pem");
    std::fs::write(&forged, pem).unwrap();
    let (ok, out) = verify(&ca.files.cert(), &forged);
    assert!(!ok, "a certificate for a forbidden host must not verify: {out}");
    assert!(
        out.to_lowercase().contains("permitted"),
        "rejected for the right reason: {out}"
    );

    // A sibling of a permitted host is outside the constraint too.
    let (pem, _, _) = ca.sign_leaf(&hosts(&["fr.wikipedia.org"]), NOW).unwrap();
    std::fs::write(&forged, pem).unwrap();
    assert!(!verify(&ca.files.cert(), &forged).0);
}
