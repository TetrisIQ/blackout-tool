//! Package catalog: manifests, profiles, loading and cross-package validation.

pub mod http;
pub mod kiwix;
pub mod manifest;
pub mod profile;
pub mod resolve;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub use manifest::{License, Manifest, PackageType};
pub use profile::Profile;

/// One validation problem, optionally tied to the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{}: {}", p.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("cannot read {}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("catalog is invalid:\n{}", .0.iter().map(|i| format!("  - {i}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<Issue>),
    #[error("unknown package {0:?}")]
    UnknownPackage(String),
    #[error("unknown profile {0:?}")]
    UnknownProfile(String),
}

#[derive(Debug, Default)]
pub struct Catalog {
    packages: BTreeMap<String, Manifest>,
    profiles: BTreeMap<String, Profile>,
}

impl Catalog {
    /// Loads `<dir>/**/*.yaml` as manifests and `<dir>/profiles/*.yaml` as profiles.
    /// Every problem in the whole tree is reported at once.
    pub fn load(dir: &Path) -> Result<Self, CatalogError> {
        let mut issues = Vec::new();
        let mut manifests = Vec::new();
        let mut profiles = Vec::new();
        let profiles_dir = dir.join("profiles");

        for path in yaml_files(dir)? {
            let text = std::fs::read_to_string(&path).map_err(|source| CatalogError::Io {
                path: path.clone(),
                source,
            })?;
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let mut fail = |message: String| {
                issues.push(Issue {
                    path: Some(path.clone()),
                    message,
                })
            };
            if path.starts_with(&profiles_dir) {
                match serde_yaml_ng::from_str::<Profile>(&text) {
                    Ok(p) => {
                        if p.id != stem {
                            fail(format!("id {:?} does not match file name {stem:?}", p.id));
                        }
                        p.validate().into_iter().for_each(&mut fail);
                        profiles.push(p);
                    }
                    Err(e) => fail(e.to_string()),
                }
            } else {
                match serde_yaml_ng::from_str::<Manifest>(&text) {
                    Ok(m) => {
                        if m.id != stem {
                            fail(format!("id {:?} does not match file name {stem:?}", m.id));
                        }
                        m.validate().into_iter().for_each(&mut fail);
                        manifests.push(m);
                    }
                    Err(e) => fail(e.to_string()),
                }
            }
        }
        issues.extend(cross_check(&manifests, &profiles));
        if !issues.is_empty() {
            return Err(CatalogError::Invalid(issues));
        }
        Ok(Self::from_valid(manifests, profiles))
    }

    /// Builds a catalog from in-memory parts, running every check `load` does except the
    /// file-name rule.
    pub fn from_parts(manifests: Vec<Manifest>, profiles: Vec<Profile>) -> Result<Self, CatalogError> {
        let mut issues: Vec<Issue> = Vec::new();
        for m in &manifests {
            issues.extend(m.validate().into_iter().map(|e| Issue {
                path: None,
                message: format!("{}: {e}", m.id),
            }));
        }
        for p in &profiles {
            issues.extend(p.validate().into_iter().map(|e| Issue {
                path: None,
                message: format!("profile {}: {e}", p.id),
            }));
        }
        issues.extend(cross_check(&manifests, &profiles));
        if !issues.is_empty() {
            return Err(CatalogError::Invalid(issues));
        }
        Ok(Self::from_valid(manifests, profiles))
    }

    fn from_valid(manifests: Vec<Manifest>, profiles: Vec<Profile>) -> Self {
        Self {
            packages: manifests.into_iter().map(|m| (m.id.clone(), m)).collect(),
            profiles: profiles.into_iter().map(|p| (p.id.clone(), p)).collect(),
        }
    }

    pub fn packages(&self) -> impl Iterator<Item = &Manifest> {
        self.packages.values()
    }

    pub fn profiles(&self) -> impl Iterator<Item = &Profile> {
        self.profiles.values()
    }

    pub fn package(&self, id: &str) -> Result<&Manifest, CatalogError> {
        self.packages
            .get(id)
            .ok_or_else(|| CatalogError::UnknownPackage(id.to_string()))
    }

    pub fn profile(&self, id: &str) -> Result<&Profile, CatalogError> {
        self.profiles
            .get(id)
            .ok_or_else(|| CatalogError::UnknownProfile(id.to_string()))
    }

