//! The offline link checker: starts at a served host (kit.local by default), follows a random sample of internal links
//! and verifies that they load. The real fetcher resolves only the hosts kit serves (to the local
//! Caddy) and trusts only the local CA, so a request that would leave the machine fails instead
//! of silently succeeding over the real internet.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use ureq::ResponseExt;
use ureq::tls::{Certificate, RootCerts, TlsConfig};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use super::NetError;

pub struct Response {
    pub status: u16,
    /// After redirects.
    pub final_url: String,
    pub body: String,
}

pub trait Fetch {
    fn get(&self, url: &str) -> Result<Response, String>;
}

#[derive(Debug, Default)]
pub struct Report {
    pub pages_checked: usize,
    pub resources_checked: usize,
    /// `<a href>` links to other hosts (not followed; they cannot work offline).
    pub external_links: usize,
    pub problems: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Link,
    Resource,
}

/// `(kind, raw reference)` of every link and embedded resource in `html`.
fn references(html: &str) -> Vec<(Kind, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = html[i..].find('<') {
        let start = i + off + 1;
        let Some(end_off) = html[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + end_off];
        i = start + end_off + 1;
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = tag[..name_end].to_ascii_lowercase();
        let (attr, kind) = match name.as_str() {
            "a" | "area" => ("href", Kind::Link),
            "link" => ("href", Kind::Resource),
            "img" | "script" | "source" | "iframe" | "embed" | "audio" | "video" | "track" => {
                ("src", Kind::Resource)
            }
            _ => continue,
        };
        if name == "link" {
            let rel = attr_value(tag, "rel").unwrap_or_default().to_ascii_lowercase();
            if !["stylesheet", "icon", "preload", "modulepreload"]
                .iter()
                .any(|r| rel.contains(r))
            {
                continue;
            }
        }
        if let Some(v) = attr_value(tag, attr) {
            out.push((kind, v));
        }
    }
    out
}

fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(attr) {
        let at = from + p;
        from = at + attr.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = tag[at + attr.len()..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let v = rest[1..].trim_start();
        return Some(match v.chars().next()? {
            q @ ('"' | '\'') => v[1..].split(q).next()?.to_string(),
            _ => v
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()?
                .to_string(),
        });
    }
    None
}

/// `(host, path-and-query)` of `reference` seen from `base` (an https URL), without fragment.
/// `None` for schemes that are not web links.
pub fn join(base: &str, reference: &str) -> Option<(String, String)> {
    let reference = reference.trim();
    let reference = reference.split('#').next().unwrap_or("");
    if reference.is_empty() {
        return None;
    }
    let (b_host, b_path) = split_url(base)?;
    if let Some(rest) = reference.strip_prefix("//") {
        return split_authority(rest);
    }
    if let Some(i) = reference.find(':')
        && reference[..i]
            .bytes()
            .all(|c| c.is_ascii_alphabetic() || c == b'+' || c == b'-' || c == b'.')
    {
        let scheme = reference[..i].to_ascii_lowercase();
        return match scheme.as_str() {
            "http" | "https" => split_authority(reference[i + 1..].trim_start_matches('/')),
            _ => None,
        };
    }
    if reference.starts_with('/') {
        return Some((b_host, normalize(reference)));
    }
    if reference.starts_with('?') {
        let path = b_path.split('?').next().unwrap_or("/");
        return Some((b_host, format!("{path}{reference}")));
    }
    let dir = b_path.split('?').next().unwrap_or("/");
    let dir = &dir[..dir.rfind('/').map_or(0, |i| i + 1)];
    Some((b_host, normalize(&format!("{dir}{reference}"))))
}

fn split_url(url: &str) -> Option<(String, String)> {
    split_authority(
        url.strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?,
    )
}

