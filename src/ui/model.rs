//! What the download manager shows and decides, independent of any toolkit: the package list,
//! the selection against the installed state, and the numbers for the footer bar.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::catalog::PackageType;

/// Display order and names of the catalog categories; unknown ones follow, capitalised.
pub const CATEGORIES: &[(&str, &str)] = &[
    ("wikipedia", "Wikipedia"),
    ("survival", "Survival and medical"),
    ("reference", "Reference"),
    ("services", "Apps"),
    ("video", "Films"),
    ("music", "Music"),
    ("books", "Books"),
    ("audiobooks", "Audiobooks"),
    ("maps", "Maps"),
    ("ai", "Local assistant"),
    ("games", "Games"),
];

/// Wikipedia editions in the order the wizard offers them: (suffix, name, short name, text).
pub const EDITIONS: [(&str, &str, &str, &str); 3] = [
    (
        "mini",
        "Introductions",
        "Intros",
        "The opening paragraph and infobox of every article. Quick to download, good for looking things up.",
    ),
    (
        "nopic",
        "Full text",
        "Text",
        "Every article in full, without images. The best balance of size and content.",
    ),
    (
        "maxi",
        "Full with images",
        "Images",
        "Every article with all images. Closest to the real thing, and by far the largest.",
    ),
];

pub fn category_label(id: &str) -> String {
    CATEGORIES.iter().find(|(c, _)| *c == id).map_or_else(
        || {
            let mut cs = id.chars();
            cs.next()
                .map_or(String::new(), |f| f.to_uppercase().chain(cs).collect())
        },
        |(_, l)| (*l).to_string(),
    )
}

pub fn language_name(code: &str) -> String {
    match code.to_ascii_uppercase().as_str() {
        "EN" => "English",
        "DE" => "Deutsch",
        "FR" => "Français",
        "ES" => "Español",
        "IT" => "Italiano",
        "NL" => "Nederlands",
        "PL" => "Polski",
        other => return other.to_string(),
    }
    .to_string()
}

/// Decimal units, as the design shows them: `2.4 GB`, `120 MB`, `75 GB`.
/// SPDX ids as people read them: `LicenseRef-Public-Domain-US` becomes `Public-Domain-US`.
pub fn license_label(spdx: &str) -> &str {
    spdx.strip_prefix("LicenseRef-").unwrap_or(spdx)
}

pub fn fmt_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{} GB", gb.round() as u64)
    } else if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", ((gb * 1000.0).round() as u64).max(1))
    }
}

#[derive(Debug, Clone)]
pub struct Pkg {
    pub id: String,
    pub name: String,
    pub desc: String,
    pub category: String,
    pub kind: PackageType,
    pub bytes: u64,
    pub license: String,
    /// Upper-case language codes; empty for language-neutral content.
    pub langs: Vec<String>,
    pub needs: Vec<String>,
    /// Version of the newest upstream release (Kiwix: the catalog date).
    pub latest: Option<String>,
    /// File name inside the downloads directory, if the package downloads one file.
    pub file: Option<String>,
    pub sha256: Option<String>,
    /// Name the package is served under (`jellyfin.kit.local`), for services.
    pub host: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProfileDef {
    pub id: String,
    pub name: String,
    pub description: String,
    pub ids: Vec<String>,
    pub budget_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Installed {
    pub version: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Downloading,
    WillRemove,
    Update,
    Installed,
    WillDownload,
    None,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Downloading => "Downloading",
            Self::WillRemove => "Will remove",
            Self::Update => "Update",
            Self::Installed => "Installed",
            Self::WillDownload => "Will download",
            Self::None => "",
        }
    }
}

/// What pressing "Download and install" would do.
#[derive(Debug, Default, Clone)]
pub struct Diff {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub add_bytes: u64,
    pub remove_bytes: u64,
    /// Installed bytes that stay.
    pub keep_bytes: u64,
}

pub struct Workspace {
    pub pkgs: Vec<Pkg>,
    index: HashMap<String, usize>,
    pub profiles: Vec<ProfileDef>,
    pub sel: BTreeSet<String>,
    pub installed: BTreeMap<String, Installed>,
    /// `custom` or a profile id.
    pub profile: String,
    pub langs: BTreeSet<String>,
    /// Size of the drive that holds the data directory, and what is free on it now.
    pub drive_bytes: u64,
    pub free_bytes: u64,
}

