//! Street and place search for the offline maps.
//!
//! The Kiwix map files hold OpenMapTiles vector tiles; their `place` layer has every named
//! settlement and their `transportation_name` layer every named road. kit reads both straight
//! out of the ZIM (see `zimfile`) at the highest zoom and writes a search index next to the map
//! that the viewer can query without any server code:
//!
//! - one entry per place and one per street *per locality* (a street gets the nearest place as
//!   its locality, so the many "Hauptstrasse" can be told apart),
//! - split into small shard files by the first two letters of every word of the name, so the
//!   viewer fetches only the shard of what the user types.
//!
//! `norm` and the shard names are mirrored by the viewer (`assets/maps/viewer.html`); change
//! them together.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::zimfile::{Entry, ZimError, ZimFile};

/// Bumped whenever the index layout changes; an index of another version is rebuilt.
pub const VERSION: u32 = 1;
pub const META_FILE: &str = "meta.json";

/// Streets are matched to the nearest place within this many degrees (about 8 km).
const LOCALITY_RADIUS: f64 = 0.08;
/// Grid cell used to find the nearest place quickly.
const GRID: f64 = 0.04;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Zim(#[from] ZimError),
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{0}")]
    Unusable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub version: u32,
    /// UUID of the ZIM file the index was built from.
    pub zim: String,
    pub places: usize,
    pub streets: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    name: String,
    lon: f64,
    lat: f64,
    kind: String,
    locality: String,
    rank: u32,
}

/// A map tile of the top zoom: its ZIM entry and position.
type TileRef = (Entry, u8, u32, u32);
/// The tiles of one cluster: `(blob, x, y)`.
type ClusterTiles = (u32, Vec<(u32, u32, u32)>);

/// One row of a shard file: `[name, lon, lat, kind, locality, rank]`.
type Row<'a> = (&'a str, f64, f64, &'a str, &'a str, u32);

/// Folds a name for matching: lower case, accents and `ß` etc. removed, anything that is not a
/// letter or digit becomes a space. The viewer does the same to what the user types.
pub fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars().flat_map(char::to_lowercase) {
        if ('\u{300}'..='\u{36f}').contains(&c) {
            continue; // combining marks
        }
        let folded: &str = match c {
            'ß' => "ss",
            'æ' => "ae",
            'œ' => "oe",
            'þ' => "th",
            c if c.is_alphanumeric() => {
                out.push(fold(c));
                space = false;
                continue;
            }
            _ => " ",
        };
        if folded == " " {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push_str(folded);
            space = false;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// The unaccented form of one letter (Latin-1 and Latin Extended-A; others stay as they are).
fn fold(c: char) -> char {
    const TABLE: [(&str, char); 18] = [
        ("àáâãäåāăąǎ", 'a'),
        ("çćĉċč", 'c'),
        ("ďđð", 'd'),
        ("èéêëēĕėęě", 'e'),
        ("ĝğġģ", 'g'),
        ("ĥħ", 'h'),
        ("ìíîïĩīĭįı", 'i'),
        ("ĵ", 'j'),
        ("ķ", 'k'),
        ("ĺļľŀł", 'l'),
        ("ñńņň", 'n'),
        ("òóôõöøōŏő", 'o'),
        ("ŕŗř", 'r'),
        ("śŝşšș", 's'),
        ("ţťŧț", 't'),
        ("ùúûüũūŭůűų", 'u'),
        ("ŵ", 'w'),
        ("ýÿŷ", 'y'),
    ];
    if c.is_ascii() {
        return c;
    }
    for (from, to) in TABLE {
        if from.contains(c) {
            return to;
        }
    }
    match c {
        'ź' | 'ż' | 'ž' => 'z',
        c => c,
    }
}

/// Shard file name for the first two characters of a (normalised) word: their code points in hex.
fn shard_name(word: &str) -> String {
    let hex: Vec<String> = word.chars().take(2).map(|c| format!("{:x}", c as u32)).collect();
    format!("{}.json", hex.join("-"))
}

/// Words that occur in more entries than this ("rue" in France) are not used to find an entry:
/// their shard would be many megabytes. The entry stays findable through its other words.
const COMMON_WORD: usize = 25_000;

/// The distinct words of a name that can lead to it: at least two characters each.
fn words_of(name: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for w in norm(name).split(' ').filter(|w| w.chars().count() >= 2) {
        if !seen.iter().any(|s| s == w) {
            seen.push(w.to_string());
        }
    }
    seen
}

/// The shard names an entry belongs to: one per distinct leading word pair, skipping `common`
/// words (unless that would leave the entry in no shard at all, then it is not indexed).
fn shards_of(name: &str, common: &HashSet<String>) -> Vec<String> {
    let mut shards = Vec::new();
    for w in words_of(name).iter().filter(|w| !common.contains(*w)) {
        let s = shard_name(w);
        if !shards.contains(&s) {
            shards.push(s);
        }
    }
    shards
}

// --- Mapbox vector tiles -------------------------------------------------------------------

/// A protobuf reader that only knows what vector tiles use.
struct Proto<'a> {
    buf: &'a [u8],
    pos: usize,
}

enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

impl<'a> Proto<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    /// The next `(field number, value)`, `None` at the end or on malformed data.
    fn field(&mut self) -> Option<(u32, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3).ok()?;
        let value = match key & 7 {
            0 => Wire::Varint(self.varint()?),
            2 => {
                let n = usize::try_from(self.varint()?).ok()?;
                Wire::Bytes(self.take(n)?)
            }
            1 => {
                self.take(8)?;
                Wire::Other
            }
            5 => {
                self.take(4)?;
                Wire::Other
            }
            _ => return None,
        };
        Some((field, value))
    }
}

