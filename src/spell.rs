//! "Did you mean ...?" for the offline search.
//!
//! Kiwix finds nothing for a misspelt word and has no spelling help, so kit builds a vocabulary
//! of its own from the installed ZIM files. It is optional and set per package, in levels of
//! effort (see [`Level`]): the more text is read, the more words are known, and the longer the
//! build takes. The index lives in `<data>/spell/<book>/` and is read by `assets/portal/spell.js`
//! in the browser; both the portal's result page and the assistant use it.
//!
//! Layout: `meta.json` and one file per word length, `w<len>.tsv`, with the lines
//! `word<TAB>count[<TAB>display]`. `word` is [`mapsearch::norm`] folded (lower case, no accents), `display`
//! is the lower-case spelling with accents when it differs. The browser fetches only the lengths
//! within two of the word it checks. `norm` and the length split are mirrored in `spell.js`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::mapsearch::norm;
use crate::zimfile::{ZimError, ZimFile};

/// Bumped whenever the index layout or the word rules change; an index of another version is rebuilt.
pub const VERSION: u32 = 1;
pub const META_FILE: &str = "meta.json";
pub const SETTINGS_FILE: &str = "settings.json";

/// Words shorter or longer than this (letters, after folding) are not kept.
pub const MIN_LEN: usize = 3;
pub const MAX_LEN: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum SpellError {
    #[error(transparent)]
    Zim(#[from] ZimError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Unusable(String),
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SpellError {
    let path = path.to_path_buf();
    move |source| SpellError::Io { path, source }
}

/// How much of a ZIM is read to learn its words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// The words of all article titles. Fast; knows names and topics, not the words inside.
    #[value(alias = "1")]
    Titles,
    /// Titles plus the text of every 8th storage cluster (about an eighth of the articles).
    #[value(alias = "2")]
    Sample,
    /// Titles plus the whole text of every article. Slowest; knows rare words as well.
    #[value(alias = "3")]
    Full,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Titles, Level::Sample, Level::Full];

    pub fn name(self) -> &'static str {
        match self {
            Level::Titles => "titles",
            Level::Sample => "sample",
            Level::Full => "full",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Level::Titles => "article titles only",
            Level::Sample => "titles and a sample (1/8) of the text",
            Level::Full => "titles and all of the text",
        }
    }

    /// Every n-th cluster has its text read (`None`: no text at all).
    fn stride(self) -> Option<u32> {
        match self {
            Level::Titles => None,
            Level::Sample => Some(8),
            Level::Full => Some(1),
        }
    }

    /// A word that is not in any title must occur this often in the text to be kept (rarer ones are mostly typos).
    fn min_body(self) -> u32 {
        match self {
            Level::Titles => 0,
            Level::Sample => 2,
            Level::Full => 3,
        }
    }

    /// The most words an index holds (the most frequent ones win); keeps the browser quick.
    pub fn cap(self) -> usize {
        match self {
            Level::Titles => 150_000,
            Level::Sample => 300_000,
            Level::Full => 500_000,
        }
    }
}

// --- settings: which package has which level ---------------------------------------------------

/// `<data>/spell/settings.json`: the level chosen per package id (no entry: off).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub packages: BTreeMap<String, Level>,
}

impl Settings {
    pub fn load(spell_dir: &Path) -> Self {
        fs::read_to_string(spell_dir.join(SETTINGS_FILE))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, spell_dir: &Path) -> Result<(), SpellError> {
        fs::create_dir_all(spell_dir).map_err(io_err(spell_dir))?;
        let path = spell_dir.join(SETTINGS_FILE);
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(self).expect("settings serialise");
        fs::write(&tmp, json)
            .and_then(|()| fs::rename(&tmp, &path))
            .map_err(io_err(&path))
    }
}

/// The book name (file name without `.zim`) of an installed package that can have the index: a
/// ZIM that is not a map (maps have their own place search).
pub fn book_of(r: &crate::install::state::Record) -> Option<String> {
    (r.kind == crate::catalog::PackageType::Zim && r.category.as_deref() != Some("maps"))
        .then(|| r.files.first()?.name.strip_suffix(".zim").map(str::to_string))
        .flatten()
}

