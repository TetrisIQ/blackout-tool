//! Package manifest schema (`catalog/<category>/<id>.yaml`) and its validation.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageType {
    Zim,
    Files,
    Container,
    /// A local program installed per user from Flathub (Phase 6b: games).
    Flatpak,
    /// A region cut out of a Protomaps planet build with `pmtiles extract` (Phase 7: maps).
    Map,
    Apt,
    Model,
}

impl PackageType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zim => "zim",
            Self::Files => "files",
            Self::Container => "container",
            Self::Flatpak => "flatpak",
            Self::Map => "map",
            Self::Apt => "apt",
            Self::Model => "model",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct License {
    /// SPDX identifier or expression, e.g. `CC-BY-SA-4.0`.
    pub spdx: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

/// Reference to an entry of the Kiwix OPDS catalog. Size, checksum and URLs are read live.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KiwixRef {
    /// Catalog name shared by all variants, e.g. `wikipedia_de_all`.
    pub name: String,
    /// Variant such as `nopic`; may be omitted when the name has a single variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flavour: Option<String>,
    /// ISO 639-3 language code, e.g. `deu`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

/// A map region: a bounding box cut out of the newest Protomaps build.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapRef {
    /// West, south, east, north in degrees (as `pmtiles extract --bbox`).
    pub bbox: [f64; 4],
    /// Highest zoom level kept; 15 is street level, each level fewer is roughly a quarter of the size.
    pub maxzoom: u8,
}

impl MapRef {
    fn problems(&self) -> Vec<String> {
        let [w, s, e, n] = self.bbox;
        let mut errs = Vec::new();
        if !(-180.0..=180.0).contains(&w) || !(-180.0..=180.0).contains(&e) || w >= e {
            errs.push("source.map.bbox needs west < east, both within -180..180".into());
        }
        if !(-90.0..=90.0).contains(&s) || !(-90.0..=90.0).contains(&n) || s >= n {
            errs.push("source.map.bbox needs south < north, both within -90..90".into());
        }
        if !(1..=15).contains(&self.maxzoom) {
            errs.push("source.map.maxzoom must be between 1 and 15".into());
        }
        errs
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kiwix: Option<KiwixRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metalink: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub torrent: Option<String>,
    /// Container image reference (type `container`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Flatpak application id on Flathub (type `flatpak`), e.g. `org.luanti.luanti`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak: Option<String>,
    /// Region of a `map` package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub map: Option<MapRef>,
    /// Distribution packages (type `apt`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
}

/// What a mount of a service container points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    /// The service's own writable directory, `<data-dir>/services/<id>/data`.
    Data,
    /// A shared media directory, `<data-dir>/media/<media>`, filled by `files` packages.
    Media,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub kind: MountKind,
    /// Media kind for `kind: media` (`music`, `audiobooks`, `video`, `books`, `games`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<String>,
    /// Path inside the container.
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

/// How a `container` package runs: one rootless container behind the local HTTPS front.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    /// Host name under which the front serves it, e.g. `navidrome.kit.local`.
    pub host: String,
    /// Port the application listens on inside the container (also used on 127.0.0.1).
    pub port: u16,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    /// Path that answers 200 when the application is up.
    pub health_path: String,
    /// Built-in first-run setup (`navidrome`, `audiobookshelf`), so no wizard is needed offline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<String>,
}

/// How a `files` package is unpacked into a shared media directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Extract {
    /// Keep the file as it is.
    None,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentSpec {
    /// Media kind; lands in `<data-dir>/media/<media>/<id>/`.
    pub media: String,
    pub extract: Extract,
}

