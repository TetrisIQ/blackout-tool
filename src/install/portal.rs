//! The portal page ("Blackout", template in `assets/portal/page.html`): a search box over all installed libraries, emergency shortcuts and a tile per
//! installed package. Written twice: `index.html` for opening as a file (absolute links to the
//! Kiwix port) and `kit.html` for `https://kit.local` (relative links, same origin as Kiwix).

use std::fs;
use std::path::Path;

use sha2::Digest;

use super::pwa_icon;
use super::state::State;
use super::{DEFAULT_PORT, Paths, io_err};
use crate::catalog::manifest::PackageType;
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

const PAGE: &str = include_str!("../../assets/portal/page.html");
const READ: &str = include_str!("../../assets/portal/read.html");
const RESULTS: &str = include_str!("../../assets/portal/results.html");
const THEME_JS: &str = include_str!("../../assets/portal/theme.js");
const CSS: &str = include_str!("../../assets/portal/portal.css");
const COMMON_JS: &str = include_str!("../../assets/portal/common.js");
/// The translation engine, served from `/translate/engine/` (see `assets/translate/NOTICE.md`).
const ENGINE: [(&str, &[u8]); 3] = [
    (
        "translator-worker.js",
        include_bytes!("../../assets/translate/translator-worker.js"),
    ),
    (
        "bergamot-translator-worker.js",
        include_bytes!("../../assets/translate/bergamot-translator-worker.js"),
    ),
    (
        "bergamot-translator-worker.wasm",
        include_bytes!("../../assets/translate/bergamot-translator-worker.wasm"),
    ),
];
const TRANSLATE_JS: &str = include_str!("../../assets/portal/translate.js");