impl Workspace {
    pub fn new(pkgs: Vec<Pkg>, profiles: Vec<ProfileDef>) -> Self {
        let index = pkgs.iter().enumerate().map(|(i, p)| (p.id.clone(), i)).collect();
        Self {
            pkgs,
            index,
            profiles,
            sel: BTreeSet::new(),
            installed: BTreeMap::new(),
            profile: "custom".into(),
            langs: BTreeSet::new(),
            drive_bytes: 0,
            free_bytes: 0,
        }
    }

    pub fn pkg(&self, id: &str) -> Option<&Pkg> {
        self.index.get(id).map(|&i| &self.pkgs[i])
    }

    /// Every language any package declares, sorted.
    pub fn all_langs(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.pkgs.iter().flat_map(|p| &p.langs).collect();
        set.into_iter().cloned().collect()
    }

    /// Language-neutral packages and those in a chosen language are shown.
    pub fn lang_ok(&self, p: &Pkg) -> bool {
        p.langs.is_empty() || p.langs.iter().any(|l| self.langs.contains(l))
    }

    pub fn categories(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = CATEGORIES
            .iter()
            .filter(|(c, _)| self.pkgs.iter().any(|p| p.category == *c))
            .map(|(c, l)| ((*c).to_string(), (*l).to_string()))
            .collect();
        let known: BTreeSet<&str> = CATEGORIES.iter().map(|(c, _)| *c).collect();
        let extra: BTreeSet<&String> = self
            .pkgs
            .iter()
            .map(|p| &p.category)
            .filter(|c| !known.contains(c.as_str()))
            .collect();
        out.extend(extra.into_iter().map(|c| (c.clone(), category_label(c))));
        out
    }

    /// Selecting a package also selects what it needs (a film needs the player).
    pub fn toggle(&mut self, id: &str) {
        if !self.sel.remove(id) {
            self.add_with_needs(id);
        }
        self.profile = "custom".into();
    }

    fn add_with_needs(&mut self, id: &str) {
        let mut stack = vec![id.to_string()];
        while let Some(i) = stack.pop() {
            if self.pkg(&i).is_none() || !self.sel.insert(i.clone()) {
                continue;
            }
            stack.extend(self.pkg(&i).map(|p| p.needs.clone()).unwrap_or_default());
        }
    }

    pub fn apply_profile(&mut self, id: &str) {
        if id == "custom" {
            self.profile = "custom".into();
            return;
        }
        let Some(def) = self.profiles.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        // The profile decides the languages: everything it contains is shown and selected, and
        // what is already installed keeps its language visible.
        self.sel.clear();
        for pid in &def.ids {
            self.add_with_needs(pid);
        }
        let installed: Vec<String> = self.installed.keys().cloned().collect();
        let langs: BTreeSet<String> = self
            .sel
            .iter()
            .chain(&installed)
            .filter_map(|i| self.pkg(i))
            .flat_map(|p| p.langs.clone())
            .collect();
        self.langs = langs;
        self.profile = id.to_string();
    }

    /// Dropping a language drops its not-yet-installed packages from the selection.
    pub fn set_langs(&mut self, langs: BTreeSet<String>) {
        self.langs = langs;
        let hidden: Vec<String> = self
            .pkgs
            .iter()
            .filter(|p| !self.lang_ok(p) && !self.installed.contains_key(&p.id))
            .map(|p| p.id.clone())
            .collect();
        let before = self.sel.len();
        for id in hidden {
            self.sel.remove(&id);
        }
        if self.sel.len() != before {
            self.profile = "custom".into();
        }
    }

    pub fn update_available(&self, id: &str) -> Option<&str> {
        let have = self.installed.get(id)?.version.as_deref()?;
        let latest = self.pkg(id)?.latest.as_deref()?;
        (latest > have).then_some(latest)
    }

    pub fn updates(&self) -> Vec<&Pkg> {
        self.pkgs
            .iter()
            .filter(|p| self.update_available(&p.id).is_some())
            .collect()
    }

    pub fn status(&self, id: &str, queued: bool) -> Status {
        let installed = self.installed.contains_key(id);
        let selected = self.sel.contains(id);
        if queued {
            Status::Downloading
        } else if installed && !selected {
            Status::WillRemove
        } else if installed && self.update_available(id).is_some() {
            Status::Update
        } else if installed {
            Status::Installed
        } else if selected {
            Status::WillDownload
        } else {
            Status::None
        }
    }

