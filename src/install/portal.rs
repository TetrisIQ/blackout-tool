//! The portal page: a search box over all installed libraries, emergency shortcuts and a tile per
//! installed package. Written twice: `index.html` for opening as a file (absolute links to the
//! Kiwix port) and `kit.html` for `https://kit.local` (relative links, same origin as Kiwix).

use std::fs;
use std::path::Path;

use super::state::State;
use super::{DEFAULT_PORT, Paths, io_err};
use crate::plan::format_bytes;

/// Host name of the portal behind the local HTTPS front.
pub const PORTAL_HOST: &str = "kit.local";
/// The portal page as served by the front (relative links); `index.html` is the file version.
pub const FRONT_PAGE: &str = "kit.html";

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `YYYY-MM-DD HH:MM` in UTC (civil-from-days, so no date crate is needed).
pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// Quick searches for situations where nobody wants to think of keywords: (label, search terms).
/// Terms in several languages; the cross-library search simply finds whatever is installed.
pub const EMERGENCY_SEARCHES: &[(&str, &str)] = &[
    ("First aid / Erste Hilfe", "first aid Erste Hilfe"),
    ("Resuscitation / Wiederbelebung", "CPR Reanimation"),
    (
        "Safe drinking water / Trinkwasser",
        "water purification Trinkwasser",
    ),
    ("Bleeding and wounds / Wunden", "bleeding wound Blutung"),
    ("Hypothermia and heat / Unterkühlung", "hypothermia Unterkühlung"),
    (
        "Food storage / Lebensmittel haltbar",
        "food preservation Haltbarmachen",
    ),
];

/// Where the page is served from.
#[derive(Debug, Clone, Copy)]
pub enum Origin<'a> {
    /// Opened as a file; content links go to the Kiwix server on this port.
    File(u16),
    /// Served by the local HTTPS front (`https://kit.local`), same origin as Kiwix; relative links.
    Front { host: &'a str },
}

pub fn render_index(state: &State) -> String {
    render_page(state, Origin::File(state.port.unwrap_or(DEFAULT_PORT)))
}

pub fn render_page(state: &State, origin: Origin) -> String {
    let (base, title_note) = match origin {
        Origin::File(port) => (
            format!("http://localhost:{port}"),
            "Installed content, served locally by Kiwix.".to_string(),
        ),
        Origin::Front { host } => (
            String::new(),
            format!("You are on {} - no internet needed.", esc(host)),
        ),
    };
    let mut items = String::new();
    for (r, sv) in state.services() {
        let href = match origin {
            Origin::File(_) => format!("http://127.0.0.1:{}/", sv.port),
            Origin::Front { .. } => format!("https://{}/", esc(&sv.host)),
        };
        items.push_str(&format!(
            "    <li><a href=\"{href}\"><strong>{}</strong></a> (service)<br>\
             <small>{} · licence {} · login: run <code>kit status</code></small></li>\n",
            esc(&r.name),
            esc(&sv.host),
            esc(&r.license),
        ));
    }
    for r in state
        .packages
        .values()
        .filter(|r| r.kind == crate::catalog::PackageType::Zim)
    {
        let book = r
            .files
            .first()
            .map(|f| f.name.strip_suffix(".zim").unwrap_or(&f.name).to_string())
            .unwrap_or_default();
        let href = match origin {
            Origin::File(_) => format!("{base}/viewer#{}", esc(&book)),
            Origin::Front { .. } => format!("/content/{}", esc(&book)),
        };
        items.push_str(&format!(
            "    <li><a href=\"{href}\"><strong>{}</strong></a><br>\
             <small>{} · {} · licence {} · installed {}</small></li>\n",
            esc(&r.name),
            format_bytes(r.size_bytes()),
            esc(r.version.as_deref().unwrap_or("unknown version")),
            esc(&r.license),
            format_utc(r.installed_at),
        ));
    }
    if items.is_empty() {
        items.push_str("    <li>Nothing installed yet.</li>\n");
    }
    let mut quick = String::new();
    for (label, terms) in EMERGENCY_SEARCHES {
        quick.push_str(&format!(
            "    <li><a href=\"{base}/search?pattern={}\">{}</a></li>\n",
            url_encode(terms),
            esc(label)
        ));
    }
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Offline kit</title>\n<style>\n\
         :root {{ color-scheme: light dark; font-family: system-ui, sans-serif; }}\n\
         body {{ max-width: 42rem; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }}\n\
         li {{ margin: 0.8rem 0; }}\n\
         form {{ display: flex; gap: 0.5rem; }} input[type=search] {{ flex: 1; font-size: 1.2rem; padding: 0.4rem; }}\n\
         button {{ font-size: 1.1rem; padding: 0.4rem 1rem; }}\n\
         .quick {{ columns: 2; list-style: none; padding: 0; }}\n</style>\n</head>\n<body>\n\
         <h1>Offline kit</h1>\n<p>{title_note} <a href=\"{base}/\">Open the full library</a>.</p>\n\
         <form action=\"{base}/search\" method=\"get\">\n\
         <input type=\"search\" name=\"pattern\" placeholder=\"Search everything installed\" autofocus required>\n\
         <button type=\"submit\">Search</button>\n</form>\n\
         <h2>Emergency</h2>\n<ul class=\"quick\">\n{quick}</ul>\n\
         <p><small>Emergency numbers: 112 (EU) · 110 police (DE) · 911 (US/CA)</small></p>\n\
         <h2>Installed content</h2>\n<ul>\n{items}</ul>\n</body>\n</html>\n"
    )
}