fn split_authority(rest: &str) -> Option<(String, String)> {
    let (auth, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = auth.rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    Some((host, normalize(&path)))
}

/// Resolves `.` and `..` segments of the path part; the query is kept as is.
fn normalize(path_and_query: &str) -> String {
    let (path, query) = match path_and_query.find('?') {
        Some(i) => (&path_and_query[..i], &path_and_query[i..]),
        None => (path_and_query, ""),
    };
    let mut segs: Vec<&str> = Vec::new();
    for seg in path.split('/').skip(1) {
        match seg {
            "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    format!("/{}{query}", segs.join("/"))
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Checks `samples` pages reachable from `https://<start_host>/`.
pub fn check(fetch: &dyn Fetch, hosts: &[String], start_host: &str, samples: usize, seed: u64) -> Report {
    let mut report = Report::default();
    let mut rng = Rng(seed | 1);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut resources: BTreeSet<String> = BTreeSet::new();
    let mut pool: Vec<String> = vec![format!("https://{start_host}/")];

    while report.pages_checked < samples {
        if pool.is_empty() {
            break;
        }
        let url = pool.swap_remove(rng.below(pool.len()));
        if !visited.insert(url.clone()) {
            continue;
        }
        let page = match fetch.get(&url) {
            Ok(p) => p,
            Err(e) => {
                report.problems.push(format!("{url}: {e}"));
                report.pages_checked += 1;
                continue;
            }
        };
        report.pages_checked += 1;
        if page.status != 200 {
            report.problems.push(format!("{url}: HTTP {}", page.status));
            continue;
        }
        for (kind, raw) in references(&page.body) {
            let Some((host, path)) = join(&page.final_url, &raw) else {
                continue;
            };
            let target = format!("https://{host}{path}");
            let internal = hosts.contains(&host);
            match kind {
                Kind::Link if internal => {
                    if !visited.contains(&target) && !pool.contains(&target) {
                        pool.push(target);
                    }
                }
                Kind::Link => report.external_links += 1,
                Kind::Resource if !internal => report
                    .problems
                    .push(format!("{url}: loads {target} from outside the machine")),
                Kind::Resource => {
                    if resources.insert(target.clone()) {
                        report.resources_checked += 1;
                        match fetch.get(&target) {
                            Ok(r) if r.status == 200 => {}
                            Ok(r) => report
                                .problems
                                .push(format!("{url}: resource {target} gave HTTP {}", r.status)),
                            Err(e) => report.problems.push(format!("{url}: resource {target}: {e}")),
                        }
                    }
                }
            }
        }
    }
    if report.pages_checked < samples {
        report.problems.push(format!(
            "only {} pages reachable from https://{start_host}/ (asked for {samples})",
            report.pages_checked
        ));
    }
    report
}

/// Resolves the hosts kit serves to the local Caddy and refuses every other name.
#[derive(Debug)]
struct PinnedResolver {
    hosts: Vec<String>,
    port: u16,
}

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().unwrap_or("").to_ascii_lowercase();
        if !self.hosts.contains(&host) {
            return Err(ureq::Error::HostNotFound);
        }
        let mut addrs = self.empty();
        addrs.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port));
        Ok(addrs)
    }
}

/// HTTPS client that trusts only the local CA and reaches only the hosts kit serves.
pub struct PinnedFetch {
    agent: ureq::Agent,
}

impl PinnedFetch {
    pub fn new(hosts: Vec<String>, listen_port: u16, ca_pem: &[u8]) -> Result<Self, NetError> {
        let cert = Certificate::from_pem(ca_pem).map_err(|e| NetError::Cert(e.to_string()))?;
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::new_with_certs(&[cert]))
            .build();
        let config = ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(20)))
            .build();
        let agent = ureq::Agent::with_parts(
            config,
            DefaultConnector::new(),
            PinnedResolver {
                hosts,
                port: listen_port,
            },
        );
        Ok(Self { agent })
    }
}

