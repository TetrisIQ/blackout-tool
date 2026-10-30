//! Reads current URLs, sizes and checksums from the Kiwix OPDS catalog.
//!
//! The OPDS feed lists variants of one content under a shared `name`, distinguished by
//! `flavour`. It carries only an approximate size and no checksum, so for the selected entry the
//! `.meta4` metalink is fetched as well; it has the exact size, the SHA-256 and the mirrors.

use super::http::{Http, HttpError, encode_component};
use super::manifest::KiwixRef;

pub const DEFAULT_BASE: &str = "https://library.kiwix.org";

#[derive(Debug, thiserror::Error)]
pub enum KiwixError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("cannot parse {what}: {message}")]
    Parse { what: &'static str, message: String },
    #[error("Kiwix catalog has no entry for {0}")]
    NotFound(String),
    #[error("{name} has several variants ({options}); set source.kiwix.flavour")]
    Ambiguous { name: String, options: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub title: String,
    pub name: String,
    pub flavour: String,
    pub languages: Vec<String>,
    pub updated: String,
    /// Link to the `.meta4` metalink.
    pub meta4_url: String,
    /// Size announced in the feed (approximate).
    pub approx_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metalink {
    pub size: u64,
    pub sha256: String,
    /// Mirror URLs ordered by priority.
    pub mirrors: Vec<String>,
}

/// Fully resolved Kiwix download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub title: String,
    /// Version tag taken from the entry's `updated` date.
    pub updated: String,
    pub url: String,
    pub mirrors: Vec<String>,
    pub size_bytes: u64,
    pub sha256: String,
}

fn parse_err(what: &'static str, e: impl ToString) -> KiwixError {
    KiwixError::Parse {
        what,
        message: e.to_string(),
    }
}

pub fn parse_opds(xml: &str) -> Result<Vec<Entry>, KiwixError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| parse_err("OPDS feed", e))?;
    let mut entries = Vec::new();
    for e in doc.descendants().filter(|n| n.has_tag_name("entry")) {
        let text = |tag: &str| {
            e.children()
                .find(|c| c.has_tag_name(tag))
                .and_then(|c| c.text())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let link = e
            .children()
            .filter(|c| c.has_tag_name("link"))
            .find(|c| c.attribute("rel").is_some_and(|r| r.contains("acquisition")))
            .ok_or_else(|| {
                parse_err(
                    "OPDS feed",
                    format!("entry {:?} has no download link", text("name")),
                )
            })?;
        entries.push(Entry {
            title: text("title"),
            name: text("name"),
            flavour: text("flavour"),
            languages: text("language")
                .split(',')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect(),
            updated: text("updated"),
            meta4_url: link.attribute("href").unwrap_or("").to_string(),
            approx_size: link.attribute("length").and_then(|l| l.parse().ok()).unwrap_or(0),
        });
    }
    Ok(entries)
}

pub fn parse_meta4(xml: &str) -> Result<Metalink, KiwixError> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| parse_err("metalink", e))?;
    let file = doc
        .descendants()
        .find(|n| n.has_tag_name("file"))
        .ok_or_else(|| parse_err("metalink", "no <file> element"))?;
    let child_text = |tag: &str| {
        file.children()
            .find(|c| c.has_tag_name(tag))
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
    };
    let size = child_text("size")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .ok_or_else(|| parse_err("metalink", "missing or invalid <size>"))?;
    let sha256 = file
        .children()
        .find(|c| c.has_tag_name("hash") && c.attribute("type") == Some("sha-256"))
        .and_then(|c| c.text())
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| parse_err("metalink", "missing or invalid sha-256 hash"))?;
    let mut urls: Vec<(u32, String)> = file
        .children()
        .filter(|c| c.has_tag_name("url"))
        .filter_map(|c| {
            let url = c.text()?.trim().to_string();
            let prio = c
                .attribute("priority")
                .and_then(|p| p.parse().ok())
                .unwrap_or(u32::MAX);
            Some((prio, url))
        })
        .collect();
    urls.sort_by_key(|(p, _)| *p);
    Ok(Metalink {
        size,
        sha256,
        mirrors: urls.into_iter().map(|(_, u)| u).collect(),
    })
}