// --- index files --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    /// UUID of the ZIM file the index was built from.
    pub zim: String,
    pub level: Level,
    pub words: usize,
    /// Size of the index files.
    pub bytes: u64,
    /// How long the build took.
    pub secs: u64,
}

pub fn index_dir(spell_dir: &Path, book: &str) -> PathBuf {
    spell_dir.join(book)
}

pub fn read_meta(dir: &Path) -> Option<Meta> {
    serde_json::from_str(&fs::read_to_string(dir.join(META_FILE)).ok()?).ok()
}

/// Whether the index in `dir` was built by this kit version, at this level, from this very ZIM file.
pub fn is_current(dir: &Path, zim: &Path, level: Level) -> bool {
    match (read_meta(dir), ZimFile::open(zim)) {
        (Some(m), Ok(z)) => m.version == VERSION && m.level == level && m.zim == z.uuid_hex(),
        _ => false,
    }
}

// --- words --------------------------------------------------------------------------------------

#[derive(Default)]
struct Stat {
    title: u32,
    body: u32,
    /// Lower-case spelling with accents, when it differs from the folded word.
    display: Option<String>,
}

type Words = HashMap<String, Stat>;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || ('\u{300}'..='\u{36f}').contains(&c)
}

/// Counts one token (a run of letters and digits). Tokens with digits, or too short or long, are ignored.
fn add_token(words: &mut Words, tok: &str, title: bool, buf: &mut String) {
    if tok.len() > 4 * MAX_LEN || tok.chars().any(char::is_numeric) {
        return;
    }
    let mut display = None;
    if tok.is_ascii() {
        buf.clear();
        buf.extend(tok.chars().map(|c| c.to_ascii_lowercase()));
    } else {
        *buf = norm(tok);
        let lower = tok.to_lowercase();
        if lower != *buf {
            display = Some(lower);
        }
    }
    let len = buf.chars().count();
    if !(MIN_LEN..=MAX_LEN).contains(&len) {
        return;
    }
    let stat = match words.get_mut(buf.as_str()) {
        Some(s) => s,
        None => words.entry(buf.clone()).or_insert(Stat {
            display,
            ..Stat::default()
        }),
    };
    if title {
        stat.title = stat.title.saturating_add(1);
    } else {
        stat.body = stat.body.saturating_add(1);
    }
}

fn add_text(words: &mut Words, text: &str, title: bool, buf: &mut String) {
    for tok in text.split(|c: char| !is_word_char(c)).filter(|t| !t.is_empty()) {
        add_token(words, tok, title, buf);
    }
}