#[derive(Clone)]
enum Val {
    Str(String),
    Num(f64),
}

fn decode_value(b: &[u8]) -> Option<Val> {
    let mut r = Proto::new(b);
    let (f, w) = r.field()?;
    Some(match (f, w) {
        (1, Wire::Bytes(s)) => Val::Str(String::from_utf8_lossy(s).into_owned()),
        (4..=6, Wire::Varint(v)) => Val::Num(match f {
            6 => unzigzag(v) as f64,
            4 => v as i64 as f64,
            _ => v as f64,
        }),
        _ => return None,
    })
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

fn layer_name(layer: &[u8]) -> Option<&str> {
    let mut r = Proto::new(layer);
    while let Some((f, w)) = r.field() {
        if let (1, Wire::Bytes(n)) = (f, w) {
            return std::str::from_utf8(n).ok();
        }
    }
    None
}

/// What one tile of one zoom/x/y position contributes.
#[derive(Default)]
struct Found {
    places: HashMap<(String, i32, i32), Item>,
    streets: HashMap<(String, i32, i32), Item>,
}

/// Position of a tile coordinate in degrees.
fn lonlat(z: u8, x: u32, y: u32, extent: f64, px: i64, py: i64) -> (f64, f64) {
    let scale = f64::from(1u32 << z);
    let (fx, fy) = (
        (f64::from(x) + px as f64 / extent) / scale,
        (f64::from(y) + py as f64 / extent) / scale,
    );
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * fy))
        .sinh()
        .atan()
        .to_degrees();
    (fx * 360.0 - 180.0, lat)
}