pub const PROVISIONERS: &[&str] = &["navidrome", "audiobookshelf", "jellyfin", "kavita"];
/// Groups of local programs on the portal, with their heading (English / German).
pub const APP_GROUPS: &[(&str, &str)] = &[
    ("arcade", "Arcade"),
    ("strategy", "Strategy and simulation / Strategie"),
    ("puzzle", "Puzzle / Denkspiele"),
    ("sandbox", "Sandbox / Bauen und Erkunden"),
    ("racing", "Racing / Rennen"),
    ("board", "Board and card games / Brett- und Kartenspiele"),
];
/// The only Flatpak remote kit uses.
pub const FLATPAK_REMOTE: &str = "flathub";
pub const MEDIA_KINDS: &[&str] = &["music", "audiobooks", "video", "books", "games"];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    #[serde(rename = "type")]
    pub kind: PackageType,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    pub license: License,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Hook ids run after installation; the set of valid hooks is defined in Phase 3.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_install: Vec<String>,
    /// Host names the package replaces locally (Phase 4), e.g. `de.wikipedia.org`. Each gets a
    /// hosts entry, a vhost and a place in the local CA's name constraints. Exact hosts only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mirror_domains: Vec<String>,
    /// Portal group of a `flatpak` package (one of [`APP_GROUPS`]); required there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Required for `container` packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceSpec>,
    /// Required for `files` packages that feed a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentSpec>,
}

/// A plain lowercase DNS host name with at least two labels; no wildcards, ports or IPs.
pub fn is_valid_host(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && host.len() <= 253
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && !labels
            .last()
            .is_some_and(|t| t.bytes().all(|b| b.is_ascii_digit()))
}

pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A Flatpak application id: reverse-DNS, at least three dot-separated parts.
fn is_app_id(id: &str) -> bool {
    id.split('.').count() >= 3
        && id.split('.').all(|p| {
            !p.is_empty()
                && !p.starts_with(|c: char| c.is_ascii_digit())
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// https, or plain http to the local machine (used by tests; no network exposure).
fn is_secure_url(url: &str) -> bool {
    url.starts_with("https://")
        || ["http://127.0.0.1", "http://localhost", "http://[::1]"]
            .iter()
            .any(|p| {
                url.strip_prefix(p)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
            })
}

fn check_https(what: &str, url: &str, errs: &mut Vec<String>) {
    if !is_secure_url(url) {
        errs.push(format!("{what} {url:?} must use https://"));
    }
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}

impl Manifest {
    /// Checks a single manifest in isolation. Cross-package rules (unique ids, dependencies)
    /// live in [`crate::catalog::Catalog`]. Returns every problem found, not just the first.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !is_valid_id(&self.id) {
            errs.push(format!("id {:?} must match [a-z0-9][a-z0-9_-]*", self.id));
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("category", &self.category),
        ] {
            if blank(value) {
                errs.push(format!("{field} must not be empty"));
            }
        }
        if blank(&self.license.spdx) {
            errs.push("license.spdx must not be empty (every package needs a license)".into());
        }
        if let Some(h) = &self.sha256
            && !is_sha256(h)
        {
            errs.push("sha256 must be 64 lowercase hex characters".into());
        }
        if self.size_bytes == Some(0) {
            errs.push("size_bytes must be greater than 0".into());
        }
        if self.languages.iter().any(|l| blank(l)) {
            errs.push("languages must not contain empty entries".into());
        }
        if self.post_install.iter().any(|h| blank(h)) {
            errs.push("post_install must not contain empty hook ids".into());
        }
        let mut hosts = std::collections::HashSet::new();
        for d in &self.mirror_domains {
            if !is_valid_host(d) {
                errs.push(format!(
                    "mirror_domains entry {d:?} must be a lowercase host name like de.wikipedia.org"
                ));
            } else if !hosts.insert(d) {
                errs.push(format!("mirror_domains lists {d:?} more than once"));
            }
        }
        if !self.mirror_domains.is_empty() && self.kind != PackageType::Zim {
            errs.push("mirror_domains is only valid for type zim".into());
        }
        match (&self.service, self.kind) {
            (Some(sv), PackageType::Container) => {
                if !is_valid_host(&sv.host) {
                    errs.push(format!(
                        "service.host {:?} must be a lowercase host name",
                        sv.host
                    ));
                }
                if sv.port < 1 {
                    errs.push("service.port must be set".into());
                }
                if !sv.health_path.starts_with('/') {
                    errs.push("service.health_path must start with /".into());
                }
                for m in &sv.mounts {
                    if !m.target.starts_with('/') {
                        errs.push(format!("mount target {:?} must be an absolute path", m.target));
                    }
                    match (m.kind, m.media.as_deref()) {
                        (MountKind::Media, Some(k)) if MEDIA_KINDS.contains(&k) => {}
                        (MountKind::Media, other) => errs.push(format!(
                            "media mount needs media: one of {} (got {other:?})",
                            MEDIA_KINDS.join(", ")
                        )),
                        (MountKind::Data, Some(_)) => errs.push("data mounts take no media".into()),
                        (MountKind::Data, None) => {}
                    }
                }
                if let Some(p) = &sv.provision
                    && !PROVISIONERS.contains(&p.as_str())
                {
                    errs.push(format!("unknown service.provision {p:?}"));
                }
            }
            (Some(_), _) => errs.push("service is only valid for type container".into()),
            (None, PackageType::Container) => errs.push("type container needs a service block".into()),
            (None, _) => {}
        }
        if let Some(c) = &self.content {
            if self.kind != PackageType::Files {
                errs.push("content is only valid for type files".into());
            }
            if !MEDIA_KINDS.contains(&c.media.as_str()) {
                errs.push(format!("content.media must be one of {}", MEDIA_KINDS.join(", ")));
            }
        }
        match (&self.group, self.kind) {
            (Some(g), PackageType::Flatpak) => {
                if !APP_GROUPS.iter().any(|(k, _)| k == g) {
                    errs.push(format!(
                        "group {g:?} must be one of {}",
                        APP_GROUPS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            (Some(_), _) => errs.push("group is only valid for type flatpak".into()),
            (None, PackageType::Flatpak) => errs.push("type flatpak needs a group".into()),
            (None, _) => {}
        }
        if let Some(i) = &self.source.image
            && !i.contains("@sha256:")
        {
            errs.push("source.image must be pinned by digest (repo[:tag]@sha256:...), images are verified by their digest".into());
        }
        if self.depends_on.contains(&self.id) {
            errs.push("depends_on must not contain the package itself".into());
        }
        let mut seen = std::collections::HashSet::new();
        for d in &self.depends_on {
            if !seen.insert(d) {
                errs.push(format!("depends_on lists {d:?} more than once"));
            }
        }

        let s = &self.source;
        if let Some(k) = &s.kiwix {
            if self.kind != PackageType::Zim {
                errs.push("source.kiwix is only valid for type zim".into());
            }
            if blank(&k.name) {
                errs.push("source.kiwix.name must not be empty".into());
            }
            let forbidden = [
                ("source.url", s.url.is_some()),
                ("source.mirrors", !s.mirrors.is_empty()),
                ("source.metalink", s.metalink.is_some()),
                ("source.torrent", s.torrent.is_some()),
                ("sha256", self.sha256.is_some()),
                ("size_bytes", self.size_bytes.is_some()),
            ];
            for (field, present) in forbidden {
                if present {
                    errs.push(format!(
                        "{field} must not be set together with source.kiwix (read live from the Kiwix catalog)"
                    ));
                }
            }
        } else {
            match self.kind {
                PackageType::Zim | PackageType::Files | PackageType::Model => {
                    if s.url.is_none() {
                        errs.push(format!(
                            "type {} needs source.url (or source.kiwix for zim)",
                            self.kind.as_str()
                        ));
                    }
                }
                PackageType::Container => {
                    if s.image.as_deref().is_none_or(blank) {
                        errs.push("type container needs source.image".into());
                    }
                }
                PackageType::Flatpak => match s.flatpak.as_deref() {
                    Some(a) if is_app_id(a) => {}
                    Some(a) => errs.push(format!(
                        "source.flatpak {a:?} must be an application id like org.luanti.luanti"
                    )),
                    None => errs.push("type flatpak needs source.flatpak".into()),
                },
                PackageType::Map => match &s.map {
                    Some(m) => errs.extend(m.problems()),
                    None => errs.push("type map needs source.map (bbox and maxzoom)".into()),
                },
                PackageType::Apt => {
                    if s.packages.is_empty() || s.packages.iter().any(|p| blank(p)) {
                        errs.push("type apt needs a non-empty source.packages list".into());
                    }
                }
            }
            if s.url.is_some() {
                if self.sha256.is_none() {
                    errs.push("source.url needs a sha256 (checksums are verified before install)".into());
                }
                if self.size_bytes.is_none() {
                    errs.push("source.url needs size_bytes".into());
                }
            }
            if self.kind == PackageType::Flatpak && (s.url.is_some() || self.sha256.is_some()) {
                errs.push(
                    "type flatpak takes no url or sha256 (Flathub signs and verifies the commits)".into(),
                );
            }
            if self.kind == PackageType::Map && (s.url.is_some() || self.sha256.is_some()) {
                errs.push(
                    "type map takes no url or sha256 (the region is cut from the newest Protomaps build; \
                     kit records the checksum of the result)"
                        .into(),
                );
            }
            if matches!(
                self.kind,
                PackageType::Container | PackageType::Flatpak | PackageType::Map | PackageType::Apt
            ) && self.size_bytes.is_none()
            {
                errs.push(format!(
                    "type {} needs an estimated size_bytes",
                    self.kind.as_str()
                ));
            }
        }

        if s.map.is_some() && self.kind != PackageType::Map {
            errs.push("source.map is only valid for type map".into());
        }
        if s.flatpak.is_some() && self.kind != PackageType::Flatpak {
            errs.push("source.flatpak is only valid for type flatpak".into());
        }
        if let Some(u) = &s.url {
            check_https("source.url", u, &mut errs);
        }
        for m in &s.mirrors {
            check_https("source.mirrors entry", m, &mut errs);
        }
        if let Some(u) = &s.metalink {
            check_https("source.metalink", u, &mut errs);
        }
        if let Some(t) = &s.torrent
            && !t.starts_with("https://")
            && !t.starts_with("magnet:")
        {
            errs.push(format!("source.torrent {t:?} must use https:// or magnet:"));
        }
        errs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0e281a87af92a798297fc671f8b37004cb7874ac051e13876b7335be96649e59";

    fn parse(y: &str) -> Result<Manifest, serde_yaml_ng::Error> {
        serde_yaml_ng::from_str(y)
    }

    const KIWIX: &str = "
id: wikipedia_de_nopic
name: Wikipedia (Deutsch, ohne Bilder)
description: German Wikipedia without images
category: wikipedia
type: zim
source:
  kiwix: {name: wikipedia_de_all, flavour: nopic, lang: deu}
license: {spdx: CC-BY-SA-4.0}
languages: [de]
";

    fn files(extra: &str) -> String {
        format!(
            "
id: sample
name: Sample
description: d
category: games
type: files
source: {{url: 'https://example.org/a.tar'}}
sha256: {SHA}
size_bytes: 10
license: {{spdx: CC0-1.0}}
{extra}"
        )
    }

    #[test]
    fn valid_kiwix_manifest() {
        let m = parse(KIWIX).unwrap();
        assert_eq!(m.kind, PackageType::Zim);
        assert_eq!(m.source.kiwix.as_ref().unwrap().flavour.as_deref(), Some("nopic"));
        assert!(m.validate().is_empty(), "{:?}", m.validate());
    }

    fn flatpak(extra: &str) -> String {
        format!(
            "
id: luanti
name: Luanti
description: d
category: games
type: flatpak
source: {{flatpak: org.luanti.luanti}}
size_bytes: 29000000
group: sandbox
license: {{spdx: LGPL-2.1-or-later}}
{extra}"
        )
    }

    #[test]
    fn flatpak_manifest_needs_app_id_group_and_size() {
        assert!(parse(&flatpak("")).unwrap().validate().is_empty());
        let bad = |from: &str, to: &str| parse(&flatpak("").replace(from, to)).unwrap().validate();
        assert!(bad("org.luanti.luanti", "luanti")[0].contains("application id"));
        assert!(bad("group: sandbox", "group: shooter")[0].contains("must be one of"));
        assert!(bad("group: sandbox\n", "")[0].contains("needs a group"));
        assert!(bad("size_bytes: 29000000\n", "")[0].contains("estimated size_bytes"));
        assert!(
            bad("source: {flatpak: org.luanti.luanti}", "source: {}")[0].contains("needs source.flatpak")
        );
        let with_url = bad(
            "source: {flatpak: org.luanti.luanti}",
            "source: {flatpak: org.luanti.luanti, url: 'https://e.org/a'}",
        );
        assert!(
            with_url.iter().any(|e| e.contains("takes no url")),
            "{with_url:?}"
        );
    }

    #[test]
    fn group_and_flatpak_source_are_only_for_flatpak_packages() {
        let e = parse(&files("group: arcade\n")).unwrap().validate();
        assert_eq!(e, vec!["group is only valid for type flatpak".to_string()]);
        let e = parse(&files("").replace(
            "source: {url: 'https://example.org/a.tar'}",
            "source: {url: 'https://example.org/a.tar', flatpak: org.a.b}",
        ))
        .unwrap()
        .validate();
        assert_eq!(
            e,
            vec!["source.flatpak is only valid for type flatpak".to_string()]
        );
    }

    #[test]
    fn valid_download_manifest() {
        assert!(parse(&files("")).unwrap().validate().is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse(&files("colour: red\n")).is_err());
    }

    #[test]
    fn unknown_type_is_rejected() {
        assert!(parse(&files("").replace("type: files", "type: tarball")).is_err());
    }

    #[test]
    fn missing_license_is_rejected() {
        let y = KIWIX.replace("license: {spdx: CC-BY-SA-4.0}\n", "");
        assert!(parse(&y).is_err());
    }

    #[test]
    fn blank_license_fails_validation() {
        let y = KIWIX.replace("CC-BY-SA-4.0", "' '");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("license.spdx")), "{errs:?}");
    }

    #[test]
    fn kiwix_forbids_hardcoded_size_and_checksum() {
        let y = format!("{KIWIX}size_bytes: 5\nsha256: {SHA}\n");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.starts_with("size_bytes")));
        assert!(errs.iter().any(|e| e.starts_with("sha256")));
    }

    #[test]
    fn kiwix_forbids_url() {
        let y = KIWIX.replace("source:\n", "source:\n  url: 'https://x.org/a'\n");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.starts_with("source.url")), "{errs:?}");
    }

    #[test]
    fn kiwix_only_for_zim() {
        let y = KIWIX.replace("type: zim", "type: files");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("only valid for type zim")));
    }

    #[test]
    fn download_needs_checksum_and_size() {
        let y = files("")
            .replace(&format!("sha256: {SHA}\n"), "")
            .replace("size_bytes: 10\n", "");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("sha256")));
        assert!(errs.iter().any(|e| e.contains("size_bytes")));
    }

    #[test]
    fn download_must_be_https() {
        let y = files("").replace("https://example.org", "http://example.org");
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("https://")));
    }

    #[test]
    fn plain_http_is_only_allowed_to_the_local_machine() {
        for ok in [
            "http://127.0.0.1:8080/a",
            "http://localhost/a",
            "http://[::1]:1/x",
            "https://example.org/a",
        ] {
            let y = files("").replace("https://example.org/a.tar", ok);
            assert!(parse(&y).unwrap().validate().is_empty(), "{ok}");
        }
        for bad in [
            "http://127.0.0.1.evil.org/a",
            "http://localhost.evil.org/a",
            "http://example.org/a",
        ] {
            let y = files("").replace("https://example.org/a.tar", bad);
            assert!(!parse(&y).unwrap().validate().is_empty(), "{bad}");
        }
    }

    #[test]
    fn mirrors_and_torrent_are_checked() {
        let y = files("").replace(
            "source: {url: 'https://example.org/a.tar'}",
            "source: {url: 'https://example.org/a.tar', mirrors: ['ftp://m.org/a'], torrent: 'http://t.org/a.torrent'}",
        );
        let errs = parse(&y).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("mirrors")));
        assert!(errs.iter().any(|e| e.contains("torrent")));
        let ok = y.replace("http://t.org/a.torrent", "magnet:?xt=urn:btih:abc");
        let errs = parse(&ok).unwrap().validate();
        assert!(!errs.iter().any(|e| e.contains("torrent")));
    }

    #[test]
    fn bad_checksum_formats() {
        for bad in ["abc", &SHA.to_uppercase(), &SHA[..63]] {
            let y = files("").replace(SHA, bad);
            let errs = parse(&y).unwrap().validate();
            assert!(errs.iter().any(|e| e.contains("64 lowercase hex")), "{bad}");
        }
    }

    #[test]
    fn container_and_apt_requirements() {
        let c = "
id: jellyfin
name: Jellyfin
description: d
category: services
type: container
source: {}
license: {spdx: GPL-2.0-only}
";
        let errs = parse(c).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("source.image")));
        assert!(errs.iter().any(|e| e.contains("size_bytes")));
        let ok = c.replace(
            "source: {}",
            "source: {image: 'docker.io/jellyfin/jellyfin:10'}\nsize_bytes: 600000000",
        );
        let errs = parse(&ok).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("pinned by digest")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("service block")), "{errs:?}");
        let ok = c.replace(
            "source: {}",
            &format!(
                "source: {{image: 'docker.io/jellyfin/jellyfin:10@sha256:{SHA}'}}\nsize_bytes: 600000000\n\
                 service: {{host: jellyfin.kit.local, port: 8096, health_path: /health, \
                 mounts: [{{kind: media, media: video, target: /media, read_only: true}}, {{kind: data, target: /config}}]}}"
            ),
        );
        assert!(
            parse(&ok).unwrap().validate().is_empty(),
            "{:?}",
            parse(&ok).unwrap().validate()
        );
        let bad = ok
            .replace("media: video", "media: pictures")
            .replace("jellyfin.kit.local", "Jelly Fin");
        let errs = parse(&bad).unwrap().validate();
        assert!(
            errs.iter().any(|e| e.contains("media mount")) && errs.iter().any(|e| e.contains("service.host")),
            "{errs:?}"
        );

        let a = c.replace("container", "apt");
        let errs = parse(&a).unwrap().validate();
        assert!(errs.iter().any(|e| e.contains("source.packages")));
        let ok = a.replace(
            "source: {}",
            "source: {packages: [kiwix-tools]}\nsize_bytes: 9000000",
        );
        assert!(parse(&ok).unwrap().validate().is_empty());
    }

    #[test]
    fn id_rules_and_self_dependency() {
        for bad in ["Wiki", "_x", "a b", ""] {
            let y = files("").replace("id: sample", &format!("id: '{bad}'"));
            let errs = parse(&y).unwrap().validate();
            assert!(errs.iter().any(|e| e.starts_with("id ")), "{bad:?}");
        }
        let errs = parse(&files("depends_on: [sample, sample]\n"))
            .unwrap()
            .validate();
        assert!(errs.iter().any(|e| e.contains("itself")));
        assert!(errs.iter().any(|e| e.contains("more than once")));
    }

    #[test]
    fn all_problems_are_reported_together() {
        let y = files("")
            .replace("name: Sample", "name: ''")
            .replace("https://", "http://");
        assert!(parse(&y).unwrap().validate().len() >= 2);
    }

    fn map(extra: &str) -> String {
        format!(
            "
id: map_x
name: Map X
description: d
category: maps
type: map
size_bytes: 1000
license: {{spdx: ODbL-1.0}}
{extra}"
        )
    }

    #[test]
    fn map_manifest_rules() {
        let ok = parse(&map(
            "source: {map: {bbox: [5.8, 47.2, 15.0, 55.1], maxzoom: 13}}",
        ))
        .unwrap();
        assert!(ok.validate().is_empty(), "{:?}", ok.validate());

        let no_region = parse(&map("source: {}")).unwrap();
        assert!(
            no_region
                .validate()
                .iter()
                .any(|e| e.contains("needs source.map"))
        );

        let swapped = parse(&map(
            "source: {map: {bbox: [15.0, 55.1, 5.8, 47.2], maxzoom: 13}}",
        ))
        .unwrap();
        let errs = swapped.validate().join("; ");
        assert!(
            errs.contains("west < east") && errs.contains("south < north"),
            "{errs}"
        );

        let deep = parse(&map(
            "source: {map: {bbox: [5.8, 47.2, 15.0, 55.1], maxzoom: 20}}",
        ))
        .unwrap();
        assert!(deep.validate().iter().any(|e| e.contains("maxzoom")));

        let with_url = parse(&map(&format!(
            "source: {{url: 'https://example.org/x.pmtiles', map: {{bbox: [5.8, 47.2, 15.0, 55.1], maxzoom: 9}}}}\nsha256: {SHA}"
        )))
        .unwrap();
        assert!(
            with_url
                .validate()
                .iter()
                .any(|e| e.contains("type map takes no url or sha256"))
        );

        let on_zim = parse(KIWIX).unwrap();
        assert!(on_zim.validate().is_empty());
        let mut misplaced = on_zim;
        misplaced.source.map = ok.source.map.clone();
        assert!(
            misplaced
                .validate()
                .iter()
                .any(|e| e.contains("only valid for type map"))
        );
    }
}