/// `text` without HTML entities (`&amp;`, `&#8211;`), which would otherwise become the "words" amp and 8211.
fn strip_entities(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('&') {
        return text.into();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .filter(|e| *e <= 8 && tail[*e..].starts_with(';'));
        match end {
            Some(e) => {
                out.push(' ');
                rest = &tail[e + 1..];
            }
            None => {
                out.push(' ');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out.into()
}

/// Counts the words in the visible text of an HTML page (no tags, scripts, styles or comments).
fn add_html(words: &mut Words, html: &[u8], buf: &mut String) {
    // Pictures, fonts and such are not text.
    if html.iter().take(512).any(|b| *b == 0) || !html.iter().take(4096).any(|b| *b == b'<') {
        return;
    }
    let text = String::from_utf8_lossy(html);
    let s: &str = &text;
    let mut i = 0;
    while i < s.len() {
        let Some(p) = s[i..].find('<') else {
            add_text(words, &strip_entities(&s[i..]), false, buf);
            break;
        };
        add_text(words, &strip_entities(&s[i..i + p]), false, buf);
        i += p;
        let rest = &s[i + 1..];
        if rest.starts_with("!--") {
            i = s[i..].find("-->").map_or(s.len(), |e| i + e + 3);
            continue;
        }
        let name = &rest[..rest
            .find(|c: char| c == '>' || c == '/' || c.is_whitespace())
            .unwrap_or(rest.len())];
        let hidden = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
        i = rest.find('>').map_or(s.len(), |g| i + 1 + g + 1);
        if hidden {
            // Skip to the closing tag, which the loop then reads as an ordinary tag.
            loop {
                let Some(q) = s[i..].find("</") else {
                    i = s.len();
                    break;
                };
                let j = i + q;
                if s.get(j + 2..j + 2 + name.len())
                    .is_some_and(|x| x.eq_ignore_ascii_case(name))
                {
                    i = j;
                    break;
                }
                i = j + 2;
            }
        }
    }
}

/// Reads the ZIM at `level` and writes its index to `out_dir` (replacing an older one only when the
/// new one is complete). `progress(done, total)` counts clusters read (the titles come first and
/// are reported as `0/total`).
pub fn build(
    zim_path: &Path,
    out_dir: &Path,
    level: Level,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Meta, SpellError> {
    let started = Instant::now();
    let zim = ZimFile::open(zim_path)?;
    let mut words = Words::new();
    let mut buf = String::new();
    // Blobs to read per cluster (only for the clusters the level reads).
    let mut text: HashMap<u32, Vec<u32>> = HashMap::new();
    zim.visit(|_, title, cluster, blob| {
        if title.is_empty() {
            return; // a resource, not an article
        }
        add_text(&mut words, title, true, &mut buf);
        if level.stride().is_some_and(|s| cluster.is_multiple_of(s)) {
            text.entry(cluster).or_default().push(blob);
        }
    })?;
    let mut clusters: Vec<(u32, Vec<u32>)> = text.into_iter().collect();
    clusters.sort_by_key(|(c, _)| *c);
    let total = clusters.len();
    progress(0, total);

    if total > 0 {
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
        let parts: Vec<Words> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let (mut local, mut buf) = (Words::new(), String::new());
                        loop {
                            let n = next.fetch_add(1, Ordering::Relaxed);
                            let Some((id, blobs)) = clusters.get(n) else { break };
                            if let Ok(cluster) = zim.cluster(*id) {
                                for b in blobs {
                                    if let Some(bytes) = cluster.blob(*b) {
                                        add_html(&mut local, bytes, &mut buf);
                                    }
                                }
                            }
                            // One-off words of a huge book would eat the memory; they would be dropped anyway.
                            if local.len() > 3_000_000 {
                                local.retain(|_, s| s.body >= 2);
                            }
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                        local
                    })
                })
                .collect();
            while handles.iter().any(|h| !h.is_finished()) {
                progress(done.load(Ordering::Relaxed), total);
                std::thread::sleep(Duration::from_millis(200));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("a spell worker panicked"))
                .collect()
        });
        for part in parts {
            for (w, s) in part {
                let e = words.entry(w).or_default();
                e.body = e.body.saturating_add(s.body);
                if e.display.is_none() {
                    e.display = s.display;
                }
            }
        }
        progress(total, total);
    }

    let min_body = level.min_body();
    let mut kept: Vec<(String, Stat)> = words
        .into_iter()
        .filter(|(_, s)| s.title > 0 || s.body >= min_body)
        .collect();
    // The most used words first (a title word counts as many text uses); the cap cuts the tail.
    let score = |s: &Stat| u64::from(s.title) * 20 + u64::from(s.body);
    kept.sort_by(|a, b| score(&b.1).cmp(&score(&a.1)).then_with(|| a.0.cmp(&b.0)));
    kept.truncate(level.cap());
    if kept.is_empty() {
        return Err(SpellError::Unusable(format!(
            "{} has no words to learn (no article titles)",
            zim_path.display()
        )));
    }
    let meta = Meta {
        version: VERSION,
        zim: zim.uuid_hex(),
        level,
        words: kept.len(),
        bytes: 0,
        secs: 0,
    };
    write_index(out_dir, kept, meta, started)
}