/// Font Awesome Free 6.7.2 solid icons (https://fontawesome.com, icons CC BY 4.0), inlined so the page
/// works offline: (viewBox, path elements). See `assets/portal/ICONS.md`.
mod icon {
    pub type Icon = (&'static str, &'static str);
    pub const LIBRARY: Icon = (
        "0 0 576 512",
        r#"<path d="M249.6 471.5c10.8 3.8 22.4-4.1 22.4-15.5l0-377.4c0-4.2-1.6-8.4-5-11C247.4 52 202.4 32 144 32C93.5 32 46.3 45.3 18.1 56.1C6.8 60.5 0 71.7 0 83.8L0 454.1c0 11.9 12.8 20.2 24.1 16.5C55.6 460.1 105.5 448 144 448c33.9 0 79 14 105.6 23.5zm76.8 0C353 462 398.1 448 432 448c38.5 0 88.4 12.1 119.9 22.6c11.3 3.8 24.1-4.6 24.1-16.5l0-370.3c0-12.1-6.8-23.3-18.1-27.6C529.7 45.3 482.5 32 432 32c-58.4 0-103.4 20-123 35.6c-3.3 2.6-5 6.8-5 11L304 456c0 11.4 11.7 19.3 22.4 15.5z"/>"#,
    );
    pub const VIDEO: Icon = (
        "0 0 512 512",
        r#"<path d="M0 96C0 60.7 28.7 32 64 32l384 0c35.3 0 64 28.7 64 64l0 320c0 35.3-28.7 64-64 64L64 480c-35.3 0-64-28.7-64-64L0 96zM48 368l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16l-32 0c-8.8 0-16 7.2-16 16zm368-16c-8.8 0-16 7.2-16 16l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16l-32 0zM48 240l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16l-32 0c-8.8 0-16 7.2-16 16zm368-16c-8.8 0-16 7.2-16 16l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16l-32 0zM48 112l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16L64 96c-8.8 0-16 7.2-16 16zM416 96c-8.8 0-16 7.2-16 16l0 32c0 8.8 7.2 16 16 16l32 0c8.8 0 16-7.2 16-16l0-32c0-8.8-7.2-16-16-16l-32 0zM160 128l0 64c0 17.7 14.3 32 32 32l128 0c17.7 0 32-14.3 32-32l0-64c0-17.7-14.3-32-32-32L192 96c-17.7 0-32 14.3-32 32zm32 160c-17.7 0-32 14.3-32 32l0 64c0 17.7 14.3 32 32 32l128 0c17.7 0 32-14.3 32-32l0-64c0-17.7-14.3-32-32-32l-128 0z"/>"#,
    );
    pub const MUSIC: Icon = (
        "0 0 512 512",
        r#"<path d="M499.1 6.3c8.1 6 12.9 15.6 12.9 25.7l0 72 0 264c0 44.2-43 80-96 80s-96-35.8-96-80s43-80 96-80c11.2 0 22 1.6 32 4.6L448 147 192 223.8 192 432c0 44.2-43 80-96 80s-96-35.8-96-80s43-80 96-80c11.2 0 22 1.6 32 4.6L128 200l0-72c0-14.1 9.3-26.6 22.8-30.7l320-96c9.7-2.9 20.2-1.1 28.3 5z"/>"#,
    );
    pub const AUDIOBOOK: Icon = (
        "0 0 512 512",
        r#"<path d="M256 80C149.9 80 62.4 159.4 49.6 262c9.4-3.8 19.6-6 30.4-6c26.5 0 48 21.5 48 48l0 128c0 26.5-21.5 48-48 48c-44.2 0-80-35.8-80-80l0-16 0-48 0-48C0 146.6 114.6 32 256 32s256 114.6 256 256l0 48 0 48 0 16c0 44.2-35.8 80-80 80c-26.5 0-48-21.5-48-48l0-128c0-26.5 21.5-48 48-48c10.8 0 21 2.1 30.4 6C449.6 159.4 362.1 80 256 80z"/>"#,
    );
    pub const EBOOK: Icon = (
        "0 0 512 512",
        r#"<path d="M160 96a96 96 0 1 1 192 0A96 96 0 1 1 160 96zm80 152l0 264-48.4-24.2c-20.9-10.4-43.5-17-66.8-19.3l-96-9.6C12.5 457.2 0 443.5 0 427L0 224c0-17.7 14.3-32 32-32l30.3 0c63.6 0 125.6 19.6 177.7 56zm32 264l0-264c52.1-36.4 114.1-56 177.7-56l30.3 0c17.7 0 32 14.3 32 32l0 203c0 16.4-12.5 30.2-28.8 31.8l-96 9.6c-23.2 2.3-45.9 8.9-66.8 19.3L272 512z"/>"#,
    );
    pub const MAP: Icon = (
        "0 0 576 512",
        r#"<path d="M408 120c0 54.6-73.1 151.9-105.2 192c-7.7 9.6-22 9.6-29.6 0C241.1 271.9 168 174.6 168 120C168 53.7 221.7 0 288 0s120 53.7 120 120zm8 80.4c3.5-6.9 6.7-13.8 9.6-20.6c.5-1.2 1-2.5 1.5-3.7l116-46.4C558.9 123.4 576 135 576 152l0 270.8c0 9.8-6 18.6-15.1 22.3L416 503l0-302.6zM137.6 138.3c2.4 14.1 7.2 28.3 12.8 41.5c2.9 6.8 6.1 13.7 9.6 20.6l0 251.4L32.9 502.7C17.1 509 0 497.4 0 480.4L0 209.6c0-9.8 6-18.6 15.1-22.3l122.6-49zM327.8 332c13.9-17.4 35.7-45.7 56.2-77l0 249.3L192 449.4 192 255c20.5 31.3 42.3 59.6 56.2 77c20.5 25.6 59.1 25.6 79.6 0zM288 152a40 40 0 1 0 0-80 40 40 0 1 0 0 80z"/>"#,
    );
    pub const ASSISTANT: Icon = (
        "0 0 640 512",
        r#"<path d="M320 0c17.7 0 32 14.3 32 32l0 64 120 0c39.8 0 72 32.2 72 72l0 272c0 39.8-32.2 72-72 72l-304 0c-39.8 0-72-32.2-72-72l0-272c0-39.8 32.2-72 72-72l120 0 0-64c0-17.7 14.3-32 32-32zM208 384c-8.8 0-16 7.2-16 16s7.2 16 16 16l32 0c8.8 0 16-7.2 16-16s-7.2-16-16-16l-32 0zm96 0c-8.8 0-16 7.2-16 16s7.2 16 16 16l32 0c8.8 0 16-7.2 16-16s-7.2-16-16-16l-32 0zm96 0c-8.8 0-16 7.2-16 16s7.2 16 16 16l32 0c8.8 0 16-7.2 16-16s-7.2-16-16-16l-32 0zM264 256a40 40 0 1 0 -80 0 40 40 0 1 0 80 0zm152 40a40 40 0 1 0 0-80 40 40 0 1 0 0 80zM48 224l16 0 0 192-16 0c-26.5 0-48-21.5-48-48l0-96c0-26.5 21.5-48 48-48zm544 0c26.5 0 48 21.5 48 48l0 96c0 26.5-21.5 48-48 48l-16 0 0-192 16 0z"/>"#,
    );
    pub const GAMES: Icon = (
        "0 0 640 512",
        r#"<path d="M192 64C86 64 0 150 0 256S86 448 192 448l256 0c106 0 192-86 192-192s-86-192-192-192L192 64zM496 168a40 40 0 1 1 0 80 40 40 0 1 1 0-80zM392 304a40 40 0 1 1 80 0 40 40 0 1 1 -80 0zM168 200c0-13.3 10.7-24 24-24s24 10.7 24 24l0 32 32 0c13.3 0 24 10.7 24 24s-10.7 24-24 24l-32 0 0 32c0 13.3-10.7 24-24 24s-24-10.7-24-24l0-32-32 0c-13.3 0-24-10.7-24-24s10.7-24 24-24l32 0 0-32z"/>"#,
    );
    pub const SERVER: Icon = (
        "0 0 512 512",
        r#"<path d="M64 32C28.7 32 0 60.7 0 96l0 64c0 35.3 28.7 64 64 64l384 0c35.3 0 64-28.7 64-64l0-64c0-35.3-28.7-64-64-64L64 32zm280 72a24 24 0 1 1 0 48 24 24 0 1 1 0-48zm48 24a24 24 0 1 1 48 0 24 24 0 1 1 -48 0zM64 288c-35.3 0-64 28.7-64 64l0 64c0 35.3 28.7 64 64 64l384 0c35.3 0 64-28.7 64-64l0-64c0-35.3-28.7-64-64-64L64 288zm280 72a24 24 0 1 1 0 48 24 24 0 1 1 0-48zm56 24a24 24 0 1 1 48 0 24 24 0 1 1 -48 0z"/>"#,
    );
}

/// One launcher tile.
fn tile((view_box, body): icon::Icon, label: &str, href: &str) -> String {
    format!(
        "        <a href=\"{href}\"><span class=\"ico\"><svg width=\"20\" height=\"20\" viewBox=\"{view_box}\" \
         fill=\"currentColor\" aria-hidden=\"true\">{body}</svg></span>{}</a>\n",
        esc(label)
    )
}

/// A service gets the icon of what it does when the name says so.
fn service_icon(name: &str) -> icon::Icon {
    let name = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["audiobook"]) {
        icon::AUDIOBOOK
    } else if has(&["jellyfin", "video", "film"]) {
        icon::VIDEO
    } else if has(&["navidrome", "music"]) {
        icon::MUSIC
    } else if has(&["kavita", "ebook", "comic"]) {
        icon::EBOOK
    } else {
        icon::SERVER
    }
}