/// The vertices of a feature's geometry as tile coordinates, one list per line or ring.
fn geometry_lines(cmds: &[u64]) -> Vec<Vec<(i64, i64)>> {
    let mut lines: Vec<Vec<(i64, i64)>> = Vec::new();
    let mut cur: Vec<(i64, i64)> = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    let mut i = 0;
    while i < cmds.len() {
        let (id, count) = (cmds[i] & 7, (cmds[i] >> 3) as usize);
        i += 1;
        match id {
            1 | 2 => {
                for _ in 0..count {
                    let (Some(dx), Some(dy)) = (cmds.get(i), cmds.get(i + 1)) else {
                        return lines;
                    };
                    i += 2;
                    x += unzigzag(*dx);
                    y += unzigzag(*dy);
                    if id == 1 && !cur.is_empty() {
                        lines.push(std::mem::take(&mut cur));
                    }
                    cur.push((x, y));
                }
            }
            _ => {}
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Importance of a place class (smaller is bigger); the viewer lists the biggest first.
fn class_order(class: &str) -> u32 {
    match class {
        "continent" => 0,
        "country" => 1,
        "state" | "province" => 2,
        "region" | "county" => 3,
        "island" => 4,
        "city" => 5,
        "town" => 6,
        "village" => 7,
        "suburb" | "quarter" => 8,
        "hamlet" | "neighbourhood" => 9,
        _ => 10,
    }
}

/// Whether a place class can serve as the locality of a street.
fn is_locality(class: &str) -> bool {
    matches!(
        class,
        "city" | "town" | "village" | "hamlet" | "suburb" | "quarter" | "neighbourhood"
    )
}

fn cell(v: f64) -> i32 {
    (v / 0.02).floor() as i32
}

struct Feature<'a> {
    tags: Vec<usize>,
    kind: u64,
    geometry: Vec<u64>,
    keys: &'a [String],
    values: &'a [Option<Val>],
}

impl Feature<'_> {
    fn get(&self, key: &str) -> Option<&Val> {
        self.tags
            .as_chunks::<2>()
            .0
            .iter()
            .find_map(|&[k, v]| (self.keys.get(k)? == key).then(|| self.values.get(v)?.as_ref())?)
    }
    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Val::Str(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
    fn number(&self, key: &str) -> Option<u32> {
        match self.get(key)? {
            Val::Num(n) if *n >= 0.0 => Some(*n as u32),
            _ => None,
        }
    }
}

fn read_feature<'a>(feature: &[u8], keys: &'a [String], values: &'a [Option<Val>]) -> Feature<'a> {
    let mut f = Feature {
        tags: Vec::new(),
        kind: 0,
        geometry: Vec::new(),
        keys,
        values,
    };
    let mut r = Proto::new(feature);
    while let Some((n, w)) = r.field() {
        match (n, w) {
            (2, Wire::Bytes(b)) => {
                let mut p = Proto::new(b);
                while let Some(v) = p.varint() {
                    f.tags.push(v as usize);
                }
            }
            (3, Wire::Varint(v)) => f.kind = v,
            (4, Wire::Bytes(b)) => {
                let mut p = Proto::new(b);
                while let Some(v) = p.varint() {
                    f.geometry.push(v);
                }
            }
            _ => {}
        }
    }
    f
}

/// Adds the places and streets of one tile to `found`.
fn read_tile(tile: &[u8], z: u8, x: u32, y: u32, found: &mut Found) {
    let mut r = Proto::new(tile);
    while let Some((f, w)) = r.field() {
        let (3, Wire::Bytes(layer)) = (f, w) else {
            continue;
        };
        let place = match layer_name(layer) {
            Some("place") => true,
            Some("transportation_name") => false,
            _ => continue,
        };
        let (mut keys, mut values, mut features) = (Vec::new(), Vec::new(), Vec::new());
        let mut extent = 4096.0;
        let mut lr = Proto::new(layer);
        while let Some((n, w)) = lr.field() {
            match (n, w) {
                (2, Wire::Bytes(b)) => features.push(b),
                (3, Wire::Bytes(b)) => keys.push(String::from_utf8_lossy(b).into_owned()),
                (4, Wire::Bytes(b)) => values.push(decode_value(b)),
                (5, Wire::Varint(v)) if v > 0 => extent = v as f64,
                _ => {}
            }
        }
        for raw in features {
            let feat = read_feature(raw, &keys, &values);
            if place {
                add_place(&feat, z, x, y, extent, found);
            } else {
                add_street(&feat, z, x, y, extent, found);
            }
        }
    }
}

fn add_place(f: &Feature, z: u8, x: u32, y: u32, extent: f64, found: &mut Found) {
    let (Some(name), Some(class)) = (f.text("name"), f.text("class")) else {
        return;
    };
    let lines = geometry_lines(&f.geometry);
    let Some(&(px, py)) = lines.first().and_then(|l| l.first()) else {
        return;
    };
    let (lon, lat) = lonlat(z, x, y, extent, px, py);
    let rank = class_order(class) * 1000 + f.number("rank").unwrap_or(0).min(999);
    // The name and its German/English forms, so "Munich" finds München.
    let mut names = vec![name];
    for k in ["name:de", "name:en"] {
        if let Some(n) = f.text(k)
            && !names.contains(&n)
        {
            names.push(n);
        }
    }
    for n in names {
        let item = Item {
            name: n.to_string(),
            lon,
            lat,
            kind: class.to_string(),
            locality: String::new(),
            rank,
        };
        let slot = found.places.entry((n.to_string(), cell(lon), cell(lat)));
        slot.and_modify(|old| {
            if item.rank < old.rank {
                *old = item.clone();
            }
        })
        .or_insert(item);
    }
}

fn add_street(f: &Feature, z: u8, x: u32, y: u32, extent: f64, found: &mut Found) {
    if f.kind != 2 {
        return;
    }
    let (Some(name), Some(class)) = (f.text("name"), f.text("class")) else {
        return;
    };
    // The middle vertex of the first line stands for the street.
    let lines = geometry_lines(&f.geometry);
    let Some(line) = lines.first().filter(|l| !l.is_empty()) else {
        return;
    };
    let (px, py) = line[line.len() / 2];
    let (lon, lat) = lonlat(z, x, y, extent, px, py);
    found
        .streets
        .entry((name.to_string(), cell(lon), cell(lat)))
        .or_insert_with(|| Item {
            name: name.to_string(),
            lon,
            lat,
            kind: class.to_string(),
            locality: String::new(),
            rank: 0,
        });
}

/// The tile bytes of a blob: gzip-compressed or plain.
fn tile_bytes(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(blob).read_to_end(&mut out).ok()?;
        Some(out)
    } else {
        Some(blob.to_vec())
    }
}

/// `tiles/<z>/<x>/<y>.pbf` as numbers.
fn parse_tile_path(path: &str) -> Option<(u8, u32, u32)> {
    let rest = path.strip_prefix("tiles/")?.strip_suffix(".pbf")?;
    let mut it = rest.split('/');
    let (z, x, y) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    Some((z.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

// --- Building ------------------------------------------------------------------------------

/// Where the index of `book` (the ZIM's file name without `.zim`) lives.
pub fn index_dir(maps_dir: &Path, book: &str) -> PathBuf {
    maps_dir.join(format!("{book}.search"))
}

pub fn read_meta(dir: &Path) -> Option<Meta> {
    serde_json::from_str(&fs::read_to_string(dir.join(META_FILE)).ok()?).ok()
}

/// Whether the index in `dir` was built by this kit version from this very ZIM file.
pub fn is_current(dir: &Path, zim: &Path) -> bool {
    match (read_meta(dir), ZimFile::open(zim)) {
        (Some(m), Ok(z)) => m.version == VERSION && m.zim == z.uuid_hex(),
        _ => false,
    }
}

/// Reads the map file and writes its search index to `out_dir` (replacing an older one only when
/// the new one is complete). `progress(done, total)` is called now and then with cluster counts.
pub fn build(
    zim_path: &Path,
    out_dir: &Path,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Meta, SearchError> {
    let zim = ZimFile::open(zim_path)?;
    // Streets and places are read at the highest zoom: every feature is there.
    let mut tiles: Vec<TileRef> = zim
        .entries(|p| p.starts_with("tiles/"))?
        .into_iter()
        .filter_map(|e| {
            let (z, x, y) = parse_tile_path(&e.path)?;
            Some((e, z, x, y))
        })
        .collect();
    let top = tiles
        .iter()
        .map(|t| t.1)
        .max()
        .ok_or_else(|| SearchError::Unusable(format!("{} has no map tiles", zim_path.display())))?;
    tiles.retain(|t| t.1 == top);
    let mut by_cluster: HashMap<u32, Vec<(u32, u32, u32)>> = HashMap::new();
    for (e, _, x, y) in &tiles {
        by_cluster.entry(e.cluster).or_default().push((e.blob, *x, *y));
    }
    let clusters: Vec<ClusterTiles> = {
        let mut v: Vec<_> = by_cluster.into_iter().collect();
        v.sort_by_key(|(c, _)| *c);
        v
    };

    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    let results: Vec<Result<Found, SearchError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| -> Result<Found, SearchError> {
                    let mut found = Found::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((cluster, blobs)) = clusters.get(i) else {
                            return Ok(found);
                        };
                        let c = zim.cluster(*cluster)?;
                        for (blob, x, y) in blobs {
                            if let Some(tile) = c.blob(*blob).and_then(tile_bytes) {
                                read_tile(&tile, top, *x, *y, &mut found);
                            }
                        }
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        while handles.iter().any(|h| !h.is_finished()) {
            std::thread::sleep(Duration::from_millis(200));
            progress(done.load(Ordering::Relaxed), clusters.len());
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("indexing thread panicked"))
            .collect()
    });
    progress(clusters.len(), clusters.len());

    let mut found = Found::default();
    for r in results {
        let r = r?;
        for (k, v) in r.places {
            found
                .places
                .entry(k)
                .and_modify(|o| {
                    if v.rank < o.rank {
                        *o = v.clone();
                    }
                })
                .or_insert(v);
        }
        for (k, v) in r.streets {
            found.streets.entry(k).or_insert(v);
        }
    }
    let items = assemble(found);
    let meta = Meta {
        version: VERSION,
        zim: zim.uuid_hex(),
        places: items.iter().filter(|i| i.rank > 0).count(),
        streets: items.iter().filter(|i| i.rank == 0).count(),
    };
    write_index(out_dir, &items, &meta)?;
    Ok(meta)
}

/// Gives every street its locality, drops repeats and sorts: places biggest first, then streets.
fn assemble(found: Found) -> Vec<Item> {
    let mut places: Vec<Item> = found.places.into_values().collect();
    places.sort_by(|a, b| {
        a.rank
            .cmp(&b.rank)
            .then_with(|| a.name.cmp(&b.name))
            .then(a.lon.total_cmp(&b.lon))
    });
    let mut grid: HashMap<(i32, i32), Vec<&Item>> = HashMap::new();
    for p in places.iter().filter(|p| is_locality(&p.kind)) {
        grid.entry(((p.lon / GRID).floor() as i32, (p.lat / GRID).floor() as i32))
            .or_default()
            .push(p);
    }
    let reach = (LOCALITY_RADIUS / GRID).ceil() as i32;
    let nearest = |lon: f64, lat: f64| -> String {
        let (cx, cy) = ((lon / GRID).floor() as i32, (lat / GRID).floor() as i32);
        let k = lat.to_radians().cos();
        let mut best: Option<(f64, &Item)> = None;
        for gx in cx - reach..=cx + reach {
            for gy in cy - reach..=cy + reach {
                for p in grid.get(&(gx, gy)).into_iter().flatten() {
                    let d = ((p.lon - lon) * k).powi(2) + (p.lat - lat).powi(2);
                    if d <= LOCALITY_RADIUS * LOCALITY_RADIUS && best.is_none_or(|(b, _)| d < b) {
                        best = Some((d, p));
                    }
                }
            }
        }
        best.map(|(_, p)| p.name.clone()).unwrap_or_default()
    };

    let mut streets: Vec<Item> = found
        .streets
        .into_values()
        .map(|mut s| {
            s.locality = nearest(s.lon, s.lat);
            s
        })
        .collect();
    streets.sort_by(|a, b| {
        norm(&a.name)
            .cmp(&norm(&b.name))
            .then_with(|| a.locality.cmp(&b.locality))
            .then(a.lon.total_cmp(&b.lon))
            .then(a.lat.total_cmp(&b.lat))
    });
    let mut seen = HashSet::new();
    streets.retain(|s| seen.insert((s.name.clone(), s.locality.clone())));
    places.extend(streets);
    places
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> SearchError {
    let path = path.to_path_buf();
    move |source| SearchError::Io { path, source }
}

fn write_index(out_dir: &Path, items: &[Item], meta: &Meta) -> Result<(), SearchError> {
    let mut freq: HashMap<String, usize> = HashMap::new();
    for item in items {
        for w in words_of(&item.name) {
            *freq.entry(w).or_default() += 1;
        }
    }
    let common: HashSet<String> = freq
        .into_iter()
        .filter(|(_, n)| *n > COMMON_WORD)
        .map(|(w, _)| w)
        .collect();
    let mut shards: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        for s in shards_of(&item.name, &common) {
            shards.entry(s).or_default().push(i);
        }
    }
    let tmp = out_dir.with_extension("search.tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(io_err(&tmp))?;
    let round = |v: f64| (v * 1e5).round() / 1e5;
    for (name, idx) in &shards {
        let rows: Vec<Row> = idx
            .iter()
            .map(|&i| {
                let it = &items[i];
                (
                    it.name.as_str(),
                    round(it.lon),
                    round(it.lat),
                    it.kind.as_str(),
                    it.locality.as_str(),
                    it.rank,
                )
            })
            .collect();
        let json = serde_json::to_vec(&rows).expect("rows serialise");
        let path = tmp.join(name);
        fs::write(&path, json).map_err(io_err(&path))?;
    }
    let path = tmp.join(META_FILE);
    fs::write(&path, serde_json::to_vec(meta).expect("meta serialises")).map_err(io_err(&path))?;
    // The old index goes only now that the new one is complete.
    match fs::remove_dir_all(out_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(out_dir)(e)),
    }
    fs::rename(&tmp, out_dir).map_err(io_err(out_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_folded_the_way_the_viewer_does() {
        assert_eq!(norm("Hauptstraße"), "hauptstrasse");
        assert_eq!(norm("Straße des 17. Juni"), "strasse des 17 juni");
        assert_eq!(norm("München"), "munchen");
        assert_eq!(norm("Şişli  Cd."), "sisli cd");
        assert_eq!(norm("Łódź"), "lodz");
        assert_eq!(norm("Øster Allé"), "oster alle");
        assert_eq!(norm("  --  "), "");
        assert_eq!(norm("北京市"), "北京市");
    }

    #[test]
    fn shards_are_named_by_the_first_two_letters_of_each_word() {
        assert_eq!(shard_name("hauptstrasse"), "68-61.json");
        let none = HashSet::new();
        let s = shards_of("Straße des 17. Juni", &none);
        assert_eq!(s, ["73-74.json", "64-65.json", "31-37.json", "6a-75.json"]);
        // one-letter words are not indexed, and a name is listed once per shard
        assert_eq!(shards_of("A B Hauptstraße Hauptweg", &none), ["68-61.json"]);
        assert!(shards_of("X", &none).is_empty());
        // a very common word does not lead to the entry, the other words still do
        let common: HashSet<String> = ["strasse".to_string(), "des".to_string()].into();
        assert_eq!(
            shards_of("Straße des 17. Juni", &common),
            ["31-37.json", "6a-75.json"]
        );
        assert!(shards_of("Straße", &common).is_empty());
    }

    #[test]
    fn geometry_is_decoded_to_vertices() {
        // MoveTo(1) (25,17); LineTo(2) +(0,8) +(8,0); then a second line.
        let cmds = [9, 50, 34, 18, 0, 16, 16, 0, 9, 2, 2];
        let lines = geometry_lines(&cmds);
        assert_eq!(lines[0], [(25, 17), (25, 25), (33, 25)]);
        assert_eq!(lines[1], [(34, 26)]);
    }

    #[test]
    fn tile_paths_and_positions() {
        assert_eq!(parse_tile_path("tiles/14/8514/5510.pbf"), Some((14, 8514, 5510)));
        assert_eq!(parse_tile_path("tiles/14/8514.pbf"), None);
        assert_eq!(parse_tile_path("fonts/Noto Sans/0-255.pbf"), None);
        let (lon, lat) = lonlat(0, 0, 0, 4096.0, 2048, 2048);
        assert!(lon.abs() < 1e-9 && lat.abs() < 1e-9);
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/maps/mini.zim");

    type TestRow = (String, f64, f64, String, String, u32);

    fn rows(dir: &Path, shard: &str) -> Vec<TestRow> {
        serde_json::from_str(&fs::read_to_string(dir.join(shard)).unwrap()).unwrap()
    }

    #[test]
    fn indexes_places_and_streets_of_a_real_extract() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("mini.search");
        let mut last = (0, 0);
        let meta = build(Path::new(FIXTURE), &out, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(last.0, last.1, "progress ends at 100%");
        assert!(meta.places > 0 && meta.streets > 100, "{meta:?}");
        assert_eq!(read_meta(&out), Some(meta));
        assert!(is_current(&out, Path::new(FIXTURE)));

        // A street is found through the shard of one of its words, with a locality and a
        // position inside the extract (Luxembourg City).
        let shard = rows(&out, &shard_name("rue"));
        let street = shard
            .iter()
            .find(|r| r.5 == 0 && norm(&r.0).split(' ').any(|w| w == "rue"))
            .expect("a rue");
        assert!(
            (6.0..6.3).contains(&street.1) && (49.5..49.7).contains(&street.2),
            "{street:?}"
        );
        assert!(!street.4.is_empty(), "the street has a locality: {street:?}");

        // A place is found by name, and places sort before streets.
        let lux = rows(&out, &shard_name("luxembourg"));
        assert!(
            lux.iter()
                .any(|r| r.0 == "Luxembourg" && r.5 > 0 && r.3 == "city"),
            "{lux:?}"
        );

        // The same street name in one locality is listed once.
        let mut seen = HashSet::new();
        for r in shard.iter().filter(|r| r.5 == 0) {
            assert!(seen.insert((r.0.clone(), r.4.clone())), "duplicate {r:?}");
        }
    }

    #[test]
    fn a_rebuild_replaces_the_old_index_and_a_foreign_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("mini.search");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("stale.json"), "[]").unwrap();
        build(Path::new(FIXTURE), &out, &mut |_, _| {}).unwrap();
        assert!(!out.join("stale.json").exists());
        assert!(!tmp.path().join("mini.search.tmp").exists());

        let bad = tmp.path().join("x.zim");
        fs::write(
            &bad,
            b"not a zim file at all, but long enough to hold a header .....................",
        )
        .unwrap();
        let e = build(&bad, &tmp.path().join("y.search"), &mut |_, _| {}).unwrap_err();
        assert!(e.to_string().contains("not a usable ZIM"), "{e}");
        assert!(!is_current(&out, &bad));
        assert!(!tmp.path().join("y.search").exists());
    }
}
