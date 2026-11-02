//! Place-name index for the offline map viewer.
//!
//! Protomaps builds carry no search data, but their `places` layer holds every named city, town,
//! village and neighbourhood as a point. kit reads that layer out of an installed `.pmtiles` file
//! (PMTiles v3 container, gzip-compressed Mapbox vector tiles) and writes a small JSON list the
//! viewer can search without a network. Streets and house numbers are not covered.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;

use flate2::read::GzDecoder;
use serde::Serialize;

/// Tiles are read at this zoom at most: a place appears in every tile from its own minimum zoom
/// upwards, so one zoom level lists it once, and 12 still carries the neighbourhoods.
pub const SCAN_ZOOM: u8 = 12;

/// Two entries with the same name closer than this (degrees) are the same place seen in two
/// neighbouring tiles (features near an edge are repeated in both).
const SAME_PLACE: f64 = 0.004;

#[derive(Debug, thiserror::Error)]
pub enum GeoError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path} is not a usable PMTiles v3 file: {reason}")]
    Format { path: String, reason: String },
}

/// One entry of the index. Serialised as `[name, lon, lat, kind, population]`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Place(pub String, pub f64, pub f64, pub String, pub u64);

impl Place {
    pub fn name(&self) -> &str {
        &self.0
    }
    pub fn lon(&self) -> f64 {
        self.1
    }
    pub fn lat(&self) -> f64 {
        self.2
    }
    pub fn kind(&self) -> &str {
        &self.3
    }
    pub fn population(&self) -> u64 {
        self.4
    }
}

/// The index of `path` as compact JSON, most important places first.
pub fn places_json(path: &Path) -> Result<String, GeoError> {
    let places = read_places(path, SCAN_ZOOM)?;
    Ok(serde_json::to_string(&places).expect("places serialise"))
}

/// All named places of the file, read at `min(zoom, max zoom of the file)`; biggest first.
pub fn read_places(path: &Path, zoom: u8) -> Result<Vec<Place>, GeoError> {
    let disp = path.display().to_string();
    let bad = |reason: &str| GeoError::Format {
        path: disp.clone(),
        reason: reason.into(),
    };
    let file = File::open(path).map_err(|source| GeoError::Io {
        path: disp.clone(),
        source,
    })?;
    let io = |source| GeoError::Io {
        path: disp.clone(),
        source,
    };
    let mut head = [0u8; 127];
    file.read_exact_at(&mut head, 0).map_err(io)?;
    let h = Header::parse(&head).ok_or_else(|| bad("bad header"))?;
    if h.tile_type != 1 {
        return Err(bad("the tiles are not vector tiles"));
    }
    let z = zoom.min(h.max_zoom);
    let (first_id, last_id) = (tile_id_start(z), tile_id_start(z + 1));

    let mut tiles = Vec::new();
    collect_tiles(
        &file,
        &h,
        (h.root_offset, h.root_len),
        (first_id, last_id),
        &mut tiles,
        0,
    )
    .map_err(|e| match e {
        Walk::Io(e) => io(e),
        Walk::Bad(r) => bad(&r),
    })?;

    let mut found: Vec<Place> = Vec::new();
    let mut buf = Vec::new();
    for t in &tiles {
        buf.resize(t.length as usize, 0);
        file.read_exact_at(&mut buf, h.data_offset + t.offset)
            .map_err(io)?;
        let raw = decompress(&buf, h.tile_compression).map_err(|e| bad(&e))?;
        for id in t.id..t.id + u64::from(t.run.max(1)) {
            if id < first_id || id >= last_id {
                continue;
            }
            let (_, x, y) = id_to_zxy(id);
            places_of_tile(&raw, z, x, y, &mut found);
        }
    }
    Ok(dedupe(found))
}

struct Header {
    root_offset: u64,
    root_len: u64,
    leaf_offset: u64,
    data_offset: u64,
    internal_compression: u8,
    tile_compression: u8,
    tile_type: u8,
    max_zoom: u8,
}

