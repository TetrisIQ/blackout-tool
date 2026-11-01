//! The Caddyfile and the systemd user unit for Caddy. Caddy listens on an unprivileged loopback
//! port; an nftables redirect (see `nft`) maps 443 to it. The certificate comes from the local CA,
//! so Caddy's own automatic HTTPS is off.

use std::path::PathBuf;

use crate::install::service::quote_exec_arg;
use crate::install::tools::s;

pub const UNIT_NAME: &str = "kit-caddy.service";
pub const MARKER: &str = "# Managed by kit (emergency-kit). Remove with: kit net disable";
pub const DEFAULT_PORT: u16 = 8443;

/// A mirrored host and the Kiwix book that answers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirror {
    pub host: String,
    /// Content id in kiwix-serve, i.e. the ZIM file name without `.zim`.
    pub book: String,
}

pub struct CaddySpec<'a> {
    pub mirrors: &'a [Mirror],
    pub cert: PathBuf,
    pub key: PathBuf,
    pub listen_port: u16,
    pub kiwix_port: u16,
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\\\"width=device-width\\\">\
         <title>{title}</title><body style=\\\"font-family:sans-serif;max-width:40em;margin:3em auto;padding:0 1em\\\">\
         <h1>{title}</h1>{body}"
    )
}

/// Everything the page body may contain is fixed text, so no escaping of user input is needed.
fn offline_page() -> String {
    page(
        "Not available offline",
        "<p>This page is not part of the offline copy on this computer.</p>\
         <p><a href=\\\"/\\\">Back to the start page</a></p>",
    )
}

fn site(host_group: &[&Mirror], spec: &CaddySpec) -> String {
    let book = &host_group[0].book;
    let addrs: Vec<String> = host_group
        .iter()
        .map(|m| format!("https://{}:{}", m.host, spec.listen_port))
        .collect();
    let upstream = format!("127.0.0.1:{}", spec.kiwix_port);
    let proxy = format!(
        "reverse_proxy {upstream} {{\n\
         \t\t\theader_up Host {upstream}\n\
         \t\t\theader_down Location ^http://[^/]+/ https://{{host}}/\n\
         \t\t}}"
    );
    format!(
        "{addrs} {{\n\
         \ttls {cert} {key}\n\
         \n\
         \t@root path /\n\
         \tredir @root /content/{book} 302\n\
         \n\
         \t@search {{\n\
         \t\tquery search=*\n\
         \t\tpath /w/index.php /wiki/Special:Search /w/\n\
         \t}}\n\
         \thandle @search {{\n\
         \t\turi query pattern {{query.search}}\n\
         \t\turi query -search\n\
         \t\turi query content {book}\n\
         \t\trewrite * /search\n\
         \t\t{proxy}\n\
         \t}}\n\
         \n\
         \t@article path_regexp ^/wiki/.+\n\
         \thandle @article {{\n\
         \t\turi path_regexp ^/wiki/(.*)$ /content/{book}/$1\n\
         \t\t{proxy}\n\
         \t}}\n\
         \n\
         \thandle /content/* {{\n\
         \t\t{proxy}\n\
         \t}}\n\
         \thandle /search* {{\n\
         \t\t{proxy}\n\
         \t}}\n\
         \thandle /skin/* {{\n\
         \t\t{proxy}\n\
         \t}}\n\
         \thandle /suggest* {{\n\
         \t\t{proxy}\n\
         \t}}\n\
         \n\
         \thandle {{\n\
         \t\trespond \"{offline}\" 404\n\
         \t}}\n\
         }}\n",
        addrs = addrs.join(", "),
        cert = quote(&spec.cert),
        key = quote(&spec.key),
        offline = offline_page(),
    )
}

