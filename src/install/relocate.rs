//! Moves the data directory (downloads, library, services, state, portal, net) to another place,
//! e.g. an external disk, and points everything that mentions the old path at the new one.
//!
//! Order: stop our units, move (rename on the same filesystem, else copy keeping hard links),
//! rewrite the paths in the units, Quadlet files, Caddyfile and state, remember the new location,
//! start again. Until the new location is remembered, a failure puts everything back.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};

use super::state::StateGuard;
use super::tools::{require, s};
use super::{Ctx, InstallError, io_err, reconcile, service};
use crate::datadir;
use crate::diskspace::FreeSpace;

/// Characters that would need quoting in a unit, Quadlet or Caddyfile. Rewriting paths in those
/// files is a plain text replacement, so the new location must not contain any of them (a
/// symlink with a plain name to the disk works).
const UNSAFE: &[char] = &[
    ' ', '\t', '\n', '\r', '"', '\'', '\\', '%', '$', ':', '`', ';', '&', '|', '<', '>', '(', ')', '{', '}',
];

pub fn check_target(old: &Path, new: &Path) -> Result<(), InstallError> {
    let bad = |why: String| Err(InstallError::BadPath(format!("{}: {why}", new.display())));
    let Some(text) = new.to_str() else {
        return bad("not valid UTF-8".into());
    };
    if !new.is_absolute() {
        return bad("must be an absolute path".into());
    }
    if new
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return bad("must not contain `..`".into());
    }
    if let Some(c) = text.chars().find(|c| UNSAFE.contains(c)) {
        return bad(format!(
            "contains {c:?}; units and Quadlet files cannot hold it unquoted. Use a path without \
             spaces or special characters, e.g. a symlink like ~/kit-data to the disk"
        ));
    }
    if new == old {
        return bad("is the current location already".into());
    }
    if new.starts_with(old) || old.starts_with(new) {
        return bad("must not lie inside the current location, nor contain it".into());
    }
    match fs::read_dir(new) {
        Ok(it) => {
            if it.count() > 0 {
                return bad("exists and is not empty; choose an empty or new directory".into());
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("cannot read", new)(e)),
    }
}

/// Bytes on disk below `dir`; a file with several hard links counts once.
pub fn tree_size(dir: &Path) -> std::io::Result<u64> {
    fn walk(dir: &Path, seen: &mut std::collections::HashSet<(u64, u64)>) -> std::io::Result<u64> {
        let mut total = 0;
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                total += walk(&entry.path(), seen)?;
            } else if meta.is_file() && (meta.nlink() < 2 || seen.insert((meta.dev(), meta.ino()))) {
                total += meta.len();
            }
        }
        Ok(total)
    }
    walk(dir, &mut std::collections::HashSet::new())
}

fn same_filesystem(old: &Path, new: &Path) -> bool {
    let Some(existing) = new.ancestors().find(|p| p.exists()) else {
        return false;
    };
    matches!((fs::metadata(old), fs::metadata(existing)), (Ok(a), Ok(b)) if a.dev() == b.dev())
}

/// Copies `src` to `dst` keeping permissions, modification times, symlinks and the hard links
/// between files (`downloads/` and `library/` share them).
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fn walk(src: &Path, dst: &Path, links: &mut HashMap<(u64, u64), PathBuf>) -> std::io::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let from = entry.path();
            let to = dst.join(entry.file_name());
            let meta = fs::symlink_metadata(&from)?;
            let kind = meta.file_type();
            if kind.is_dir() {
                walk(&from, &to, links)?;
            } else if kind.is_symlink() {
                symlink(fs::read_link(&from)?, &to)?;
            } else if kind.is_file() {
                let key = (meta.dev(), meta.ino());
                if meta.nlink() > 1
                    && let Some(first) = links.get(&key)
                {
                    fs::hard_link(first, &to)?;
                    continue;
                }
                fs::copy(&from, &to)?;
                if let Ok(t) = meta.modified() {
                    fs::File::options().write(true).open(&to)?.set_modified(t)?;
                }
                if meta.nlink() > 1 {
                    links.insert(key, to);
                }
            } else {
                return Err(std::io::Error::other(format!(
                    "{} is neither a file, a directory nor a symlink",
                    from.display()
                )));
            }
        }
        fs::set_permissions(dst, fs::metadata(src)?.permissions())
    }
    walk(src, dst, &mut HashMap::new())
}