impl Header {
    fn parse(b: &[u8; 127]) -> Option<Self> {
        if &b[..7] != b"PMTiles" || b[7] != 3 {
            return None;
        }
        let u = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"));
        Some(Self {
            root_offset: u(8),
            root_len: u(16),
            leaf_offset: u(40),
            data_offset: u(56),
            internal_compression: b[97],
            tile_compression: b[98],
            tile_type: b[99],
            max_zoom: b[101],
        })
    }
}

enum Walk {
    Io(std::io::Error),
    Bad(String),
}

struct TileRef {
    id: u64,
    run: u32,
    offset: u64,
    length: u32,
}

struct Entry {
    id: u64,
    run: u32,
    offset: u64,
    length: u32,
}

/// Collects the tile entries whose ids may fall into `[first, last)`, walking leaf directories.
fn collect_tiles(
    file: &File,
    h: &Header,
    dir: (u64, u64),
    range: (u64, u64),
    out: &mut Vec<TileRef>,
    depth: u8,
) -> Result<(), Walk> {
    if depth > 3 {
        return Err(Walk::Bad("directories nest too deeply".into()));
    }
    let mut raw = vec![0u8; dir.1 as usize];
    file.read_exact_at(&mut raw, dir.0).map_err(Walk::Io)?;
    let raw = decompress(&raw, h.internal_compression).map_err(Walk::Bad)?;
    let entries = parse_directory(&raw).ok_or_else(|| Walk::Bad("corrupt directory".into()))?;
    for (i, e) in entries.iter().enumerate() {
        // An entry covers ids up to the next entry's id (a leaf pointer) or its run.
        let end = if e.run == 0 {
            entries.get(i + 1).map_or(u64::MAX, |n| n.id)
        } else {
            e.id + u64::from(e.run)
        };
        if end <= range.0 || e.id >= range.1 {
            continue;
        }
        if e.run == 0 {
            collect_tiles(
                file,
                h,
                (h.leaf_offset + e.offset, u64::from(e.length)),
                range,
                out,
                depth + 1,
            )?;
        } else {
            out.push(TileRef {
                id: e.id,
                run: e.run,
                offset: e.offset,
                length: e.length,
            });
        }
    }
    Ok(())
}

fn parse_directory(raw: &[u8]) -> Option<Vec<Entry>> {
    let mut r = Proto::new(raw);
    let n = usize::try_from(r.varint()?).ok()?;
    if n > raw.len() {
        return None;
    }
    let mut entries: Vec<Entry> = (0..n)
        .map(|_| Entry {
            id: 0,
            run: 0,
            offset: 0,
            length: 0,
        })
        .collect();
    let mut id = 0u64;
    for e in &mut entries {
        id = id.checked_add(r.varint()?)?;
        e.id = id;
    }
    for e in &mut entries {
        e.run = u32::try_from(r.varint()?).ok()?;
    }
    for e in &mut entries {
        e.length = u32::try_from(r.varint()?).ok()?;
    }
    let mut prev_end = 0u64;
    for e in &mut entries {
        let o = r.varint()?;
        e.offset = if o == 0 { prev_end } else { o - 1 };
        prev_end = e.offset + u64::from(e.length);
    }
    Some(entries)
}

fn decompress(data: &[u8], kind: u8) -> Result<Vec<u8>, String> {
    match kind {
        1 | 0 => Ok(data.to_vec()),
        2 => {
            let mut out = Vec::new();
            GzDecoder::new(data)
                .read_to_end(&mut out)
                .map_err(|e| format!("cannot unpack gzip data: {e}"))?;
            Ok(out)
        }
        k => Err(format!("unsupported compression {k}")),
    }
}

/// First tile id of zoom `z`: the number of tiles on all lower zooms.
fn tile_id_start(z: u8) -> u64 {
    ((1u64 << (2 * u32::from(z))) - 1) / 3
}

/// Tile id (Hilbert curve over the zooms, as in the PMTiles spec) to `(z, x, y)`.
fn id_to_zxy(id: u64) -> (u8, u32, u32) {
    let mut z = 0u8;
    while tile_id_start(z + 1) <= id {
        z += 1;
    }
    let mut t = id - tile_id_start(z);
    let n = 1u64 << z;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (z, x as u32, y as u32)
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
            6 => ((v >> 1) as i64 ^ -((v & 1) as i64)) as f64,
            4 => v as i64 as f64,
            _ => v as f64,
        }),
        _ => return None,
    })
}

