//! Finding the external programs the installers need, with an actionable error when missing.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::InstallError;

pub fn find_in_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(name)).find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Distro-specific hint for installing `kiwix-tools`, from the contents of `/etc/os-release`.
pub fn install_hint(os_release: &str) -> String {
    let ids: Vec<String> = os_release
        .lines()
        .filter_map(|l| l.strip_prefix("ID=").or_else(|| l.strip_prefix("ID_LIKE=")))
        .flat_map(|v| {
            v.trim_matches('"')
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    let has = |s: &str| ids.iter().any(|i| i == s);
    if has("debian") || has("ubuntu") {
        "sudo apt install kiwix-tools".into()
    } else if has("arch") {
        "sudo pacman -S kiwix-tools".into()
    } else if has("fedora") || has("rhel") {
        "sudo dnf install kiwix-tools".into()
    } else {
        "install the kiwix-tools package from your distribution (https://kiwix.org)".into()
    }
}

pub fn require(name: &str, dirs: &[PathBuf]) -> Result<PathBuf, InstallError> {
    find_in_path(name, dirs).ok_or_else(|| {
        let hint = if name.starts_with("kiwix") {
            format!(
                "Install it with: {}",
                install_hint(&std::fs::read_to_string("/etc/os-release").unwrap_or_default())
            )
        } else {
            "It must be available on PATH.".to_string()
        };
        InstallError::MissingTool {
            tool: name.to_string(),
            hint,
        }
    })
}

/// Path as `&str` for passing to a command; paths in `Paths` are validated as UTF-8 up front.
pub fn s(p: &Path) -> &str {
    p.to_str().unwrap_or("")
}

#[cfg(test)]
pub(crate) fn fake_exe(dir: &Path, name: &str) {
    let p = dir.join(name);
    std::fs::write(&p, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_executables_only() {
        let d = tempfile::tempdir().unwrap();
        fake_exe(d.path(), "kiwix-serve");
        std::fs::write(d.path().join("not-exec"), "x").unwrap();
        let dirs = vec![PathBuf::from("/nonexistent"), d.path().to_path_buf()];
        assert_eq!(
            find_in_path("kiwix-serve", &dirs),
            Some(d.path().join("kiwix-serve"))
        );
        assert_eq!(find_in_path("not-exec", &dirs), None);
        assert_eq!(find_in_path("missing", &dirs), None);
    }

    #[test]
    fn hints_per_distro() {
        assert!(install_hint("ID=ubuntu\nID_LIKE=debian\n").contains("apt install"));
        assert!(install_hint("ID=cachyos\nID_LIKE=\"arch\"\n").contains("pacman -S"));
        assert!(install_hint("ID=fedora\n").contains("dnf"));
        assert!(install_hint("ID=plan9\n").contains("distribution"));
        assert!(install_hint("").contains("distribution"));
    }

    #[test]
    fn missing_tool_error_names_the_tool_and_the_fix() {
        let e = require("kiwix-manage", &[]).unwrap_err().to_string();
        assert!(e.contains("kiwix-manage") && e.contains("kiwix-tools"), "{e}");
    }
}