/// Files outside the data directory that mention it, plus the Caddyfile inside.
fn referencing_files(ctx: &Ctx, data_dir: &Path) -> Vec<PathBuf> {
    let mut files = vec![
        ctx.paths.unit_file(),
        ctx.paths.unit_dir.join("kit-caddy.service"),
    ];
    files.push(data_dir.join("net/Caddyfile"));
    if let Ok(entries) = fs::read_dir(&ctx.paths.quadlet_dir) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("kit-") && name.ends_with(".container") {
                files.push(e.path());
            }
        }
    }
    files
}

/// Replaces `from` with `to` in every file that kit wrote (marked first line). Returns the files
/// that changed, so the caller can undo it.
fn rewrite_references(files: &[PathBuf], from: &Path, to: &Path) -> Result<Vec<PathBuf>, InstallError> {
    let (from, to) = (s(from), s(to));
    let mut changed = Vec::new();
    for f in files {
        let Ok(text) = fs::read_to_string(f) else {
            continue;
        };
        let first = text.lines().next().unwrap_or("");
        let ours = first.starts_with("# Managed by kit");
        // Unit, Quadlet and Caddyfile all start with kit's marker; anything else is not ours.
        if !ours || !text.contains(from) {
            continue;
        }
        reconcile::write_if_changed(f, &text.replace(from, to))?;
        changed.push(f.clone());
    }
    Ok(changed)
}

fn systemctl_user(ctx: &Ctx, args: &[&str]) -> Option<bool> {
    let tool = require("systemctl", &ctx.search_path).ok()?;
    Some(reconcile::systemctl(ctx, s(&tool), args).ok()?.success)
}

/// Units that keep files open under the data directory.
fn our_units(ctx: &Ctx, state: &super::state::State) -> Vec<String> {
    let mut units = Vec::new();
    for (name, present) in [
        (service::UNIT_NAME.to_string(), ctx.paths.unit_file().exists()),
        (
            "kit-caddy.service".to_string(),
            ctx.paths.unit_dir.join("kit-caddy.service").exists(),
        ),
    ] {
        if present {
            units.push(name);
        }
    }
    units.extend(
        state
            .packages
            .values()
            .filter_map(|r| r.service.as_ref())
            .map(|sv| sv.unit.clone()),
    );
    units
}

#[derive(Debug)]
pub struct Moved {
    /// What the user should know (what was left, what to run next).
    pub notes: Vec<String>,
}