/// The `places` layer of one tile, appended to `out` with real coordinates.
fn places_of_tile(tile: &[u8], z: u8, x: u32, y: u32, out: &mut Vec<Place>) {
    let mut r = Proto::new(tile);
    while let Some((f, w)) = r.field() {
        if let (3, Wire::Bytes(layer)) = (f, w)
            && layer_name(layer) == Some("places")
        {
            read_places_layer(layer, z, x, y, out);
        }
    }
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

fn read_places_layer(layer: &[u8], z: u8, x: u32, y: u32, out: &mut Vec<Place>) {
    let (mut keys, mut values, mut features) = (Vec::new(), Vec::new(), Vec::new());
    let mut extent = 4096.0;
    let mut r = Proto::new(layer);
    while let Some((f, w)) = r.field() {
        match (f, w) {
            (2, Wire::Bytes(b)) => features.push(b),
            (3, Wire::Bytes(b)) => keys.push(String::from_utf8_lossy(b).into_owned()),
            (4, Wire::Bytes(b)) => values.push(decode_value(b)),
            (5, Wire::Varint(v)) if v > 0 => extent = v as f64,
            _ => {}
        }
    }
    for feature in features {
        if let Some(p) = read_feature(feature, &keys, &values, extent, z, x, y) {
            out.extend(p);
        }
    }
}

fn read_feature(
    feature: &[u8],
    keys: &[String],
    values: &[Option<Val>],
    extent: f64,
    z: u8,
    x: u32,
    y: u32,
) -> Option<Vec<Place>> {
    let (mut tags, mut geometry, mut kind) = (Vec::new(), Vec::new(), 0);
    let mut r = Proto::new(feature);
    while let Some((f, w)) = r.field() {
        match (f, w) {
            (2, Wire::Bytes(b)) => {
                let mut p = Proto::new(b);
                while let Some(v) = p.varint() {
                    tags.push(v as usize);
                }
            }
            (3, Wire::Varint(v)) => kind = v,
            (4, Wire::Bytes(b)) => {
                let mut p = Proto::new(b);
                while let Some(v) = p.varint() {
                    geometry.push(v);
                }
            }
            _ => {}
        }
    }
    if kind != 1 {
        return None;
    }
    // Properties we use, by key.
    let mut names: HashMap<&str, String> = HashMap::new();
    let mut population = 0u64;
    let mut place_kind = String::new();
    let mut detail = String::new();
    for &[k, v] in tags.as_chunks::<2>().0 {
        let key = keys.get(k)?.as_str();
        let value = values.get(v)?.as_ref();
        match (key, value) {
            ("name" | "name:de" | "name:en", Some(Val::Str(s))) if !s.is_empty() => {
                names.insert(key, s.clone());
            }
            ("population", Some(Val::Num(n))) if *n > 0.0 => population = *n as u64,
            ("kind", Some(Val::Str(s))) => place_kind = s.clone(),
            ("kind_detail", Some(Val::Str(s))) => detail = s.clone(),
            _ => {}
        }
    }
    let name = names.get("name")?.clone();
    // First `MoveTo` of the geometry is the point.
    let (cmd, dx, dy) = (*geometry.first()?, *geometry.get(1)?, *geometry.get(2)?);
    if cmd & 7 != 1 {
        return None;
    }
    let zz = |v: u64| ((v >> 1) as i64 ^ -((v & 1) as i64)) as f64;
    let scale = f64::from(1u32 << z);
    let (fx, fy) = (
        (f64::from(x) + zz(dx) / extent) / scale,
        (f64::from(y) + zz(dy) / extent) / scale,
    );
    let lon = fx * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * fy))
        .sinh()
        .atan()
        .to_degrees();
    let round = |v: f64| (v * 1e5).round() / 1e5;
    let label = if detail.is_empty() { place_kind } else { detail };
    // The name and the German/English forms, so "Munich" finds München.
    let mut all = vec![name.clone()];
    for k in ["name:de", "name:en"] {
        if let Some(n) = names.get(k)
            && !all.contains(n)
        {
            all.push(n.clone());
        }
    }
    Some(
        all.into_iter()
            .map(|n| Place(n, round(lon), round(lat), label.clone(), population))
            .collect(),
    )
}