    /// `queued` tells which packages are already downloading, so they are not added twice.
    pub fn diff(&self, queued: &dyn Fn(&str) -> bool) -> Diff {
        let mut d = Diff::default();
        for p in &self.pkgs {
            let installed = self.installed.get(&p.id);
            let selected = self.sel.contains(&p.id);
            match (installed, selected) {
                (None, true) if !queued(&p.id) => {
                    d.add.push(p.id.clone());
                    d.add_bytes += p.bytes;
                }
                (Some(i), false) => {
                    d.remove.push(p.id.clone());
                    d.remove_bytes += i.bytes.max(p.bytes);
                }
                (Some(i), true) => d.keep_bytes += i.bytes.max(p.bytes),
                _ => {}
            }
        }
        d
    }

    pub fn installed_bytes(&self) -> u64 {
        self.installed
            .iter()
            .map(|(id, i)| {
                if i.bytes > 0 {
                    i.bytes
                } else {
                    self.pkg(id).map_or(0, |p| p.bytes)
                }
            })
            .sum()
    }

    /// Free space after the diff is applied; negative when it does not fit.
    pub fn free_after(&self, d: &Diff) -> i128 {
        i128::from(self.free_bytes) + i128::from(d.remove_bytes) - i128::from(d.add_bytes)
    }

    pub fn sum(&self, ids: impl IntoIterator<Item = impl AsRef<str>>) -> u64 {
        ids.into_iter()
            .filter_map(|i| self.pkg(i.as_ref()))
            .map(|p| p.bytes)
            .sum()
    }