/// The footer button and the dialog behind it: what is installed, as `kit status` lists it, without
/// the container services (those have logins and are in the launcher).
fn render_providers(state: &State) -> String {
    let mut rows = String::new();
    let mut total = 0;
    let mut count = 0;
    for r in state.packages.values().filter(|r| r.service.is_none()) {
        let size = r.size_bytes();
        total += size;
        count += 1;
        rows.push_str(&format!(
            "<tr><td>{}</td><td class=\"num\">{}</td><td>{}</td><td class=\"num\">{}</td><td>{}</td></tr>\n",
            esc(&r.name),
            if size == 0 {
                "-".to_string()
            } else {
                format_bytes(size)
            },
            esc(r.version.as_deref().unwrap_or("-")),
            format_utc(r.installed_at),
            esc(&r.license),
        ));
    }
    format!(
        "  <button type=\"button\" class=\"linkbtn\" id=\"providers-open\">Data providers</button>\n\
         <dialog class=\"providers\" id=\"providers\" aria-labelledby=\"providers-title\">\n\
         <header><h2 id=\"providers-title\">Data providers</h2>\
         <span class=\"sum\">{count} installed · {}</span>\
         <button type=\"button\" class=\"btn\" data-close>Close</button></header>\n\
         <div class=\"tablewrap\"><table class=\"table\">\n\
         <thead><tr><th>Name</th><th>Size</th><th>Version</th><th>Installed (UTC)</th><th>Licence</th></tr></thead>\n\
         <tbody>\n{rows}</tbody></table></div>\n</dialog>\n",
        format_bytes(total)
    )
}