/// Percent-encodes everything but unreserved characters; space becomes `+`.
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn render_json(state: &State) -> String {
    serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".into())
}

fn write_atomic(path: &Path, content: &str) -> Result<(), super::InstallError> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)
        .and_then(|()| fs::rename(&tmp, path))
        .map_err(io_err("cannot write", path))
}

/// Writes `index.html`, `kit.html` and `installed.json`, or removes the portal when nothing is installed.
pub fn write(paths: &Paths, state: &State) -> Result<(), super::InstallError> {
    let dir = paths.portal_dir();
    if state.packages.is_empty() {
        return match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("cannot remove", &dir)(e)),
        };
    }
    fs::create_dir_all(&dir).map_err(io_err("cannot create", &dir))?;
    write_atomic(&dir.join("index.html"), &render_index(state))?;
    write_atomic(
        &dir.join(FRONT_PAGE),
        &render_page(state, Origin::Front { host: PORTAL_HOST }),
    )?;
    write_atomic(&dir.join("installed.json"), &render_json(state))
}

#[cfg(test)]
mod tests {
    use super::super::state::sample_record;
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(951_782_400 + 3_600 * 13 + 60 * 5), "2000-02-29 13:05");
        assert_eq!(format_utc(1_793_364_900), "2026-10-30 12:55");
    }

    #[test]
    fn index_lists_packages_with_links() {
        let mut st = State {
            port: Some(7000),
            ..State::default()
        };
        st.packages
            .insert("a".into(), sample_record("a", "wiki_2026-10.zim", 18_585_337_585));
        let html = render_index(&st);
        assert!(
            html.contains("http://localhost:7000/viewer#wiki_2026-10"),
            "{html}"
        );
        assert!(html.contains("Package a") && html.contains("18.6 GB") && html.contains("CC-BY-SA-4.0"));
        assert!(html.contains("http://localhost:7000/\">Open the full library"));
    }

    #[test]
    fn front_page_uses_relative_links_and_searches_all_libraries() {
        let mut st = State::default();
        st.packages
            .insert("a".into(), sample_record("a", "wiki_2026-10.zim", 1));
        let html = render_page(&st, Origin::Front { host: "kit.local" });
        assert!(html.contains("href=\"/content/wiki_2026-10\""), "{html}");
        assert!(
            html.contains("<form action=\"/search\" method=\"get\">") && html.contains("name=\"pattern\"")
        );
        assert!(!html.contains("localhost"));
        assert!(html.contains("/search?pattern=first+aid+Erste+Hilfe"));
        assert!(
            html.contains("Unterk%C3%BChlung"),
            "non-ASCII terms are percent-encoded"
        );
    }

    #[test]
    fn url_encoding() {
        assert_eq!(url_encode("a b&c=ü"), "a+b%26c%3D%C3%BC");
    }

    #[test]
    fn index_escapes_markup_in_names() {
        let mut st = State::default();
        let mut r = sample_record("a", "a.zim", 1);
        r.name = "<script>alert(1)</script> & co".into();
        st.packages.insert("a".into(), r);
        let html = render_index(&st);
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; co"));
    }

    #[test]
    fn empty_state_says_so_and_default_port_is_used() {
        let html = render_index(&State::default());
        assert!(html.contains("Nothing installed yet"));
        assert!(html.contains(&format!("localhost:{DEFAULT_PORT}")));
    }

    #[test]
    fn write_creates_files_and_removes_them_when_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(d.path().join("data"), d.path().join("u")).unwrap();
        let mut st = State::default();
        st.packages.insert("a".into(), sample_record("a", "a.zim", 1));
        write(&p, &st).unwrap();
        assert!(p.portal_dir().join("kit.html").exists());
        assert!(p.portal_dir().join("index.html").exists() && p.portal_dir().join("installed.json").exists());
        write(&p, &State::default()).unwrap();
        assert!(!p.portal_dir().exists());
        write(&p, &State::default()).unwrap();
    }
}
