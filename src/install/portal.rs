//! Static landing page listing the installed packages. The real portal comes in Phase 5.

use std::fs;
use std::path::Path;

use super::state::State;
use super::{DEFAULT_PORT, Paths, io_err};
use crate::plan::format_bytes;

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

pub fn render_index(state: &State) -> String {
    let port = state.port.unwrap_or(DEFAULT_PORT);
    let base = format!("http://localhost:{port}");
    let mut items = String::new();
    for r in state.packages.values() {
        let book = r
            .files
            .first()
            .map(|f| f.name.strip_suffix(".zim").unwrap_or(&f.name).to_string())
            .unwrap_or_default();
        items.push_str(&format!(
            "    <li><a href=\"{base}/viewer#{}\"><strong>{}</strong></a><br>\
             <small>{} · {} · licence {} · installed {}</small></li>\n",
            esc(&book),
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
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>Offline kit</title>\n<style>\n\
         :root {{ color-scheme: light dark; font-family: system-ui, sans-serif; }}\n\
         body {{ max-width: 42rem; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }}\n\
         li {{ margin: 0.8rem 0; }}\n</style>\n</head>\n<body>\n\
         <h1>Offline kit</h1>\n<p>Installed content, served locally by Kiwix. \
         <a href=\"{base}/\">Open the full library</a>.</p>\n<ul>\n{items}</ul>\n</body>\n</html>\n"
    )
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

/// Writes `index.html` and `installed.json`, or removes the portal when nothing is installed.
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
        assert!(p.portal_dir().join("index.html").exists() && p.portal_dir().join("installed.json").exists());
        write(&p, &State::default()).unwrap();
        assert!(!p.portal_dir().exists());
        write(&p, &State::default()).unwrap();
    }
}