    /// Expands `ids` with their transitive dependencies. Dependencies come before the
    /// packages that need them, each package appears once, and the order is deterministic.
    pub fn resolve_selection(&self, ids: &[String]) -> Result<Vec<&Manifest>, CatalogError> {
        fn visit<'a>(
            cat: &'a Catalog,
            id: &str,
            seen: &mut HashSet<String>,
            out: &mut Vec<&'a Manifest>,
        ) -> Result<(), CatalogError> {
            if !seen.insert(id.to_string()) {
                return Ok(());
            }
            let m = cat.package(id)?;
            for d in &m.depends_on {
                visit(cat, d, seen, out)?;
            }
            out.push(m);
            Ok(())
        }
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in ids {
            visit(self, id, &mut seen, &mut out)?;
        }
        Ok(out)
    }
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>, CatalogError> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CatalogError> {
        let io = |source| CatalogError::Io {
            path: dir.to_path_buf(),
            source,
        };
        for entry in std::fs::read_dir(dir).map_err(io)? {
            let path = entry.map_err(io)?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "yaml") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn cross_check(manifests: &[Manifest], profiles: &[Profile]) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut issue = |message: String| issues.push(Issue { path: None, message });

    let mut ids: HashMap<&str, usize> = HashMap::new();
    for m in manifests {
        *ids.entry(m.id.as_str()).or_default() += 1;
    }
    let mut dups: Vec<_> = ids.iter().filter(|(_, n)| **n > 1).map(|(id, _)| *id).collect();
    dups.sort_unstable();
    for id in dups {
        issue(format!("package id {id:?} is defined more than once"));
    }

    let deps: HashMap<&str, &[String]> = manifests
        .iter()
        .map(|m| (m.id.as_str(), m.depends_on.as_slice()))
        .collect();
    for m in manifests {
        for d in &m.depends_on {
            if !deps.contains_key(d.as_str()) {
                issue(format!("{}: depends_on unknown package {d:?}", m.id));
            }
        }
    }
    for cycle in find_cycles(&deps) {
        issue(format!("dependency cycle: {}", cycle.join(" -> ")));
    }

    let mut seen_profiles = HashSet::new();
    for p in profiles {
        if !seen_profiles.insert(p.id.as_str()) {
            issue(format!("profile id {:?} is defined more than once", p.id));
        }
        for pkg in &p.packages {
            if !deps.contains_key(pkg.as_str()) {
                issue(format!("profile {}: unknown package {pkg:?}", p.id));
            }
        }
    }
    issues
}