/// Moves the data directory of `ctx` to `new` and remembers `new` in `config_dir`.
pub fn relocate(
    ctx: &Ctx,
    new: &Path,
    config_dir: &Path,
    space: &dyn FreeSpace,
    progress: &mut dyn FnMut(&str),
) -> Result<Moved, InstallError> {
    let old = ctx.paths.data_dir.clone();
    check_target(&old, new)?;
    if !old.exists() {
        return Err(InstallError::State(format!(
            "{} does not exist; nothing to move. Use `kit data-dir set` without --move",
            old.display()
        )));
    }
    let renameable = same_filesystem(&old, new);
    if !renameable {
        let need = tree_size(&old).map_err(io_err("cannot measure", &old))?;
        let free = space
            .available(new)
            .map_err(io_err("cannot check free space of", new))?;
        if free < need {
            return Err(InstallError::State(format!(
                "not enough space at {}: {need} bytes needed, {free} free",
                new.display()
            )));
        }
    }

    // Held until the new location works: nothing else installs while the files move.
    let guard = StateGuard::open(&ctx.paths)?;
    let units = our_units(ctx, &guard.state);
    let was_active: Vec<&String> = units
        .iter()
        .filter(|u| systemctl_user(ctx, &["is-active", "--quiet", u]) == Some(true))
        .collect();
    for u in &was_active {
        progress(&format!("stopping {u}"));
        systemctl_user(ctx, &["stop", u]);
    }
    let restart = || {
        for u in &was_active {
            systemctl_user(ctx, &["start", u]);
        }
    };

    // 1. Move.
    let created = !new.exists();
    if renameable {
        progress(&format!("moving {} to {}", old.display(), new.display()));
        if let Some(parent) = new.parent() {
            fs::create_dir_all(parent).map_err(io_err("cannot create", parent))?;
        }
        if let Err(e) = fs::rename(&old, new) {
            restart();
            return Err(io_err("cannot move", &old)(e));
        }
    } else {
        progress(&format!(
            "copying {} to {} (this can take a while)",
            old.display(),
            new.display()
        ));
        if let Err(e) = copy_tree(&old, new) {
            if created {
                let _ = fs::remove_dir_all(new);
            }
            restart();
            return Err(io_err("cannot copy to", new)(e));
        }
    }
    let undo_transfer = || {
        if renameable {
            let _ = fs::rename(new, &old);
        } else if created {
            let _ = fs::remove_dir_all(new);
        }
    };

    // 2. Paths in units, Quadlet files and the Caddyfile.
    let mut new_paths = ctx.paths.clone();
    new_paths.data_dir = new.to_path_buf();
    if let Ok(rest) = ctx.paths.quadlet_dir.strip_prefix(&old) {
        new_paths.quadlet_dir = new.join(rest);
    }
    let old_files = referencing_files(
        ctx,
        &if renameable {
            new.to_path_buf()
        } else {
            old.clone()
        },
    );
    let changed = match rewrite_references(&old_files, &old, new) {
        Ok(c) => c,
        Err(e) => {
            undo_transfer();
            restart();
            return Err(e);
        }
    };
    let undo = |e: InstallError| {
        let _ = rewrite_references(&changed, new, &old);
        undo_transfer();
        restart();
        e
    };

    // 3. State: where the downloads were linked from. Taken from the new place.
    drop(guard);
    let mut st = match StateGuard::open(&new_paths) {
        Ok(g) => g,
        Err(e) => return Err(undo(e)),
    };
    for rec in st.packages.values_mut() {
        for f in &mut rec.files {
            if let Some(src) = &f.source
                && let Ok(rest) = src.strip_prefix(&old)
            {
                f.source = Some(new.join(rest));
            }
        }
    }
    if let Err(e) = st.save() {
        return Err(undo(e));
    }

    // 4. Remember the place. From here on the move counts as done.
    if let Err(e) = datadir::save(config_dir, new) {
        drop(st);
        return Err(undo(InstallError::State(format!(
            "cannot save the new location: {e}"
        ))));
    }

    // 5. Rebuild what is derived from the state and start again.
    let mut notes = Vec::new();
    let new_ctx = Ctx {
        paths: new_paths.clone(),
        runner: ctx.runner,
        probe: ctx.probe,
        search_path: ctx.search_path.clone(),
        port: ctx.port,
        user: ctx.user.clone(),
        start_wait: ctx.start_wait,
        now: ctx.now,
        check_port: false,
    };
    if systemctl_user(&new_ctx, &["daemon-reload"]) == Some(false) {
        notes.push("systemctl --user daemon-reload failed; run it yourself".into());
    }
    if let Err(e) = reconcile::reconcile(&new_ctx, &st.state) {
        notes.push(format!(
            "the content server could not be rebuilt: {e}; run `kit install` again"
        ));
    }
    for u in was_active.iter().filter(|u| u.as_str() != service::UNIT_NAME) {
        if systemctl_user(&new_ctx, &["start", u]) == Some(false) {
            notes.push(format!("{u} did not start; see: journalctl --user -u {u}"));
        }
    }
    drop(st);

    // 6. A copy leaves the old files behind; remove them only now that the new place works.
    if !renameable {
        progress(&format!("removing {}", old.display()));
        if let Err(e) = fs::remove_dir_all(&old) {
            notes.push(format!(
                "the copy is complete, but {} could not be removed ({e}); delete it by hand",
                old.display()
            ));
        }
    }
    if new.join("net/state.json").exists() {
        notes.push(
            "local HTTPS (kit net) was on: run `kit net enable` once so the hosts, certificate and service stay in sync"
                .into(),
        );
    }
    Ok(Moved { notes })
}