/// Chooses the entry a manifest refers to; the newest `updated` wins.
pub fn select<'a>(entries: &'a [Entry], r: &KiwixRef) -> Result<&'a Entry, KiwixError> {
    let describe = || {
        format!(
            "name={} flavour={} lang={}",
            r.name,
            r.flavour.as_deref().unwrap_or("*"),
            r.lang.as_deref().unwrap_or("*")
        )
    };
    let named: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.name == r.name)
        .filter(|e| r.lang.as_ref().is_none_or(|l| e.languages.contains(l)))
        .collect();
    let candidates: Vec<&Entry> = match &r.flavour {
        Some(f) => named.iter().copied().filter(|e| &e.flavour == f).collect(),
        None => {
            let mut flavours: Vec<&str> = named.iter().map(|e| e.flavour.as_str()).collect();
            flavours.sort_unstable();
            flavours.dedup();
            if flavours.len() > 1 {
                return Err(KiwixError::Ambiguous {
                    name: r.name.clone(),
                    options: flavours.join(", "),
                });
            }
            named.clone()
        }
    };
    candidates
        .into_iter()
        .max_by(|a, b| a.updated.cmp(&b.updated))
        .ok_or_else(|| KiwixError::NotFound(describe()))
}

pub struct KiwixClient<'a, H: Http + ?Sized> {
    http: &'a H,
    base: String,
}