/// Returns one representative path for every dependency cycle (unknown ids are ignored).
fn find_cycles(deps: &HashMap<&str, &[String]>) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Active,
        Done,
    }
    fn dfs<'a>(
        id: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        state: &mut HashMap<&'a str, State>,
        stack: &mut Vec<&'a str>,
        cycles: &mut Vec<Vec<String>>,
    ) {
        state.insert(id, State::Active);
        stack.push(id);
        for d in deps.get(id).copied().unwrap_or_default() {
            let d = d.as_str();
            if !deps.contains_key(d) {
                continue;
            }
            match state.get(d) {
                Some(State::Active) => {
                    let start = stack.iter().position(|s| *s == d).unwrap_or(0);
                    let mut cycle: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
                    cycle.push(d.to_string());
                    cycles.push(cycle);
                }
                Some(State::Done) => {}
                None => dfs(d, deps, state, stack, cycles),
            }
        }
        stack.pop();
        state.insert(id, State::Done);
    }
    let mut ids: Vec<&str> = deps.keys().copied().collect();
    ids.sort_unstable();
    let mut state = HashMap::new();
    let mut cycles = Vec::new();
    for id in ids {
        if !state.contains_key(id) {
            dfs(id, deps, &mut state, &mut Vec::new(), &mut cycles);
        }
    }
    cycles
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use manifest::Source;

    const SHA: &str = "0e281a87af92a798297fc671f8b37004cb7874ac051e13876b7335be96649e59";

    pub fn pkg(id: &str, deps: &[&str]) -> Manifest {
        Manifest {
            id: id.into(),
            name: id.into(),
            description: "d".into(),
            category: "test".into(),
            kind: PackageType::Files,
            source: Source {
                url: Some(format!("https://example.org/{id}")),
                ..Default::default()
            },
            sha256: Some(SHA.into()),
            size_bytes: Some(100),
            license: License {
                spdx: "CC0-1.0".into(),
                url: None,
                attribution: None,
            },
            languages: vec![],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            post_install: vec![],
            mirror_domains: vec![],
        }
    }

    fn profile(id: &str, pkgs: &[&str]) -> Profile {
        Profile {
            id: id.into(),
            name: id.into(),
            description: "d".into(),
            budget_bytes: 1000,
            packages: pkgs.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn messages(r: Result<Catalog, CatalogError>) -> Vec<String> {
        match r {
            Err(CatalogError::Invalid(i)) => i.iter().map(|i| i.message.clone()).collect(),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    fn ids(v: Vec<&Manifest>) -> Vec<&str> {
        v.into_iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn valid_catalog() {
        let c =
            Catalog::from_parts(vec![pkg("a", &["b"]), pkg("b", &[])], vec![profile("p", &["a"])]).unwrap();
        assert_eq!(c.packages().count(), 2);
        assert_eq!(c.profile("p").unwrap().packages, vec!["a"]);
        assert!(matches!(c.package("zzz"), Err(CatalogError::UnknownPackage(_))));
        assert!(matches!(c.profile("zzz"), Err(CatalogError::UnknownProfile(_))));
    }

    #[test]
    fn duplicate_ids_rejected() {
        let m = messages(Catalog::from_parts(vec![pkg("a", &[]), pkg("a", &[])], vec![]));
        assert!(m.iter().any(|m| m.contains("more than once")), "{m:?}");
    }

    #[test]
    fn unknown_dependency_rejected() {
        let m = messages(Catalog::from_parts(vec![pkg("a", &["ghost"])], vec![]));
        assert!(m.iter().any(|m| m.contains("ghost")));
    }

    #[test]
    fn cycles_rejected() {
        let m = messages(Catalog::from_parts(
            vec![pkg("a", &["b"]), pkg("b", &["c"]), pkg("c", &["a"])],
            vec![],
        ));
        assert!(m.iter().any(|m| m.contains("a -> b -> c -> a")), "{m:?}");
    }

    #[test]
    fn profile_with_unknown_package_rejected() {
        let m = messages(Catalog::from_parts(
            vec![pkg("a", &[])],
            vec![profile("p", &["nope"])],
        ));
        assert!(m.iter().any(|m| m.contains("profile p") && m.contains("nope")));
    }

    #[test]
    fn invalid_manifest_is_reported_with_its_id() {
        let mut bad = pkg("a", &[]);
        bad.license.spdx.clear();
        let m = messages(Catalog::from_parts(vec![bad], vec![]));
        assert!(m.iter().any(|m| m.starts_with("a: license.spdx")));
    }

    #[test]
    fn selection_puts_dependencies_first_and_dedupes() {
        let c = Catalog::from_parts(
            vec![
                pkg("app", &["lib", "data"]),
                pkg("lib", &["data"]),
                pkg("data", &[]),
                pkg("solo", &[]),
            ],
            vec![],
        )
        .unwrap();
        let sel = c
            .resolve_selection(&["app".into(), "solo".into(), "lib".into()])
            .unwrap();
        assert_eq!(ids(sel), vec!["data", "lib", "app", "solo"]);
        assert!(matches!(
            c.resolve_selection(&["ghost".into()]),
            Err(CatalogError::UnknownPackage(_))
        ));
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn manifest_yaml(id: &str) -> String {
        format!(
            "id: {id}\nname: {id}\ndescription: d\ncategory: c\ntype: files\n\
             source: {{url: 'https://example.org/{id}'}}\nsha256: {SHA}\nsize_bytes: 5\n\
             license: {{spdx: CC0-1.0}}\n"
        )
    }

    #[test]
    fn load_reads_nested_manifests_and_profiles() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "games/a.yaml", &manifest_yaml("a"));
        write(d.path(), "b.yaml", &manifest_yaml("b"));
        write(d.path(), "notes.txt", "ignored");
        write(
            d.path(),
            "profiles/mini.yaml",
            "id: mini\nname: Mini\ndescription: d\nbudget_bytes: 10\npackages: [a, b]\n",
        );
        let c = Catalog::load(d.path()).unwrap();
        assert_eq!(
            c.packages().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(c.profiles().count(), 1);
    }

    #[test]
    fn load_reports_all_errors_with_paths() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.yaml", &manifest_yaml("a").replace("CC0-1.0", "''"));
        write(d.path(), "b.yaml", "id: b\nnonsense: true\n");
        write(d.path(), "c.yaml", &manifest_yaml("other"));
        let Err(CatalogError::Invalid(issues)) = Catalog::load(d.path()) else {
            panic!("expected Invalid");
        };
        let text = issues
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("a.yaml: license.spdx"), "{text}");
        assert!(text.contains("b.yaml"), "{text}");
        assert!(
            text.contains("c.yaml: id \"other\" does not match file name"),
            "{text}"
        );
    }

    #[test]
    fn load_missing_dir_is_an_io_error() {
        assert!(matches!(
            Catalog::load(Path::new("/nonexistent/catalog")),
            Err(CatalogError::Io { .. })
        ));
    }

    #[test]
    fn shipped_catalog_is_valid() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("catalog");
        let c = Catalog::load(&dir).unwrap();
        assert!(c.packages().count() > 0);
        for name in ["mini", "standard", "full"] {
            c.profile(name).unwrap();
        }
    }
}