/// Replaces `{{KEY}}` markers in one pass, so inserted text is never scanned for markers.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 4096);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let hit = after.find("}}").and_then(|end| {
            values
                .iter()
                .find(|(k, _)| *k == &after[..end])
                .map(|(_, v)| (end, *v))
        });
        match hit {
            Some((end, value)) => {
                out.push_str(value);
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Books "Random article" draws from: every installed library, as Kiwix names them.
fn random_books(state: &State) -> Vec<String> {
    let mut books: Vec<String> = state
        .packages
        .values()
        .filter(|r| r.kind == PackageType::Zim)
        .filter_map(|r| Some(r.files.first()?.name.strip_suffix(".zim")?.to_string()))
        .collect();
    books.sort();
    books
}

/// The launcher tiles: services, and on the front also maps and the assistant.
fn launcher_tiles(state: &State, origin: Origin) -> String {
    let mut tiles = String::new();
    if matches!(origin, Origin::File(_)) {
        tiles.push_str(&tile(icon::LIBRARY, "Library", &format!("{}/", base_of(origin))));
    }
    for (r, sv) in state.services() {
        let href = match origin {
            Origin::File(_) => format!("http://127.0.0.1:{}/", sv.port),
            // Under the portal header, in the reading frame.
            Origin::Front { .. } => format!("/read.html#https://{}/", esc(&sv.host)),
        };
        tiles.push_str(&tile(
            service_icon(&format!("{} {}", r.id, r.name)),
            // "Jellyfin (films and series)" is "Jellyfin" under its icon.
            r.name.split(" (").next().unwrap_or(&r.name),
            &href,
        ));
    }
    if matches!(origin, Origin::Front { .. }) && !map_books(state).is_empty() {
        tiles.push_str(&tile(icon::MAP, "Maps", "/read.html#/maps/"));
    }
    if !state.games().is_empty() {
        let href = match origin {
            Origin::File(_) => "games/index.html",
            // Not in the reading frame: a frame cannot install the hub as an app.
            Origin::Front { .. } => "/games/",
        };
        tiles.push_str(&tile(icon::GAMES, "Games", href));
    }
    if matches!(origin, Origin::Front { .. }) && !state.models().is_empty() {
        tiles.push_str(&tile(icon::ASSISTANT, "Assistant", "/read.html#/ai/"));
    }
    tiles
}

fn base_of(origin: Origin) -> String {
    match origin {
        Origin::File(port) => format!("http://localhost:{port}"),
        Origin::Front { .. } => String::new(),
    }
}

/// `read.html`: the Blackout header above an article, which is shown in a frame. The front links
/// every article through it (`/read.html#/content/<book>/<path>`), so the way back is always there.
pub fn render_read(state: &State, pairs: &[String]) -> String {
    let tiles = launcher_tiles(state, Origin::Front { host: PORTAL_HOST });
    fill(
        READ,
        &[
            ("CSS", CSS),
            ("THEME_JS", THEME_JS),
            ("COMMON_JS", COMMON_JS),
            ("LAUNCHER", &tiles),
            ("LAUNCHER_HIDDEN", if tiles.is_empty() { " hidden" } else { "" }),
            ("TRANSLATE_JS", TRANSLATE_JS),
            ("TRANSLATE_PAIRS", &serde_json::json!(pairs).to_string()),
            ("TRANSLATE_HIDDEN", if pairs.is_empty() { " hidden" } else { "" }),
        ],
    )
}

/// Language pairs (`de-en`) with an installed translation model: a complete folder under
/// `<data>/translate/` (model, lexicon, vocabulary).
pub fn translate_pairs(paths: &Paths) -> Vec<String> {
    let mut pairs: Vec<String> = fs::read_dir(paths.translate_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| super::translation::is_complete(&e.path()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.split_once('-').is_some_and(|(a, b)| {
                [a, b]
                    .iter()
                    .all(|c| (2..=3).contains(&c.len()) && c.bytes().all(|x| x.is_ascii_lowercase()))
            })
        })
        .collect();
    pairs.sort();
    pairs
}

/// `results.html`: the search results in the portal's look, filled in by the browser from the
/// Kiwix search on the same origin. Only the front serves it.
pub fn render_results(state: &State) -> String {
    let tiles = launcher_tiles(state, Origin::Front { host: PORTAL_HOST });
    fill(
        RESULTS,
        &[
            ("CSS", CSS),
            ("THEME_JS", THEME_JS),
            ("COMMON_JS", COMMON_JS),
            ("LAUNCHER", &tiles),
            ("LAUNCHER_HIDDEN", if tiles.is_empty() { " hidden" } else { "" }),
            ("PROVIDERS", &render_providers(state)),
        ],
    )
}

pub fn render_page(state: &State, origin: Origin) -> String {
    let (base, host, note) = match origin {
        Origin::File(port) => (
            format!("http://localhost:{port}"),
            format!("localhost:{port}"),
            format!(
                "<p class=\"note\">Installed content, served locally by Kiwix. \
                 <a href=\"http://localhost:{port}/\">Open the full library</a>.</p>\n"
            ),
        ),
        Origin::Front { host } => (String::new(), esc(host), String::new()),
    };
    let tiles = launcher_tiles(state, origin);
    // The front shows results in the portal's own page; the file version has only Kiwix's.
    let (action, field) = match origin {
        Origin::File(_) => (format!("{base}/search"), "pattern"),
        Origin::Front { .. } => ("/results.html".to_string(), "q"),
    };
    // The link works as is (first book); the page script picks a book and adds a nonce per click,
    // so neither the book nor a cached redirect repeats.
    let prefix = match origin {
        Origin::File(_) => base.clone(),
        Origin::Front { .. } => "/read.html#".to_string(),
    };
    let books = random_books(state);
    let random = books
        .first()
        .map(|first| {
            let all: Vec<String> = books.iter().map(|b| url_encode(b)).collect();
            format!(
                "      <a class=\"btn\" id=\"random\" href=\"{prefix}/random?content={}\" data-prefix=\"{prefix}\" data-books=\"{}\">Random article</a>\n",
                url_encode(first),
                all.join(",")
            )
        })
        .unwrap_or_default();
    fill(
        PAGE,
        &[
            ("CSS", CSS),
            ("THEME_JS", THEME_JS),
            ("COMMON_JS", COMMON_JS),
            ("HOST", &host),
            ("ACTION", &action),
            ("FIELD", field),
            ("LAUNCHER", &tiles),
            ("LAUNCHER_HIDDEN", if tiles.is_empty() { " hidden" } else { "" }),
            ("PROVIDERS", &render_providers(state)),
            ("RANDOM", &random),
            ("NOTE", &note),
        ],
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

const ASSISTANT: &str = include_str!("../../assets/ai/chat.html");
const ASSISTANT_JS: &str = include_str!("../../assets/ai/rag.js");

/// The assistant page under `<portal>/ai/`, or nothing when no language model is installed.
fn write_assistant(state: &State, dir: &Path) -> Result<(), super::InstallError> {
    let ai = dir.join("ai");
    let Some((_, model)) = state.models().into_iter().next() else {
        return match fs::remove_dir_all(&ai) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("cannot remove", &ai)(e)),
        };
    };
    fs::create_dir_all(&ai).map_err(io_err("cannot create", &ai))?;
    write_atomic(&ai.join("index.html"), ASSISTANT)?;
    write_atomic(&ai.join("rag.js"), ASSISTANT_JS)?;
    write_atomic(
        &ai.join("config.json"),
        &serde_json::json!({
            "model": model.name,
            "thinking": model.thinking,
            "books": survival_books(state),
        })
        .to_string(),
    )
}

/// Book names (the file name without `.zim`, as in `/content/<name>`) of the installed survival
/// packages: the assistant searches only these, so a travel guide cannot outrank first aid.
fn survival_books(state: &State) -> Vec<String> {
    let mut books: Vec<String> = state
        .packages
        .values()
        .filter(|r| r.kind == PackageType::Zim && r.category.as_deref() == Some("survival"))
        .filter_map(|r| Some(r.files.first()?.name.strip_suffix(".zim")?.to_string()))
        .collect();
    books.sort();
    books
}

const VIEWER: &str = include_str!("../../assets/maps/viewer.html");
const VENDOR: [(&str, &str); 4] = [
    ("leaflet.js", include_str!("../../assets/maps/leaflet.js")),
    ("leaflet.css", include_str!("../../assets/maps/leaflet.css")),
    ("maplibre-gl.js", include_str!("../../assets/maps/maplibre-gl.js")),
    (
        "maplibre-gl.css",
        include_str!("../../assets/maps/maplibre-gl.css"),
    ),
];
const VENDOR_BRIDGE: (&str, &str) = (
    "leaflet-maplibre-gl.js",
    include_str!("../../assets/maps/leaflet-maplibre-gl.js"),
);

/// Installed map books: `zim` packages of the category `maps` (Kiwix's `maps_en_<country>`), as
/// `(id, name, book)`. The book is the file name without `.zim`, as in `/content/<book>/`.
fn map_books(state: &State) -> Vec<(String, String, String)> {
    let mut books: Vec<_> = state
        .packages
        .values()
        .filter(|r| r.kind == PackageType::Zim && r.category.as_deref() == Some("maps"))
        .filter_map(|r| {
            let book = r.files.first()?.name.strip_suffix(".zim")?.to_string();
            Some((r.id.clone(), r.name.clone(), book))
        })
        .collect();
    books.sort();
    books
}

/// Whether the street and place index of `book` has been built.
fn has_search(paths: &Paths, book: &str) -> bool {
    crate::mapsearch::read_meta(&crate::mapsearch::index_dir(&paths.maps_dir(), book)).is_some()
}

/// `regions.json` for the viewer: the installed map books. Tiles, style, fonts and the bounds
/// all come from the book itself, served by the content server under `/content/<book>/`; the
/// search index (if built) is linked under `/maps/search/<book>/`.
fn render_regions(paths: &Paths, state: &State) -> String {
    let regions: Vec<_> = map_books(state)
        .into_iter()
        .map(|(id, name, book)| {
            let search = has_search(paths, &book).then(|| format!("/maps/search/{book}/"));
            serde_json::json!({ "id": id, "name": name, "book": book, "search": search })
        })
        .collect();
    serde_json::to_string_pretty(&regions).unwrap_or_else(|_| "[]".into())
}

/// The map viewer under `<portal>/maps/`: the page, the vendored scripts, `regions.json` and one
/// link per search index into `<data-dir>/maps/` (the front serves them as plain files).
fn write_maps(paths: &Paths, state: &State, dir: &Path) -> Result<(), super::InstallError> {
    let maps = dir.join("maps");
    if map_books(state).is_empty() {
        return match fs::remove_dir_all(&maps) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("cannot remove", &maps)(e)),
        };
    }
    let vendor = maps.join("vendor");
    fs::create_dir_all(&vendor).map_err(io_err("cannot create", &vendor))?;
    write_atomic(&maps.join("index.html"), VIEWER)?;
    for (name, text) in VENDOR.iter().chain(std::iter::once(&VENDOR_BRIDGE)) {
        write_atomic(&vendor.join(name), text)?;
    }
    write_atomic(&maps.join("regions.json"), &render_regions(paths, state))?;
    // Rebuild the links: drop stale ones, then point each existing index at the data directory.
    let search = maps.join("search");
    let _ = fs::remove_dir_all(&search);
    for (_, _, book) in map_books(state) {
        if has_search(paths, &book) {
            fs::create_dir_all(&search).map_err(io_err("cannot create", &search))?;
            let link = search.join(&book);
            let target = crate::mapsearch::index_dir(&paths.maps_dir(), &book);
            std::os::unix::fs::symlink(&target, &link).map_err(io_err("cannot link", &link))?;
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, content: &str) -> Result<(), super::InstallError> {
    write_atomic_bytes(path, content.as_bytes())
}

fn write_atomic_bytes(path: &Path, content: &[u8]) -> Result<(), super::InstallError> {
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
    write_atomic(&dir.join("results.html"), &render_results(state))?;
    write_atomic(
        &dir.join("read.html"),
        &render_read(state, &translate_pairs(paths)),
    )?;
    write_atomic(
        &dir.join(FRONT_PAGE),
        &render_page(state, Origin::Front { host: PORTAL_HOST }),
    )?;
    write_atomic(&dir.join("installed.json"), &render_json(state))?;
    write_assistant(state, &dir)?;
    write_translate(paths, &dir)?;
    write_maps(paths, state, &dir)?;
    write_games(paths, state, &dir)
}

const GAMES_HUB: &str = include_str!("../../assets/games/hub.html");
const GAMES_SW: &str = include_str!("../../assets/games/sw.js");
const GAMES_MANIFEST: &str = include_str!("../../assets/games/manifest.webmanifest");

/// What a game ships in `game.yml` (read leniently; the catalog already validated it).
#[derive(serde::Deserialize, Default)]
struct GameMeta {
    description: Option<String>,
    controls: Option<Vec<String>>,
    version: Option<String>,
    entry: Option<String>,
}

/// Files of a game that the browser needs (no tests, docs or hidden files), as paths relative to
/// the game folder, plus a revision hash over their names and contents.
fn game_files(root: &Path) -> (Vec<String>, String) {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir).into_iter().flatten().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let skip = name.starts_with('.')
                || matches!(
                    name.as_str(),
                    "test" | "tests" | "node_modules" | "README.md" | "game.yml" | "game.yaml"
                )
                || name.ends_with(".test.js");
            let Ok(kind) = e.file_type() else { continue };
            if skip || kind.is_symlink() {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if kind.is_dir() {
                walk(&e.path(), &path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(root, "", &mut files);
    let mut hash = sha2::Sha256::new();
    for f in &files {
        hash.update(f.as_bytes());
        hash.update(fs::read(root.join(f)).unwrap_or_default());
    }
    let rev: String = hash
        .finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    (files, rev)
}

/// Makes JSON safe to embed in a `<script>` element.
fn script_json(v: &serde_json::Value) -> String {
    v.to_string().replace("</", "<\\/").replace("<!--", "<\\!--")
}

/// The game hub under `<portal>/games/`: `index.html` is the library (Steam-like, data inlined),
/// `play/<id>` links to each game's folder in the data directory (the front serves it, and the
/// file version of the portal opens it from disk). Next to it lie the PWA parts (manifest, icons,
/// service worker) so a phone can install the hub and save the games for offline use. Removed
/// when no game is installed.
fn write_games(paths: &Paths, state: &State, dir: &Path) -> Result<(), super::InstallError> {
    let games = dir.join("games");
    let installed = state.games();
    if installed.is_empty() {
        return match fs::remove_dir_all(&games) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err("cannot remove", &games)(e)),
        };
    }
    let play = games.join("play");
    fs::create_dir_all(&play).map_err(io_err("cannot create", &play))?;
    for entry in fs::read_dir(&play)
        .map_err(io_err("cannot read", &play))?
        .flatten()
    {
        let _ = fs::remove_file(entry.path());
    }
    let mut data = Vec::new();
    let mut cards = String::new();
    let mut all_revs = String::new();
    for r in &installed {
        let root = paths.media_dir("games").join(&r.id);
        let link = play.join(&r.id);
        std::os::unix::fs::symlink(&root, &link).map_err(io_err("cannot link", &link))?;
        // Older releases of the games catalog ship `game.yaml`.
        let meta: GameMeta = fs::read_to_string(root.join("game.yml"))
            .or_else(|_| fs::read_to_string(root.join("game.yaml")))
            .ok()
            .and_then(|y| serde_yaml_ng::from_str(&y).ok())
            .unwrap_or_default();
        let entry = meta.entry.unwrap_or_else(|| "index.html".into());
        let (files, rev) = game_files(&root);
        all_revs.push_str(&rev);
        cards.push_str(&format!(
            "<li><a href=\"play/{}/{}\">{}</a></li>\n",
            url_encode(&r.id),
            esc(&entry),
            esc(&r.name)
        ));
        data.push(serde_json::json!({
            "id": r.id,
            "name": r.name,
            "description": meta.description.unwrap_or_default(),
            "controls": meta.controls.unwrap_or_default(),
            "version": meta.version.or_else(|| r.version.clone()),
            "license": r.license,
            "size": r.size_bytes(),
            "entry": entry,
            "rev": rev,
            "files": files,
        }));
    }
    write_atomic(
        &games.join("index.html"),
        &fill(
            GAMES_HUB,
            &[
                ("CSS", CSS),
                ("THEME_JS", THEME_JS),
                ("CARDS", &cards),
                ("DATA", &script_json(&serde_json::Value::Array(data))),
            ],
        ),
    )?;
    // The service worker changes with the games, so browsers replace the hub shell.
    let mut sw_rev = sha2::Sha256::new();
    sw_rev.update(all_revs.as_bytes());
    sw_rev.update(GAMES_HUB.as_bytes());
    let sw_rev: String = sw_rev
        .finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect();
    write_atomic(&games.join("sw.js"), &fill(GAMES_SW, &[("REV", &sw_rev)]))?;
    write_atomic(&games.join("manifest.webmanifest"), GAMES_MANIFEST)?;
    write_atomic_bytes(&games.join("icon-192.png"), &pwa_icon::png(192, false))?;
    write_atomic_bytes(&games.join("icon-512.png"), &pwa_icon::png(512, false))?;
    write_atomic_bytes(&games.join("icon-maskable-512.png"), &pwa_icon::png(512, true))?;
    write_atomic_bytes(&games.join("apple-touch-icon.png"), &pwa_icon::png(180, true))
}

/// `<portal>/translate/<pair>` links to the model folders in the data directory, so the front can
/// serve them next to the vendored engine in `translate/engine/`.
fn write_translate(paths: &Paths, dir: &Path) -> Result<(), super::InstallError> {
    let out = dir.join("translate");
    for entry in fs::read_dir(&out).into_iter().flatten().flatten() {
        if entry.file_name() != "engine" {
            let _ = fs::remove_file(entry.path());
        }
    }
    let pairs = translate_pairs(paths);
    if pairs.is_empty() {
        let _ = fs::remove_dir_all(&out);
        return Ok(());
    }
    let engine = out.join("engine");
    fs::create_dir_all(&engine).map_err(io_err("cannot create", &engine))?;
    for (name, bytes) in ENGINE {
        write_atomic_bytes(&engine.join(name), bytes)?;
    }
    for pair in pairs {
        let link = out.join(&pair);
        std::os::unix::fs::symlink(paths.translate_dir().join(&pair), &link)
            .map_err(io_err("cannot link", &link))?;
    }
    Ok(())
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
    fn index_links_to_kiwix_and_lists_no_content() {
        let mut st = State {
            port: Some(7000),
            ..State::default()
        };
        st.packages
            .insert("a".into(), sample_record("a", "wiki_2026-10.zim", 18_585_337_585));
        let html = render_index(&st);
        // The package is listed only in the providers dialog, not on the page itself.
        let page = html.replace(
            &html[html.find("<dialog").unwrap()..html.find("</dialog>").unwrap()],
            "",
        );
        assert!(
            !page.contains("<h2>Installed content") && !page.contains("Package a"),
            "{page}"
        );
        assert!(html.contains("http://localhost:7000/\">Open the full library"));
    }

    #[test]
    fn front_page_uses_relative_links_and_searches_all_libraries() {
        let mut st = State::default();
        st.packages
            .insert("a".into(), sample_record("a", "wiki_2026-10.zim", 1));
        let html = render_page(&st, Origin::Front { host: "kit.local" });
        assert!(
            !html.contains("/content/wiki_2026-10\""),
            "no content list on the home page"
        );
        assert!(
            html.contains("href=\"/read.html#/random?content=wiki_2026-10"),
            "{html}"
        );
        assert!(
            !render_index(&st).contains("/read.html"),
            "the file version has no frame page"
        );
        assert!(
            html.contains("<form class=\"search\" action=\"/results.html\" method=\"get\">")
                && html.contains("name=\"q\"")
        );
        assert!(!html.contains("localhost"));
    }

    #[test]
    fn providers_dialog_lists_packages_but_not_services() {
        let mut st = State::default();
        let mut z = sample_record("wiki", "wiki_2026-10.zim", 18_585_337_585);
        z.name = "Wiki <b>".into();
        st.packages.insert("wiki".into(), z);
        let mut sv = sample_record("jelly", "x", 1);
        sv.name = "Jellyfin".into();
        sv.service = Some(super::super::state::ServiceRecord {
            host: "jelly.kit.local".into(),
            port: 8096,
            unit: "j.service".into(),
            image: "x".into(),
        });
        st.packages.insert("jelly".into(), sv);
        for html in [
            render_index(&st),
            render_page(&st, Origin::Front { host: "kit.local" }),
            render_results(&st),
        ] {
            let dialog = &html[html.find("<dialog").expect(&html)..html.find("</dialog>").unwrap()];
            assert!(
                dialog.contains("Wiki &lt;b&gt;") && dialog.contains("18.6 GB"),
                "{dialog}"
            );
            assert!(
                dialog.contains("CC-BY-SA-4.0") && dialog.contains("1 installed"),
                "{dialog}"
            );
            assert!(!dialog.contains("Jellyfin"), "services are not data providers");
        }
    }

    #[test]
    fn every_page_has_the_theme_script_and_toggle() {
        let st = State::default();
        for html in [
            render_index(&st),
            render_page(&st, Origin::Front { host: "kit.local" }),
            render_results(&st),
            render_read(&st, &[]),
        ] {
            assert!(
                html.contains("window.kitTheme") && html.contains("id=\"theme-toggle\""),
                "{html}"
            );
            assert!(html.contains(":root[data-theme=dark]") && !html.contains("{{"));
        }
    }

    #[test]
    fn services_open_in_the_reading_frame_on_the_front_only() {
        let mut st = State::default();
        let mut sv = sample_record("jelly", "x", 1);
        sv.name = "Jellyfin (films)".into();
        sv.service = Some(super::super::state::ServiceRecord {
            host: "jelly.kit.local".into(),
            port: 8096,
            unit: "j.service".into(),
            image: "x".into(),
        });
        st.packages.insert("jelly".into(), sv);
        let front = render_page(&st, Origin::Front { host: "kit.local" });
        assert!(
            front.contains("href=\"/read.html#https://jelly.kit.local/\""),
            "{front}"
        );
        assert!(render_index(&st).contains("href=\"http://127.0.0.1:8096/\""));
    }

    #[test]
    fn both_pages_keep_their_footer() {
        let st = State::default();
        for html in [
            render_index(&st),
            render_page(&st, Origin::Front { host: "kit.local" }),
            render_results(&st),
        ] {
            assert!(
                html.contains("<footer>") && html.contains("</footer>\n</div>"),
                "{html}"
            );
        }
    }

    #[test]
    fn installed_games_get_a_launcher_tile_and_a_hub_with_play_links() {
        let d = tempfile::tempdir().unwrap();
        let p = Paths::new(d.path().join("data"), d.path().join("u")).unwrap();
        let mut st = State::default();
        st.packages.insert("a".into(), sample_record("a", "a.zim", 1));
        let front = |st: &State| render_page(st, Origin::Front { host: "kit.local" });
        assert!(!front(&st).contains("Games"), "no games, no tile");

        let mut game = sample_record("tetris", "games/tetris", 5_000);
        game.kind = PackageType::Files;
        game.name = "Tetris <b>".into();
        st.packages.insert("tetris".into(), game);
        assert!(front(&st).contains("href=\"/games/\""));
        assert!(render_index(&st).contains("href=\"games/index.html\""));

        let root = p.media_dir("games").join("tetris");
        fs::create_dir_all(root.join("test")).unwrap();
        fs::write(root.join("index.html"), "<html>").unwrap();
        fs::write(root.join("game.js"), "x").unwrap();
        fs::write(root.join("test/a.test.js"), "t").unwrap();
        fs::write(
            root.join("game.yaml"),
            "id: tetris\ntitle: T\ndescription: Stack </script> blocks\ncontrols: [\"Arrows\"]\nunknown: 1\n",
        )
        .unwrap();

        write(&p, &st).unwrap();
        let games = p.portal_dir().join("games");
        let hub = fs::read_to_string(games.join("index.html")).unwrap();
        assert!(
            hub.contains("href=\"play/tetris/index.html\"") && hub.contains("Tetris &lt;b&gt;"),
            "{hub}"
        );
        let json = hub
            .split("id=\"data\">")
            .nth(1)
            .unwrap()
            .split("</script>")
            .next()
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(data[0]["files"], serde_json::json!(["game.js", "index.html"]));
        assert_eq!(data[0]["description"], "Stack </script> blocks");
        assert_eq!(data[0]["controls"][0], "Arrows");
        assert_eq!(data[0]["rev"].as_str().unwrap().len(), 12);
        let sw = fs::read_to_string(games.join("sw.js")).unwrap();
        assert!(sw.contains("kit-hub-shell-") && !sw.contains("{{"));
        for f in [
            "manifest.webmanifest",
            "icon-192.png",
            "icon-512.png",
            "icon-maskable-512.png",
            "apple-touch-icon.png",
        ] {
            assert!(games.join(f).exists(), "{f}");
        }
        assert_eq!(
            fs::read_link(p.portal_dir().join("games/play/tetris")).unwrap(),
            p.media_dir("games").join("tetris")
        );

        st.packages.remove("tetris");
        write(&p, &st).unwrap();
        assert!(
            !p.portal_dir().join("games").exists(),
            "hub goes with the last game"
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
        r.service = Some(super::super::state::ServiceRecord {
            host: "a.kit.local".into(),
            port: 9000,
            unit: "a.service".into(),
            image: "x".into(),
        });
        st.packages.insert("a".into(), r);
        let html = render_index(&st);
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; co"));
    }

    #[test]
    fn default_port_is_used_without_state() {
        let html = render_index(&State::default());
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
        let read = fs::read_to_string(p.portal_dir().join("read.html")).unwrap();
        assert!(
            read.contains("<iframe id=\"page\"") && read.contains("kit-dark") && !read.contains("{{"),
            "{read}"
        );
        let results = fs::read_to_string(p.portal_dir().join("results.html")).unwrap();
        assert!(results.contains("/search?pattern=") && results.contains("format=xml"));
        assert!(!results.contains("{{"), "every marker is filled");
        assert!(
            !results.contains("Installed content"),
            "no content lists on the results page"
        );
        assert!(p.portal_dir().join("index.html").exists() && p.portal_dir().join("installed.json").exists());
        write(&p, &State::default()).unwrap();
        assert!(!p.portal_dir().exists());
        write(&p, &State::default()).unwrap();
    }

    #[test]
    fn maps_are_in_the_launcher_on_the_front_only_and_described_for_the_viewer() {
        let mut st = State::default();
        let mut r = sample_record("maps_en_x", "maps_en_x_2026-06.zim", 1_000_000);
        r.name = "Map X".into();
        r.kind = crate::catalog::PackageType::Zim;
        r.category = Some("maps".into());
        st.packages.insert("maps_en_x".into(), r);
        let front = render_page(&st, Origin::Front { host: "kit.local" });
        assert!(front.contains("href=\"/read.html#/maps/\""), "{front}");
        assert!(!render_index(&st).contains("href=\"/maps/\""));
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        let regions = render_regions(&paths, &st);
        assert!(regions.contains("\"book\": \"maps_en_x_2026-06\""), "{regions}");
        assert!(regions.contains("\"name\": \"Map X\""), "{regions}");
        assert!(regions.contains("\"search\": null"), "{regions}");
        write_maps(&paths, &st, dir.path()).unwrap();
        let maps = dir.path().join("maps");
        for f in [
            "index.html",
            "regions.json",
            "vendor/leaflet.js",
            "vendor/maplibre-gl.js",
            "vendor/leaflet-maplibre-gl.js",
        ] {
            assert!(maps.join(f).exists(), "{f}");
        }
        assert!(!maps.join("search").exists());
        // With a search index the viewer is told where it is and it is linked.
        let index = crate::mapsearch::index_dir(&paths.maps_dir(), "maps_en_x_2026-06");
        fs::create_dir_all(&index).unwrap();
        let meta = crate::mapsearch::Meta {
            version: crate::mapsearch::VERSION,
            zim: "u".into(),
            places: 0,
            streets: 0,
        };
        fs::write(index.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
        fs::write(index.join("68-61.json"), "[]").unwrap();
        assert!(render_regions(&paths, &st).contains("/maps/search/maps_en_x_2026-06/"));
        write_maps(&paths, &st, dir.path()).unwrap();
        assert!(maps.join("search/maps_en_x_2026-06/68-61.json").exists());
        // Without a map book the viewer is removed again; other ZIMs are not maps.
        st.packages.get_mut("maps_en_x").unwrap().category = Some("survival".into());
        write_maps(&paths, &st, dir.path()).unwrap();
        assert!(!maps.exists());
    }

    #[test]
    fn assistant_page_and_config_exist_only_with_a_model() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        let mut st = State::default();
        let mut r = sample_record("llm", "x.bin", 1);
        r.kind = crate::catalog::PackageType::Model;
        r.model = Some(super::super::state::ModelRecord {
            name: "granite4.2:3b".into(),
            size: 1,
            by_kit: true,
            thinking: true,
        });
        st.packages.insert("llm".into(), r);
        write(&paths, &st).unwrap();
        let ai = paths.portal_dir().join("ai");
        assert!(
            fs::read_to_string(ai.join("index.html"))
                .unwrap()
                .contains("Offline assistant")
        );
        assert!(
            fs::read_to_string(ai.join("rag.js"))
                .unwrap()
                .contains("parseSearch")
        );
        assert_eq!(
            fs::read_to_string(ai.join("config.json")).unwrap(),
            r#"{"books":[],"model":"granite4.2:3b","thinking":true}"#
        );
        // Only survival ZIMs are searched; Kiwix names a book after the file without its date.
        let mut zim = sample_record("readygov_en", "ready.gov_en_all_2026-10.zim", 1);
        zim.category = Some("survival".into());
        let mut other = sample_record("voyage", "wikivoyage_en_all_nopic_2026-09.zim", 1);
        other.category = Some("travel".into());
        st.packages.insert(zim.id.clone(), zim);
        st.packages.insert(other.id.clone(), other);
        assert_eq!(survival_books(&st), ["ready.gov_en_all_2026-10"]);
        let front = render_page(&st, Origin::Front { host: "kit.local" });
        assert!(front.contains("href=\"/read.html#/ai/\""), "{front}");
        assert!(!render_index(&st).contains("href=\"/ai/\""));

        st.packages
            .insert("other".into(), sample_record("other", "a.zim", 1));
        st.packages.remove("llm");
        write(&paths, &st).unwrap();
        assert!(!ai.exists());
    }
}

#[cfg(test)]
mod translate_tests {
    use super::*;

    #[test]
    fn translate_button_only_with_an_installed_pair() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        assert!(translate_pairs(&paths).is_empty());
        assert!(
            render_read(&State::default(), &[]).contains(
                "id=\"translate-btn\" aria-label=\"Translate page\" title=\"Translate page\" hidden"
            )
        );
        for (d, file) in [
            ("en-de", "model.bin"),
            ("fr-en", "model.bin"),
            ("x", "model.bin"),
            ("Bad-Dir", "model.bin"),
        ] {
            fs::create_dir_all(paths.translate_dir().join(d)).unwrap();
            for f in ["lex.bin", "vocab.spm", file] {
                fs::write(paths.translate_dir().join(d).join(f), "x").unwrap();
            }
        }
        // Only a complete folder counts; a name that is no language pair never does.
        fs::remove_file(paths.translate_dir().join("fr-en/lex.bin")).unwrap();
        let pairs = translate_pairs(&paths);
        assert_eq!(pairs, ["en-de"]);
        let html = render_read(&State::default(), &pairs);
        assert!(
            html.contains("window.kitTranslatePairs = [\"en-de\"]")
                && !html.contains(" hidden>\n    <svg width=\"18\"")
        );
        assert!(!html.contains("{{"));
    }
}
