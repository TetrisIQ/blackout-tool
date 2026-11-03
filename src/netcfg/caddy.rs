//! The Caddyfile and the systemd user unit for Caddy. Caddy listens on an unprivileged loopback
//! port; an nftables redirect (see `nft`) maps 443 to it. The certificate comes from the local CA,
//! so Caddy's own automatic HTTPS is off.

use std::path::PathBuf;

use crate::install::service::quote_exec_arg;
use crate::install::tools::s;

pub const UNIT_NAME: &str = "kit-caddy.service";
pub const MARKER: &str = "# Managed by kit (emergency-kit). Remove with: kit net disable";
pub const DEFAULT_PORT: u16 = 8443;

/// A container service reachable on loopback, served under its own host name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub host: String,
    pub port: u16,
}

pub struct CaddySpec<'a> {
    pub services: &'a [Service],
    pub cert: PathBuf,
    pub key: PathBuf,
    pub listen_port: u16,
    /// Plain-HTTP port that only redirects the portal host to HTTPS (port 80 is redirected to it).
    pub http_port: u16,
    pub kiwix_port: u16,
    /// Where `kit.html` lives; served at `https://<portal_host>/`.
    pub portal_dir: PathBuf,
    pub portal_host: String,
    /// Port of the local Ollama server; set when a language model is installed. The portal then
    /// proxies exactly `chat` and `tags` of its API to the assistant page.
    pub ollama_port: Option<u16>,
}

/// The portal's plain-HTTP port next to the HTTPS one.
pub fn http_port_for(listen_port: u16) -> u16 {
    listen_port.checked_add(1).unwrap_or(listen_port - 1)
}

fn quote(p: &std::path::Path) -> String {
    format!("\"{}\"", s(p).replace('\\', "\\\\").replace('"', "\\\""))
}