fn write_index(
    out_dir: &Path,
    kept: Vec<(String, Stat)>,
    mut meta: Meta,
    started: Instant,
) -> Result<Meta, SpellError> {
    let mut by_len: BTreeMap<usize, String> = BTreeMap::new();
    for (word, s) in &kept {
        let shard = by_len.entry(word.chars().count()).or_default();
        shard.push_str(word);
        shard.push('\t');
        shard.push_str(&(s.title.saturating_mul(20).saturating_add(s.body)).to_string());
        if let Some(d) = &s.display {
            shard.push('\t');
            shard.push_str(d);
        }
        shard.push('\n');
    }
    let tmp = out_dir.with_extension("tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(io_err(&tmp))?;
    let mut bytes = 0;
    for (len, content) in &by_len {
        let path = tmp.join(format!("w{len}.tsv"));
        fs::write(&path, content).map_err(io_err(&path))?;
        bytes += content.len() as u64;
    }
    meta.bytes = bytes;
    meta.secs = started.elapsed().as_secs();
    let path = tmp.join(META_FILE);
    fs::write(&path, serde_json::to_vec(&meta).expect("meta serialises")).map_err(io_err(&path))?;
    // The old index goes only now that the new one is complete.
    match fs::remove_dir_all(out_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(out_dir)(e)),
    }
    fs::rename(&tmp, out_dir).map_err(io_err(out_dir))?;
    Ok(meta)
}

// --- estimates ----------------------------------------------------------------------------------

/// What a build is expected to cost, before it is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub secs: u64,
    pub bytes: u64,
    pub words: u64,
}

/// What one pass over a ZIM's directory says about it; the estimates are computed from this.
/// It is cached per ZIM file (`<spell>/profiles/<uuid>.json`), so only the first estimate scans.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub version: u32,
    pub zim: String,
    /// Entries with a title (articles).
    pub titles: u64,
    /// Different words in all titles.
    pub title_words: u64,
    /// Stored size of the clusters that hold articles, and of every 8th of them.
    pub text_bytes: u64,
    pub sample_bytes: u64,
    /// How long the title pass took on this machine.
    pub scan_secs: f64,
}

/// Stored ZIM bytes whose text is read and counted per second and core (measured).
const TEXT_BYTES_PER_SEC: f64 = 22_000_000.0;
/// Reading from the disk is never faster than this (a hard disk or a USB stick is much slower;
/// the first build of a cold file takes longer than the estimate).
const DISK_BYTES_PER_SEC: f64 = 300_000_000.0;
/// Bytes of index per word (word, count, separators, accented spelling now and then).
const BYTES_PER_WORD: f64 = 12.5;
/// Words the text adds to the title words: `TEXT_WORDS * sqrt(bytes read)` (fitted to real ZIM files, within about 2x).
const TEXT_WORDS: f64 = 10.0;

pub fn profile_zim(zim_path: &Path) -> Result<Profile, SpellError> {
    let started = Instant::now();
    let zim = ZimFile::open(zim_path)?;
    let mut words = Words::new();
    let mut buf = String::new();
    let mut has_text = vec![false; zim.cluster_count() as usize];
    let mut titles = 0u64;
    zim.visit(|_, title, cluster, _| {
        if title.is_empty() {
            return;
        }
        titles += 1;
        add_text(&mut words, title, true, &mut buf);
        if let Some(slot) = has_text.get_mut(cluster as usize) {
            *slot = true;
        }
    })?;
    let (mut text_bytes, mut sample_bytes) = (0, 0);
    for (n, _) in has_text.iter().enumerate().filter(|(_, h)| **h) {
        let n = n as u32;
        let size = zim.cluster_bytes(n)?;
        text_bytes += size;
        if n.is_multiple_of(8) {
            sample_bytes += size;
        }
    }
    Ok(Profile {
        version: VERSION,
        zim: zim.uuid_hex(),
        titles,
        title_words: words.len() as u64,
        text_bytes,
        sample_bytes,
        scan_secs: started.elapsed().as_secs_f64(),
    })
}

/// The profile of the ZIM, from the cache in `cache_dir` when it is of this very file.
pub fn profile(zim_path: &Path, cache_dir: Option<&Path>) -> Result<Profile, SpellError> {
    let uuid = ZimFile::open(zim_path)?.uuid_hex();
    let cached = cache_dir.map(|d| d.join(format!("{uuid}.json")));
    if let Some(p) = cached
        .as_ref()
        .and_then(|f| fs::read_to_string(f).ok())
        .and_then(|t| serde_json::from_str::<Profile>(&t).ok())
        .filter(|p| p.version == VERSION && p.zim == uuid)
    {
        return Ok(p);
    }
    let p = profile_zim(zim_path)?;
    if let Some(f) = cached {
        // A cache that cannot be written only costs a rescan.
        let _ = f
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&f, serde_json::to_vec(&p).expect("profile serialises")));
    }
    Ok(p)
}