#[cfg(test)]
mod tests {
    use super::super::runner::CmdOutput;
    use super::super::runner::testing::FakeRunner;
    use super::super::state::{StateGuard, sample_record};
    use super::super::{Paths, container};
    use super::*;
    use crate::catalog::http::testing::FakeHttp;
    use std::time::Duration;

    struct Plenty;
    impl FreeSpace for Plenty {
        fn available(&self, _: &Path) -> std::io::Result<u64> {
            Ok(u64::MAX)
        }
    }

    fn ctx_for<'a>(root: &Path, runner: &'a FakeRunner, probe: &'a FakeHttp) -> Ctx<'a> {
        let paths = Paths::new(root.join("data"), root.join("units"))
            .unwrap()
            .with_quadlet_dir(root.join("quadlets"))
            .unwrap();
        Ctx {
            paths,
            runner,
            probe,
            search_path: vec![],
            port: 6589,
            user: None,
            start_wait: Duration::ZERO,
            now: 1,
            check_port: false,
        }
    }

    #[test]
    fn rejects_paths_the_rewrite_cannot_handle() {
        let old = Path::new("/home/u/.local/share/kit");
        for bad in [
            "relative/dir",
            "/mnt/My Disk/kit",
            "/mnt/a:b",
            "/mnt/../etc",
            "/home/u/.local/share/kit/sub",
            "/home/u/.local/share",
            "/home/u/.local/share/kit",
        ] {
            assert!(check_target(old, Path::new(bad)).is_err(), "{bad}");
        }
        assert!(check_target(old, Path::new("/run/media/u/disk/kit")).is_ok());
    }