fn portal_site(spec: &CaddySpec) -> String {
    let host = &spec.portal_host;
    let upstream = format!("127.0.0.1:{}", spec.kiwix_port);
    let proxy = format!(
        "reverse_proxy {upstream} {{\n\
         \t\t\theader_up Host {upstream}\n\
         \t\t\theader_down Location ^http://[^/]+/ https://{{host}}/\n\
         \t\t}}"
    );
    let mut out = format!(
        "http://{host}:{http} {{\n\
         \tredir https://{host}{{uri}} 308\n\
         }}\n\
         \n\
         https://{host}:{https} {{\n\
         \ttls {cert} {key}\n\
         \n",
        http = spec.http_port,
        https = spec.listen_port,
        cert = quote(&spec.cert),
        key = quote(&spec.key),
    );
    // Kiwix endpoints: search across all books, content, skin.
    for path in [
        "/content/*",
        "/search*",
        "/random*",
        "/skin/*",
        "/suggest*",
        "/catalog/*",
    ] {
        out.push_str(&format!("\thandle {path} {{\n\t\t{proxy}\n\t}}\n"));
    }
    if let Some(port) = spec.ollama_port {
        // Only what the assistant page needs; pull, delete and the rest of Ollama's API stay closed.
        // The browser's Origin is dropped because Ollama rejects origins it does not know.
        out.push_str(&format!(
            "\n\t@aiapi path /ai/api/chat /ai/api/tags\n\
             \thandle @aiapi {{\n\
             \t\turi strip_prefix /ai\n\
             \t\treverse_proxy 127.0.0.1:{port} {{\n\
             \t\t\theader_up Host 127.0.0.1:{port}\n\
             \t\t\theader_up -Origin\n\
             \t\t\tflush_interval -1\n\
             \t\t}}\n\
             \t}}\n\
             \thandle /ai/api/* {{\n\
             \t\trespond \"Not allowed\" 403\n\
             \t}}\n"
        ));
    }
    out.push_str(&format!(
        "\n\thandle {{\n\
         \t\troot * {}\n\
         \t\t@front path /\n\
         \t\trewrite @front /{}\n\
         \t\tfile_server\n\
         \t}}\n\
         }}\n",
        quote(&spec.portal_dir),
        crate::install::portal::FRONT_PAGE,
    ));
    out
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
    let mut services: Vec<&Service> = spec.services.iter().collect();
    services.sort_by(|a, b| a.host.cmp(&b.host));
    for sv in services {
        // The portal shows a service under its own header, in a frame: allow exactly the portal as
        // the parent (the services forbid every frame by default), nobody else.
        out.push_str(&format!(
            "\nhttps://{host}:{https} {{\n\
             \ttls {cert} {key}\n\
             \treverse_proxy 127.0.0.1:{port} {{\n\
             \t\theader_down -X-Frame-Options\n\
             \t\theader_down Content-Security-Policy \"frame-ancestors [^;]*\" \"frame-ancestors 'self' https://{portal}\"\n\
             \t}}\n\
             }}\n",
            portal = spec.portal_host,
            host = sv.host,
            https = spec.listen_port,
            cert = quote(&spec.cert),
            key = quote(&spec.key),
            port = sv.port,
        ));
    }
    out.push('\n');
    out.push_str(&portal_site(spec));
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
         Description=Local HTTPS front for the offline kit (emergency-kit)\n\
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

    fn spec() -> CaddySpec<'static> {
        CaddySpec {
            services: &[],
            cert: "/d/net/ca/leaf.pem".into(),
            key: "/d/net/ca/leaf.key".into(),
            listen_port: 8443,
            http_port: 8444,
            kiwix_port: 6589,
            portal_dir: "/d/portal".into(),
            portal_host: "kit.local".into(),
            ollama_port: None,
        }
    }

    #[test]
    fn global_options_are_safe() {
        let c = render_caddyfile(&spec());
        assert!(c.starts_with(MARKER));
        for want in ["admin off", "auto_https off", "default_bind 127.0.0.1"] {
            assert!(c.contains(want), "{want}");
        }
    }

    #[test]
    fn services_get_their_own_vhost_proxying_to_loopback() {
        let services = [Service {
            host: "navidrome.kit.local".into(),
            port: 4533,
        }];
        let c = render_caddyfile(&CaddySpec {
            services: &services,
            ..spec()
        });
        assert!(c.contains("https://navidrome.kit.local:8443 {\n\ttls \"/d/net/ca/leaf.pem\" \"/d/net/ca/leaf.key\"\n\treverse_proxy 127.0.0.1:4533 {\n"));
        assert!(c.contains("\t\theader_down -X-Frame-Options\n"));
        assert!(
            c.contains("\"frame-ancestors 'self' https://kit.local\"\n\t}\n}"),
            "{c}"
        );
    }

    #[test]
    fn portal_is_served_from_files_with_search_proxied() {
        let c = render_caddyfile(&spec());
        assert!(c.contains("http://kit.local:8444 {\n\tredir https://kit.local{uri} 308"));
        assert!(c.contains("https://kit.local:8443 {"));
        assert!(c.contains("handle /search* {"));
        assert!(c.contains("root * \"/d/portal\"") && c.contains("rewrite @front /kit.html"));
        assert_eq!(http_port_for(8443), 8444);
        assert_eq!(http_port_for(65535), 65534);
    }

    #[test]
    fn nothing_listens_beyond_loopback_or_proxies_elsewhere() {
        let c = render_caddyfile(&spec());
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

    #[test]
    fn assistant_api_is_limited_to_chat_and_tags() {
        let without = render_caddyfile(&spec());
        assert!(!without.contains("/ai/api"));
        let c = render_caddyfile(&CaddySpec {
            ollama_port: Some(11434),
            ..spec()
        });
        assert!(c.contains("@aiapi path /ai/api/chat /ai/api/tags"), "{c}");
        assert!(c.contains("reverse_proxy 127.0.0.1:11434") && c.contains("header_up -Origin"));
        assert!(c.contains("flush_interval -1"));
        // /ai/api/chat reaches Ollama as /api/chat.
        assert!(c.contains("uri strip_prefix /ai\n"), "{c}");
        assert!(c.contains("handle /ai/api/* {\n\t\trespond \"Not allowed\" 403"));
        // The limited proxy must come before the catch-all file server.
        assert!(c.find("@aiapi").unwrap() < c.rfind("file_server").unwrap());
    }
}