/// Drops repeats (same name, nearly the same spot) and sorts biggest first.
fn dedupe(mut places: Vec<Place>) -> Vec<Place> {
    places.sort_by(|a, b| a.name().cmp(b.name()).then(a.lon().total_cmp(&b.lon())));
    let mut kept: Vec<Place> = Vec::new();
    // `kept` ends with the group of the current name; only that group is searched.
    let mut group_start = 0;
    for p in places {
        if kept.get(group_start).is_none_or(|k| k.name() != p.name()) {
            group_start = kept.len();
        }
        let twin = kept[group_start..]
            .iter_mut()
            .find(|k| (k.lon() - p.lon()).abs() < SAME_PLACE && (k.lat() - p.lat()).abs() < SAME_PLACE);
        match twin {
            Some(k) if k.population() >= p.population() => {}
            Some(k) => *k = p,
            None => kept.push(p),
        }
    }
    kept.sort_by(|a, b| {
        b.population()
            .cmp(&a.population())
            .then_with(|| a.name().cmp(b.name()))
    });
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/maps/mitte.pmtiles");

    #[test]
    fn tile_ids_follow_the_pmtiles_spec() {
        // Examples from the PMTiles v3 spec.
        assert_eq!(id_to_zxy(0), (0, 0, 0));
        assert_eq!(id_to_zxy(1), (1, 0, 0));
        assert_eq!(id_to_zxy(2), (1, 0, 1));
        assert_eq!(id_to_zxy(3), (1, 1, 1));
        assert_eq!(id_to_zxy(4), (1, 1, 0));
        assert_eq!(id_to_zxy(5), (2, 0, 0));
        assert_eq!(tile_id_start(2), 5);
    }

    #[test]
    fn varints_and_zigzag() {
        let mut r = Proto::new(&[0x96, 0x01, 0x05]);
        assert_eq!(r.varint(), Some(150));
        assert_eq!(r.varint(), Some(5));
        assert_eq!(r.varint(), None);
        // Truncated continuation byte.
        assert_eq!(Proto::new(&[0x80]).varint(), None);
    }

    #[test]
    fn nearby_repeats_collapse_and_the_bigger_one_wins() {
        let p = |n: &str, lon: f64, pop: u64| Place(n.into(), lon, 52.0, "town".into(), pop);
        let out = dedupe(vec![
            p("A", 13.0, 0),
            p("A", 13.001, 500),
            p("A", 14.0, 10),
            p("B", 13.0, 20),
        ]);
        let names: Vec<_> = out.iter().map(|x| (x.name(), x.population())).collect();
        assert_eq!(names, [("A", 500), ("B", 20), ("A", 10)]);
    }

    #[test]
    fn reads_the_places_of_a_real_extract() {
        let places = read_places(Path::new(FIXTURE), SCAN_ZOOM).unwrap();
        assert!(!places.is_empty(), "no places found");
        let mitte = places.iter().find(|p| p.name() == "Mitte").expect("Mitte");
        assert!(
            (mitte.lon() - 13.4).abs() < 0.1 && (mitte.lat() - 52.52).abs() < 0.1,
            "{mitte:?}"
        );
        let mut seen = std::collections::HashSet::new();
        for p in &places {
            let key = (
                p.name().to_string(),
                (p.lon() * 250.0) as i64,
                (p.lat() * 250.0) as i64,
            );
            assert!(seen.insert(key), "duplicate {p:?}");
        }
        let json = places_json(Path::new(FIXTURE)).unwrap();
        assert!(json.starts_with("[[\""));
    }

    #[test]
    fn a_file_that_is_not_pmtiles_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.pmtiles");
        std::fs::write(&f, vec![0u8; 200]).unwrap();
        let e = read_places(&f, SCAN_ZOOM).unwrap_err().to_string();
        assert!(e.contains("not a usable PMTiles v3 file"), "{e}");
        assert!(read_places(&dir.path().join("none"), SCAN_ZOOM).is_err());
    }
}