    #[test]
    fn refuses_a_target_that_is_not_empty() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("x"), "1").unwrap();
        let err = check_target(Path::new("/nonexistent/old"), d.path()).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }

    #[test]
    fn copy_keeps_hard_links_symlinks_and_content() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        fs::create_dir_all(src.join("downloads/a")).unwrap();
        fs::create_dir_all(src.join("library")).unwrap();
        fs::write(src.join("downloads/a/f.zim"), "0123456789").unwrap();
        fs::hard_link(src.join("downloads/a/f.zim"), src.join("library/f.zim")).unwrap();
        symlink("library/f.zim", src.join("link")).unwrap();
        let dst = d.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        let (a, b) = (
            fs::metadata(dst.join("downloads/a/f.zim")).unwrap(),
            fs::metadata(dst.join("library/f.zim")).unwrap(),
        );
        assert_eq!(
            (a.ino(), a.nlink()),
            (b.ino(), 2),
            "still one file with two names"
        );
        assert_eq!(fs::read_to_string(dst.join("link")).unwrap(), "0123456789");
        assert_eq!(
            fs::read_link(dst.join("link")).unwrap(),
            Path::new("library/f.zim")
        );
        assert_eq!(tree_size(&src).unwrap(), 10, "a hard-linked file counts once");
    }

    #[test]
    fn move_rewrites_state_units_quadlets_and_remembers_the_place() {
        let d = tempfile::tempdir().unwrap();
        let runner = super::super::runner::testing::system(true);
        let probe = FakeHttp::default();
        let bin = d.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        for tool in ["kiwix-manage", "kiwix-serve", "systemctl"] {
            super::super::tools::fake_exe(&bin, tool);
        }
        let mut ctx = ctx_for(d.path(), &runner, &probe);
        ctx.search_path = vec![bin];
        let old = ctx.paths.data_dir.clone();

        // Something installed: a record that points at its download, a Quadlet, the Caddyfile.
        {
            let mut g = StateGuard::open(&ctx.paths).unwrap();
            let mut rec = sample_record("a", "a.zim", 3);
            rec.files[0].source = Some(old.join("downloads/a/a.zim"));
            g.packages.insert("a".into(), rec);
            g.save().unwrap();
        }
        fs::create_dir_all(old.join("downloads/a")).unwrap();
        fs::write(old.join("downloads/a/a.zim"), "abc").unwrap();
        fs::create_dir_all(old.join("net")).unwrap();
        fs::write(old.join("net/state.json"), "{}").unwrap();
        fs::create_dir_all(&ctx.paths.quadlet_dir).unwrap();
        let quadlet = ctx.paths.quadlet_dir.join("kit-navidrome.container");
        fs::write(
            &quadlet,
            format!(
                "{}\n[Container]\nVolume={}/services/navidrome/data:/data\n",
                container::MARKER,
                old.display()
            ),
        )
        .unwrap();
        let foreign = ctx.paths.quadlet_dir.join("kit-other.container");
        fs::write(&foreign, format!("[Container]\nVolume={}/x:/x\n", old.display())).unwrap();
        fs::write(
            old.join("net/Caddyfile"),
            format!("# Managed by kit (emergency-kit). Remove with: kit net disable\ntls \"{}/net/ca/leaf.pem\"\n", old.display()),
        )
        .unwrap();

        let new = d.path().join("disk/kit");
        let config = d.path().join("config");
        let mut said = Vec::new();
        let moved = relocate(&ctx, &new, &config, &Plenty, &mut |m| said.push(m.to_string())).unwrap();

        assert!(!old.exists() && new.join("downloads/a/a.zim").exists());
        let q = fs::read_to_string(ctx.paths.quadlet_dir.join("kit-navidrome.container")).unwrap();
        assert!(
            q.contains(&format!("Volume={}/services/navidrome/data:/data", new.display())),
            "{q}"
        );
        assert!(
            fs::read_to_string(&foreign)
                .unwrap()
                .contains(&old.display().to_string()),
            "files kit did not write stay untouched"
        );
        assert!(
            fs::read_to_string(new.join("net/Caddyfile"))
                .unwrap()
                .contains(&format!("{}/net/ca/leaf.pem", new.display()))
        );
        let mut np = ctx.paths.clone();
        np.data_dir = new.clone();
        let g = StateGuard::open(&np).unwrap();
        assert_eq!(
            g.packages["a"].files[0].source.as_deref(),
            Some(new.join("downloads/a/a.zim").as_path())
        );
        drop(g);
        assert_eq!(datadir::load(&config).unwrap().as_deref(), Some(new.as_path()));
        assert!(
            !moved.notes.iter().any(|n| n.contains("could not")),
            "{:?}",
            moved.notes
        );
        assert!(
            moved.notes.iter().any(|n| n.contains("kit net enable")),
            "{:?}",
            moved.notes
        );
    }

    #[test]
    fn a_failed_move_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|_, _| Ok(CmdOutput::ok()));
        let probe = FakeHttp::default();
        let ctx = ctx_for(d.path(), &runner, &probe);
        fs::create_dir_all(ctx.paths.data_dir.join("downloads")).unwrap();
        let new = d.path().join("elsewhere");
        fs::create_dir_all(&new).unwrap();
        fs::write(new.join("keep"), "1").unwrap();
        let err = relocate(&ctx, &new, &d.path().join("config"), &Plenty, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
        assert!(ctx.paths.data_dir.join("downloads").exists());
        assert!(datadir::load(&d.path().join("config")).unwrap().is_none());
    }
}