impl Fetch for PinnedFetch {
    fn get(&self, url: &str) -> Result<Response, String> {
        let mut resp = self.agent.get(url).call().map_err(|e| match e {
            ureq::Error::HostNotFound => "would leave the machine (host is not served by kit)".to_string(),
            e => e.to_string(),
        })?;
        let status = resp.status().as_u16();
        let final_url = resp.get_uri().to_string();
        let is_html = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("html"));
        // Only HTML is parsed; other bodies (images, fonts) are not downloaded.
        let body = if is_html {
            resp.body_mut().read_to_string().map_err(|e| e.to_string())?
        } else {
            String::new()
        };
        Ok(Response {
            status,
            final_url,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake(HashMap<String, (u16, String)>);

    impl Fetch for Fake {
        fn get(&self, url: &str) -> Result<Response, String> {
            match self.0.get(url) {
                Some((status, body)) => Ok(Response {
                    status: *status,
                    final_url: url.to_string(),
                    body: body.clone(),
                }),
                None => Err("would leave the machine (host is not served by kit)".into()),
            }
        }
    }

    fn site(pages: &[(&str, u16, &str)]) -> Fake {
        Fake(
            pages
                .iter()
                .map(|(u, s, b)| (u.to_string(), (*s, b.to_string())))
                .collect(),
        )
    }

    fn hosts() -> Vec<String> {
        vec!["de.wikipedia.org".into()]
    }

    #[test]
    fn joins_and_normalizes() {
        let base = "https://de.wikipedia.org/content/book/Berlin";
        let j = |r: &str| join(base, r);
        let h = |p: &str| Some(("de.wikipedia.org".to_string(), p.to_string()));
        assert_eq!(j("Hamburg"), h("/content/book/Hamburg"));
        assert_eq!(j("./Hamburg#x"), h("/content/book/Hamburg"));
        assert_eq!(j("../other/x"), h("/content/other/x"));
        assert_eq!(j("/wiki/A?x=1"), h("/wiki/A?x=1"));
        assert_eq!(j("?q=1"), h("/content/book/Berlin?q=1"));
        assert_eq!(
            j("//en.wikipedia.org/wiki/B"),
            Some(("en.wikipedia.org".into(), "/wiki/B".into()))
        );
        assert_eq!(j("http://DE.wikipedia.org:80/wiki/C"), h("/wiki/C"));
        assert_eq!(j("mailto:a@b.c"), None);
        assert_eq!(j("javascript:void(0)"), None);
        assert_eq!(j("#top"), None);
        assert_eq!(j("data:image/png;base64,AAAA"), None);
    }

    #[test]
    fn extracts_links_and_resources_only() {
        let html = r#"<a href="x">a</a> <A HREF='y'>b</A> <img src=z.png alt=q> <link rel="stylesheet" href="s.css">
            <link rel="canonical" href="https://elsewhere.org/"> <p class="href=nope"> <script src="j.js"></script>"#;
        let refs = references(html);
        assert_eq!(
            refs,
            vec![
                (Kind::Link, "x".into()),
                (Kind::Link, "y".into()),
                (Kind::Resource, "z.png".into()),
                (Kind::Resource, "s.css".into()),
                (Kind::Resource, "j.js".into()),
            ]
        );
    }

    #[test]
    fn healthy_site_passes_and_counts_external_links() {
        let f = site(&[
            (
                "https://de.wikipedia.org/",
                200,
                r#"<a href="/wiki/A">A</a><a href="/wiki/B">B</a><link rel=stylesheet href="/skin/s.css">"#,
            ),
            (
                "https://de.wikipedia.org/wiki/A",
                200,
                r#"<a href="B">B</a><a href="https://example.org/ref">ref</a>"#,
            ),
            ("https://de.wikipedia.org/wiki/B", 200, r#"<a href="A">A</a>"#),
            ("https://de.wikipedia.org/skin/s.css", 200, ""),
        ]);
        let r = check(&f, &hosts(), "de.wikipedia.org", 3, 42);
        assert_eq!(r.pages_checked, 3);
        assert_eq!(r.resources_checked, 1);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.external_links, 1);
    }

    #[test]
    fn broken_links_and_leaks_are_reported() {
        let f = site(&[
            (
                "https://de.wikipedia.org/",
                200,
                r#"<a href="/wiki/Gone">x</a><img src="https://upload.wikimedia.org/a.png">"#,
            ),
            ("https://de.wikipedia.org/wiki/Gone", 404, ""),
        ]);
        let r = check(&f, &hosts(), "de.wikipedia.org", 2, 1);
        let all = r.problems.join("\n");
        assert!(all.contains("Gone: HTTP 404"), "{all}");
        assert!(
            all.contains("upload.wikimedia.org/a.png from outside the machine"),
            "{all}"
        );
    }

    #[test]
    fn too_small_a_site_is_a_problem_and_never_loops() {
        let f = site(&[("https://de.wikipedia.org/", 200, r#"<a href="/">self</a>"#)]);
        let r = check(&f, &hosts(), "de.wikipedia.org", 50, 7);
        assert_eq!(r.pages_checked, 1);
        assert!(r.problems.iter().any(|p| p.contains("only 1 pages")));
    }

    #[test]
    fn sampling_is_deterministic_per_seed() {
        let mut pages = vec![(
            "https://de.wikipedia.org/".to_string(),
            (
                200u16,
                (0..30).map(|i| format!("<a href=\"/p{i}\">")).collect::<String>(),
            ),
        )];
        for i in 0..30 {
            pages.push((format!("https://de.wikipedia.org/p{i}"), (200, String::new())));
        }
        let f = Fake(pages.into_iter().collect());
        let a = check(&f, &hosts(), "de.wikipedia.org", 10, 5);
        let b = check(&f, &hosts(), "de.wikipedia.org", 10, 5);
        assert_eq!(
            (a.pages_checked, a.problems.len()),
            (b.pages_checked, b.problems.len())
        );
        assert_eq!(a.pages_checked, 10);
    }
}