/// The expected cost of building at `level` on a machine with `cores` processors. The numbers
/// were measured on real ZIM files; the real values depend on the disk and the processor.
pub fn estimate_for(p: &Profile, level: Level, cores: usize) -> Estimate {
    let cores = cores.clamp(1, 8) as f64;
    let mut secs = p.scan_secs.max(0.1);
    let mut words = p.title_words as f64;
    let read = match level {
        Level::Titles => 0,
        Level::Sample => p.sample_bytes,
        Level::Full => p.text_bytes,
    } as f64;
    if read > 0.0 {
        secs += (read / (TEXT_BYTES_PER_SEC * cores)).max(read / DISK_BYTES_PER_SEC);
        words += TEXT_WORDS * read.sqrt();
    }
    let words = words.min(level.cap() as f64);
    Estimate {
        secs: secs.ceil() as u64,
        bytes: (words * BYTES_PER_WORD) as u64,
        words: words as u64,
    }
}

pub fn estimate(zim_path: &Path, level: Level, cache_dir: Option<&Path>) -> Result<Estimate, SpellError> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    Ok(estimate_for(&profile(zim_path, cache_dir)?, level, cores))
}

// A tiny ZIM writer, shared with the integration tests.
#[cfg(test)]
#[path = "../tests/common/zim.rs"]
mod test_zim;

#[cfg(test)]
mod tests {
    use super::test_zim::write_zim;
    use super::*;

    fn read_shard(dir: &Path, len: usize) -> String {
        fs::read_to_string(dir.join(format!("w{len}.tsv"))).unwrap_or_default()
    }

