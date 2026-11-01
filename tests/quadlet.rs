//! The generated Quadlet file must be accepted by Podman's real generator (`quadlet -dryrun`).
//! Nothing is installed: the generator only prints the unit it would create. Skipped when
//! Podman's `quadlet` binary is not installed.

use std::path::Path;
use std::process::Command;

use emergency_kit::catalog::Catalog;
use emergency_kit::install::Paths;
use emergency_kit::install::container::render_quadlet;

fn quadlet_bin() -> Option<&'static str> {
    [
        "/usr/lib/podman/quadlet",
        "/usr/libexec/podman/quadlet",
        "/usr/lib/systemd/system-generators/podman-system-generator",
    ]
    .into_iter()
    .find(|p| Path::new(p).exists())
}

#[test]
fn shipped_container_packages_are_valid_quadlet() {
    let Some(bin) = quadlet_bin() else {
        eprintln!("quadlet not found, skipping");
        return;
    };
    let catalog = Catalog::load(Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog").as_path()).unwrap();
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(d.path().join("data"), d.path().join("units")).unwrap();
    let mut seen = 0;
    for m in catalog.packages().filter(|m| m.service.is_some()) {
        let text = render_quadlet(
            &m.id,
            &m.name,
            m.source.image.as_deref().unwrap(),
            m.service.as_ref().unwrap(),
            &paths,
        )
        .unwrap();
        let dir = d.path().join(&m.id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("kit-{}.container", m.id)), text).unwrap();
        let out = Command::new(bin)
            .args(["-dryrun", "-user"])
            .env("QUADLET_UNIT_DIRS", &dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{}: quadlet rejected the file:\n{stdout}\n{stderr}",
            m.id
        );
        assert!(
            stdout.contains(&format!("kit-{}.service", m.id)),
            "{}: no unit generated:\n{stdout}\n{stderr}",
            m.id
        );
        assert!(
            stdout.contains("127.0.0.1:") && stdout.contains(m.source.image.as_deref().unwrap()),
            "{stdout}"
        );
        let lower = stderr.to_lowercase();
        assert!(
            !lower.contains("unsupported") && !lower.contains("error") && !lower.contains("warning"),
            "{stderr}"
        );
        seen += 1;
    }
    assert!(seen >= 1, "the catalog has no container package");
}