impl<'a, H: Http + ?Sized> KiwixClient<'a, H> {
    pub fn new(http: &'a H, base: &str) -> Self {
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn query_url(&self, r: &KiwixRef) -> String {
        let mut url = format!(
            "{}/catalog/v2/entries?name={}",
            self.base,
            encode_component(&r.name)
        );
        if let Some(l) = &r.lang {
            url.push_str("&lang=");
            url.push_str(&encode_component(l));
        }
        url.push_str("&count=50");
        url
    }

    pub fn lookup(&self, r: &KiwixRef) -> Result<Download, KiwixError> {
        let feed = self.http.get(&self.query_url(r))?;
        let entries = parse_opds(&feed)?;
        let entry = select(&entries, r)?;
        let meta = parse_meta4(&self.http.get(&entry.meta4_url)?)?;
        let url = entry
            .meta4_url
            .strip_suffix(".meta4")
            .unwrap_or(&entry.meta4_url)
            .to_string();
        Ok(Download {
            title: entry.title.clone(),
            updated: entry.updated.clone(),
            url,
            mirrors: meta.mirrors,
            size_bytes: meta.size,
            sha256: meta.sha256,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::http::testing::FakeHttp;
    use super::*;

    const OPDS: &str = include_str!("../../tests/fixtures/kiwix/opds_wikipedia_de_all.xml");
    const OPDS_ARCH: &str = include_str!("../../tests/fixtures/kiwix/opds_archlinux_en_all.xml");
    const META_NOPIC: &str =
        include_str!("../../tests/fixtures/kiwix/meta4_wikipedia_de_all_nopic_2026-10.xml");
    const NOPIC_META_URL: &str =
        "https://lb.download.kiwix.org/zim/wikipedia/wikipedia_de_all_nopic_2026-10.zim.meta4";

    fn kref(name: &str, flavour: Option<&str>, lang: Option<&str>) -> KiwixRef {
        KiwixRef {
            name: name.into(),
            flavour: flavour.map(Into::into),
            lang: lang.map(Into::into),
        }
    }

    fn entry(flavour: &str, updated: &str, langs: &[&str]) -> Entry {
        Entry {
            title: "T".into(),
            name: "n".into(),
            flavour: flavour.into(),
            languages: langs.iter().map(|s| s.to_string()).collect(),
            updated: updated.into(),
            meta4_url: format!("https://x/{flavour}-{updated}.zim.meta4"),
            approx_size: 1,
        }
    }

    #[test]
    fn parses_recorded_opds_feed() {
        let entries = parse_opds(OPDS).unwrap();
        assert_eq!(entries.len(), 3);
        let nopic = entries.iter().find(|e| e.flavour == "nopic").unwrap();
        assert_eq!(nopic.name, "wikipedia_de_all");
        assert_eq!(nopic.languages, vec!["deu"]);
        assert_eq!(nopic.meta4_url, NOPIC_META_URL);
        assert_eq!(nopic.approx_size, 18_585_337_856);
        assert_eq!(nopic.updated, "2026-10-01T00:00:00Z");
    }

    #[test]
    fn entry_without_flavour_parses_and_is_unambiguous() {
        let entries = parse_opds(OPDS_ARCH).unwrap();
        assert_eq!(entries.len(), 1);
        let e = select(&entries, &kref("archlinux_en_all", None, None)).unwrap();
        assert_eq!(e.flavour, "maxi");
    }

    #[test]
    fn parses_recorded_metalink() {
        let m = parse_meta4(META_NOPIC).unwrap();
        assert_eq!(m.size, 18_585_337_585);
        assert_eq!(
            m.sha256,
            "59af34df2f21e1824d96a534cfef0a7776b589ee215ad2a7f7bb3b6328ca30ba"
        );
        assert_eq!(m.mirrors.len(), 8);
        assert!(m.mirrors[0].starts_with("https://ftp.fau.de/"));
        assert!(!m.mirrors.iter().any(|u| u == "https://kiwix.org"));
    }

    #[test]
    fn metalink_without_checksum_is_rejected() {
        let xml = META_NOPIC.replace("sha-256", "sha-512");
        assert!(matches!(parse_meta4(&xml), Err(KiwixError::Parse { .. })));
        assert!(parse_meta4("<metalink><file name='x'><size>0</size></file></metalink>").is_err());
        assert!(parse_meta4("not xml").is_err());
    }

    #[test]
    fn mirrors_sorted_by_priority() {
        let xml = "<metalink><file name='a'><size>5</size>\
            <hash type='sha-256'>59af34df2f21e1824d96a534cfef0a7776b589ee215ad2a7f7bb3b6328ca30ba</hash>\
            <url priority='2'>https://b</url><url priority='1'>https://a</url></file></metalink>";
        assert_eq!(parse_meta4(xml).unwrap().mirrors, vec!["https://a", "https://b"]);
    }

    #[test]
    fn select_by_flavour_and_language() {
        let es = vec![
            entry("mini", "2026-10", &["deu"]),
            entry("nopic", "2026-10", &["deu"]),
            entry("nopic", "2026-10", &["eng"]),
        ];
        let mut r = kref("n", Some("nopic"), Some("eng"));
        assert_eq!(select(&es, &r).unwrap().languages, vec!["eng"]);
        r.lang = Some("fra".into());
        assert!(matches!(select(&es, &r), Err(KiwixError::NotFound(_))));
    }

    #[test]
    fn select_prefers_newest() {
        let es = vec![
            entry("nopic", "2026-06", &["deu"]),
            entry("nopic", "2026-10", &["deu"]),
        ];
        assert_eq!(
            select(&es, &kref("n", Some("nopic"), None)).unwrap().updated,
            "2026-10"
        );
    }

    #[test]
    fn select_matches_one_language_of_a_multilingual_zim() {
        let es = vec![entry("", "2026-10", &["eng", "deu"])];
        assert!(select(&es, &kref("n", None, Some("deu"))).is_ok());
    }

    #[test]
    fn missing_flavour_is_ambiguous_with_several_variants() {
        let entries = parse_opds(OPDS).unwrap();
        match select(&entries, &kref("wikipedia_de_all", None, Some("deu"))) {
            Err(KiwixError::Ambiguous { options, .. }) => assert_eq!(options, "maxi, mini, nopic"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wrong_name_not_found() {
        let entries = parse_opds(OPDS).unwrap();
        assert!(matches!(
            select(&entries, &kref("wikipedia_de_all_nopic", None, None)),
            Err(KiwixError::NotFound(_))
        ));
    }

    #[test]
    fn lookup_end_to_end_with_fixtures() {
        let r = kref("wikipedia_de_all", Some("nopic"), Some("deu"));
        let http = FakeHttp::default();
        let client = KiwixClient::new(&http, DEFAULT_BASE);
        let http = FakeHttp::default()
            .with(&client.query_url(&r), OPDS)
            .with(NOPIC_META_URL, META_NOPIC);
        let d = KiwixClient::new(&http, DEFAULT_BASE).lookup(&r).unwrap();
        assert_eq!(d.size_bytes, 18_585_337_585, "exact size comes from the metalink");
        assert_eq!(d.url, NOPIC_META_URL.trim_end_matches(".meta4"));
        assert_eq!(d.mirrors.len(), 8);
        assert_eq!(d.sha256.len(), 64);
    }

    #[test]
    fn query_url_is_encoded() {
        let http = FakeHttp::default();
        let c = KiwixClient::new(&http, "https://k.org/");
        assert_eq!(
            c.query_url(&kref("a b", None, Some("deu"))),
            "https://k.org/catalog/v2/entries?name=a%20b&lang=deu&count=50"
        );
    }

    #[test]
    fn network_error_propagates() {
        let http = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let r = kref("wikipedia_de_all", Some("nopic"), None);
        assert!(matches!(
            KiwixClient::new(&http, DEFAULT_BASE).lookup(&r),
            Err(KiwixError::Http(_))
        ));
    }
}