    #[test]
    fn a_zim_is_indexed_at_each_level() {
        let tmp = tempfile::tempdir().unwrap();
        let zim = tmp.path().join("books.zim");
        write_zim(
            &zim,
            &[
                (
                    "a",
                    "Earthquake safety",
                    "<p>Drop, cover and hold on during an earthquake. Tremors.</p>",
                ),
                (
                    "b",
                    "Übelkeit",
                    "<p>Übelkeit und Erbrechen: tremors, tremors, tremors.</p><script>secretword()</script>",
                ),
                ("c", "", "<p>no title: not an article</p>"),
            ],
        );
        let dir = |l: Level| tmp.path().join(l.name());

        let m = build(&zim, &dir(Level::Titles), Level::Titles, &mut |_, _| {}).unwrap();
        assert_eq!((m.version, m.level, m.words), (VERSION, Level::Titles, 3));
        assert_eq!(read_shard(&dir(Level::Titles), 10), "earthquake\t20\n");
        assert_eq!(read_shard(&dir(Level::Titles), 8), "ubelkeit\t20\tübelkeit\n");
        assert_eq!(read_shard(&dir(Level::Titles), 6), "safety\t20\n");
        assert!(is_current(&dir(Level::Titles), &zim, Level::Titles));
        assert!(
            !is_current(&dir(Level::Titles), &zim, Level::Full),
            "another level is not current"
        );
        assert_eq!(read_meta(&dir(Level::Titles)).unwrap().bytes, m.bytes);

        // Full: the text adds words that occur at least 3 times (tremors) but neither rarer words, nor
        // script contents, nor the page without a title.
        let m = build(&zim, &dir(Level::Full), Level::Full, &mut |_, _| {}).unwrap();
        assert_eq!(m.level, Level::Full);
        assert_eq!(read_shard(&dir(Level::Full), 7), "tremors\t4\n", "1 + 3 uses");
        let all: String = (3..=24).map(|l| read_shard(&dir(Level::Full), l)).collect();
        assert!(
            !all.contains("secretword") && !all.contains("title") && !all.contains("erbrechen"),
            "{all}"
        );
        assert!(
            all.contains("earthquake\t21\n"),
            "a title word counts 20 plus its text uses: {all}"
        );

        // Sample reads every 8th cluster: only cluster 0 (page "a"), so "tremors" occurs once and is dropped.
        build(&zim, &dir(Level::Sample), Level::Sample, &mut |_, _| {}).unwrap();
        assert_eq!(read_shard(&dir(Level::Sample), 7), "");
        assert!(read_shard(&dir(Level::Sample), 10).starts_with("earthquake\t21"));

        // Rebuilding replaces the old index, and the profile (cached by file identity) matches.
        build(&zim, &dir(Level::Titles), Level::Titles, &mut |_, _| {}).unwrap();
        assert!(!tmp.path().join("titles.tmp").exists());
        let cache = tmp.path().join("profiles");
        let p = profile(&zim, Some(&cache)).unwrap();
        assert_eq!((p.titles, p.title_words), (2, 3));
        assert_eq!(
            profile(&zim, Some(&cache)).unwrap(),
            p,
            "second call reads the cache"
        );
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 1);
    }

    #[test]
    fn a_zim_without_titles_is_an_error_not_an_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        let zim = tmp.path().join("x.zim");
        write_zim(&zim, &[("a", "", "<p>text</p>")]);
        let e = build(&zim, &tmp.path().join("idx"), Level::Titles, &mut |_, _| {}).unwrap_err();
        assert!(e.to_string().contains("no words"), "{e}");
        assert!(!tmp.path().join("idx").exists());
    }

    #[test]
    fn tokens_are_folded_and_filtered() {
        let mut w = Words::new();
        let mut buf = String::new();
        add_text(&mut w, "Die Straße von München, 1999 & X2 ab", true, &mut buf);
        let mut keys: Vec<_> = w.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["die", "munchen", "strasse", "von"]);
        assert_eq!(w["munchen"].display.as_deref(), Some("münchen"));
        assert_eq!(w["strasse"].display.as_deref(), Some("straße"));
        assert_eq!(w["die"].display, None);
        assert_eq!(w["die"].title, 1);
    }

    #[test]
    fn html_gives_only_the_visible_words() {
        let mut w = Words::new();
        let mut buf = String::new();
        let html = b"<html><head><style>.x { color: red }</style><script>var hidden = 1;</SCRIPT></head>\
            <body><!-- secret --><p class=\"c\">Cool the <b>burn</b> &amp; &#8211; wait&nbsp;now</p></body></html>";
        add_html(&mut w, html, &mut buf);
        let mut keys: Vec<_> = w.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["burn", "cool", "now", "the", "wait"]);
        assert_eq!(w["burn"].body, 1);
        let mut none = Words::new();
        add_html(&mut none, b"\x89PNG\r\n\x1a\n\0\0\0 <not text>", &mut buf);
        assert!(none.is_empty(), "binary data is skipped");
    }

    #[test]
    fn settings_round_trip_and_default_to_off() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Settings::load(dir.path()).packages.is_empty());
        let mut s = Settings::default();
        s.packages.insert("wikivoyage_en".into(), Level::Sample);
        s.save(dir.path()).unwrap();
        assert_eq!(Settings::load(dir.path()), s);
        let text = fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("\"sample\""), "{text}");
    }

    #[test]
    fn estimates_grow_with_the_level() {
        let p = Profile {
            version: VERSION,
            zim: "x".into(),
            titles: 500_000,
            title_words: 80_000,
            text_bytes: 2_000_000_000,
            sample_bytes: 250_000_000,
            scan_secs: 2.0,
        };
        let e: Vec<_> = Level::ALL.iter().map(|l| estimate_for(&p, *l, 4)).collect();
        assert!(e[0].secs < e[1].secs && e[1].secs < e[2].secs, "{e:?}");
        assert!(e[0].bytes < e[1].bytes && e[1].bytes <= e[2].bytes, "{e:?}");
        assert_eq!(e[0].words, 80_000, "titles level: exactly the title words");
        assert!(e[2].words <= Level::Full.cap() as u64);
        assert!(
            estimate_for(&p, Level::Full, 1).secs > e[2].secs,
            "fewer cores take longer"
        );
    }

    #[test]
    fn levels_parse_by_name_and_number() {
        assert_eq!(Level::from_str("sample", true), Ok(Level::Sample));
        assert_eq!(Level::from_str("3", true), Ok(Level::Full));
        assert!(Level::from_str("lots", true).is_err());
    }
}