    /// Wikipedia packages as `(language code, [id for mini, nopic, maxi])`.
    pub fn wikipedia(&self) -> Vec<(String, [Option<String>; 3])> {
        let mut by_lang: BTreeMap<String, [Option<String>; 3]> = BTreeMap::new();
        for p in self.pkgs.iter().filter(|p| p.category == "wikipedia") {
            let Some(rest) = p.id.strip_prefix("wikipedia_") else {
                continue;
            };
            let Some((lang, ed)) = rest.split_once('_') else {
                continue;
            };
            if let Some(slot) = EDITIONS.iter().position(|(e, ..)| *e == ed) {
                by_lang.entry(lang.to_ascii_uppercase()).or_default()[slot] = Some(p.id.clone());
            }
        }
        by_lang.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(id: &str, cat: &str, gb: f64, langs: &[&str], needs: &[&str]) -> Pkg {
        Pkg {
            id: id.into(),
            name: id.into(),
            desc: String::new(),
            category: cat.into(),
            kind: PackageType::Zim,
            bytes: (gb * 1e9) as u64,
            license: "CC0".into(),
            langs: langs.iter().map(|s| (*s).into()).collect(),
            needs: needs.iter().map(|s| (*s).into()).collect(),
            latest: None,
            file: None,
            sha256: None,
            host: None,
        }
    }

    fn ws() -> Workspace {
        let mut w = Workspace::new(
            vec![
                pkg("wikipedia_de_nopic", "wikipedia", 18.0, &["DE"], &[]),
                pkg("wikipedia_de_mini", "wikipedia", 5.0, &["DE"], &[]),
                pkg("wikipedia_en_maxi", "wikipedia", 111.0, &["EN"], &[]),
                pkg("jellyfin", "services", 0.7, &[], &[]),
                pkg("sintel", "video", 1.3, &[], &["jellyfin"]),
                pkg("fr_book", "books", 0.5, &["FR"], &[]),
            ],
            vec![ProfileDef {
                id: "std".into(),
                name: "Standard".into(),
                description: String::new(),
                ids: vec!["wikipedia_de_nopic".into(), "sintel".into(), "fr_book".into()],
                budget_bytes: 0,
            }],
        );
        w.langs = ["DE".to_string(), "EN".to_string()].into();
        w.free_bytes = 100_000_000_000;
        w.drive_bytes = 500_000_000_000;
        w
    }

    #[test]
    fn selecting_a_film_selects_its_player() {
        let mut w = ws();
        w.toggle("sintel");
        assert!(w.sel.contains("jellyfin"));
        assert_eq!(w.profile, "custom");
    }

    #[test]
    fn a_profile_sets_the_languages_and_pulls_needs() {
        let mut w = ws();
        w.apply_profile("std");
        assert!(w.sel.contains("wikipedia_de_nopic"));
        assert!(w.sel.contains("jellyfin"));
        assert!(
            w.sel.contains("fr_book"),
            "the profile wins over the language choice"
        );
        let langs: Vec<&str> = w.langs.iter().map(String::as_str).collect();
        assert_eq!(
            langs,
            ["DE", "FR"],
            "EN was chosen before but nothing in the profile is English"
        );
        assert_eq!(w.profile, "std");
    }

    #[test]
    fn dropping_a_language_keeps_installed_packages_selected() {
        let mut w = ws();
        w.installed.insert("fr_book".into(), Installed::default());
        w.sel.insert("fr_book".into());
        w.sel.insert("wikipedia_en_maxi".into());
        w.set_langs(["DE".to_string()].into());
        assert!(w.sel.contains("fr_book"));
        assert!(!w.sel.contains("wikipedia_en_maxi"));
        assert_eq!(w.profile, "custom");
    }

    #[test]
    fn diff_counts_additions_removals_and_what_stays() {
        let mut w = ws();
        w.installed.insert(
            "wikipedia_de_mini".into(),
            Installed {
                version: None,
                bytes: 5_000_000_000,
            },
        );
        w.installed.insert("jellyfin".into(), Installed::default());
        w.sel.insert("jellyfin".into());
        w.sel.insert("wikipedia_de_nopic".into());
        let d = w.diff(&|_| false);
        assert_eq!(d.add, ["wikipedia_de_nopic"]);
        assert_eq!(d.remove, ["wikipedia_de_mini"]);
        assert_eq!(d.add_bytes, 18_000_000_000);
        assert_eq!(d.remove_bytes, 5_000_000_000);
        assert_eq!(d.keep_bytes, 700_000_000);
        assert_eq!(w.free_after(&d), 100_000_000_000 + 5_000_000_000 - 18_000_000_000);
        let queued = w.diff(&|id| id == "wikipedia_de_nopic");
        assert!(
            queued.add.is_empty(),
            "a package already downloading is not added twice"
        );
    }

    #[test]
    fn status_and_updates() {
        let mut w = ws();
        w.installed.insert(
            "wikipedia_de_nopic".into(),
            Installed {
                version: Some("2026-08".into()),
                bytes: 0,
            },
        );
        w.pkgs[0].latest = Some("2026-10".into());
        w.sel.insert("wikipedia_de_nopic".into());
        assert_eq!(w.status("wikipedia_de_nopic", false), Status::Update);
        assert_eq!(w.status("wikipedia_de_nopic", true), Status::Downloading);
        assert_eq!(w.updates().len(), 1);
        w.pkgs[0].latest = Some("2026-08".into());
        assert_eq!(w.status("wikipedia_de_nopic", false), Status::Installed);
        w.sel.clear();
        assert_eq!(w.status("wikipedia_de_nopic", false), Status::WillRemove);
        assert_eq!(w.status("jellyfin", false), Status::None);
    }

    #[test]
    fn wikipedia_options_group_by_language() {
        let w = ws();
        let wiki = w.wikipedia();
        assert_eq!(wiki.len(), 2);
        assert_eq!(wiki[0].0, "DE");
        assert_eq!(wiki[0].1[0].as_deref(), Some("wikipedia_de_mini"));
        assert_eq!(wiki[0].1[1].as_deref(), Some("wikipedia_de_nopic"));
        assert_eq!(wiki[0].1[2], None);
    }

    #[test]
    fn license_labels_drop_the_ref_prefix() {
        assert_eq!(license_label("LicenseRef-Public-Domain-US"), "Public-Domain-US");
        assert_eq!(license_label("GPL-3.0-only"), "GPL-3.0-only");
    }

    #[test]
    fn sizes_read_like_the_design() {
        assert_eq!(fmt_bytes(75_000_000_000), "75 GB");
        assert_eq!(fmt_bytes(2_400_000_000), "2.4 GB");
        assert_eq!(fmt_bytes(120_000_000), "120 MB");
        assert_eq!(fmt_bytes(4_000), "1 MB");
    }

    #[test]
    fn categories_keep_design_order_and_append_unknown() {
        let mut w = ws();
        w.pkgs.push(pkg("x", "tools", 1.0, &[], &[]));
        let c: Vec<String> = w.categories().into_iter().map(|(id, _)| id).collect();
        assert_eq!(c, ["wikipedia", "services", "video", "books", "tools"]);
        assert_eq!(category_label("tools"), "Tools");
    }
}