fn quote(p: &std::path::Path) -> String {
    format!("\"{}\"", s(p).replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn render_caddyfile(spec: &CaddySpec) -> String {
    let mut out = format!(
        "{MARKER}\n\
         {{\n\
         \tadmin off\n\
         \tauto_https off\n\
         \tdefault_bind 127.0.0.1\n\
         }}\n"
    );
    // One site block per book: the hosts of one package share all their rules.
    let mut books: Vec<&str> = spec.mirrors.iter().map(|m| m.book.as_str()).collect();
    books.sort_unstable();
    books.dedup();
    for book in books {
        let mut group: Vec<&Mirror> = spec.mirrors.iter().filter(|m| m.book == book).collect();
        group.sort_by(|a, b| a.host.cmp(&b.host));
        out.push('\n');
        out.push_str(&site(&group, spec));
    }
    out
}

pub struct UnitSpec {
    pub caddy: PathBuf,
    pub caddyfile: PathBuf,
}

pub fn render_unit(spec: &UnitSpec) -> String {
    format!(
        "{MARKER}\n\
         [Unit]\n\
         Description=Local HTTPS front for the offline mirrors (emergency-kit)\n\
         \n\
         [Service]\n\
         ExecStart={} run --config {} --adapter caddyfile\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote_exec_arg(s(&spec.caddy)),
        quote_exec_arg(s(&spec.caddyfile)),
    )
}

pub fn is_managed(text: &str) -> bool {
    text.lines().next() == Some(MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirrors() -> Vec<Mirror> {
        let m = |h: &str, b: &str| Mirror {
            host: h.into(),
            book: b.into(),
        };
        vec![
            m("de.m.wikipedia.org", "wikipedia_de_all_nopic_2026-10"),
            m("de.wikipedia.org", "wikipedia_de_all_nopic_2026-10"),
            m("en.wikipedia.org", "wikipedia_en_all_nopic_2026-09"),
        ]
    }

    fn spec(m: &[Mirror]) -> CaddySpec<'_> {
        CaddySpec {
            mirrors: m,
            cert: "/d/net/ca/leaf.pem".into(),
            key: "/d/net/ca/leaf.key".into(),
            listen_port: 8443,
            kiwix_port: 6589,
        }
    }

    #[test]
    fn global_options_are_safe() {
        let c = render_caddyfile(&spec(&mirrors()));
        assert!(c.starts_with(MARKER));
        for want in ["admin off", "auto_https off", "default_bind 127.0.0.1"] {
            assert!(c.contains(want), "{want}");
        }
    }

    #[test]
    fn one_site_per_book_with_all_hosts() {
        let c = render_caddyfile(&spec(&mirrors()));
        assert!(c.contains("https://de.m.wikipedia.org:8443, https://de.wikipedia.org:8443 {"));
        assert!(c.contains("https://en.wikipedia.org:8443 {"));
        assert_eq!(
            c.matches("tls \"/d/net/ca/leaf.pem\" \"/d/net/ca/leaf.key\"")
                .count(),
            2
        );
    }

    #[test]
    fn articles_search_and_fallback_rules() {
        let c = render_caddyfile(&spec(&mirrors()));
        assert!(c.contains("uri path_regexp ^/wiki/(.*)$ /content/wikipedia_de_all_nopic_2026-10/$1"));
        assert!(c.contains("uri query content wikipedia_en_all_nopic_2026-09"));
        assert!(c.contains("uri query pattern {query.search}") && c.contains("uri query -search"));
        assert!(c.contains("header_up Host 127.0.0.1:6589"));
        assert!(c.contains("header_down Location ^http://[^/]+/ https://{host}/"));
        assert!(c.contains("Not available offline"));
        assert!(c.contains("\" 404"));
    }

    #[test]
    fn nothing_listens_beyond_loopback_or_proxies_elsewhere() {
        let c = render_caddyfile(&spec(&mirrors()));
        for line in c.lines().filter(|l| l.contains("reverse_proxy")) {
            assert!(line.contains("127.0.0.1:6589"), "{line}");
        }
    }

    #[test]
    fn unit_is_marked_and_quotes_paths() {
        let u = render_unit(&UnitSpec {
            caddy: "/usr/bin/caddy".into(),
            caddyfile: "/home/a b/kit/net/Caddyfile".into(),
        });
        assert!(is_managed(&u));
        assert!(u.contains(
            "ExecStart=/usr/bin/caddy run --config \"/home/a b/kit/net/Caddyfile\" --adapter caddyfile"
        ));
        assert!(!is_managed("[Unit]\n"));
    }
}
