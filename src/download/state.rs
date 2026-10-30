//! Persistent progress of one download (`<name>.part.state`): which chunks are complete.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const STATE_VERSION: u32 = 1;

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

mod hex_bytes {
    use super::*;

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&to_hex(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        from_hex(&s).ok_or_else(|| D::Error::custom("invalid hex"))
    }
}

/// Chunk bitmap plus the identity of the file it belongs to. A state is only trusted when it
/// describes exactly the file we are about to download (same name, size and checksum).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub chunk_size: u64,
    #[serde(with = "hex_bytes")]
    done: Vec<u8>,
}

impl State {
    pub fn new(name: &str, size: u64, sha256: &str, chunk_size: u64) -> Self {
        let chunks = size.div_ceil(chunk_size.max(1)) as usize;
        Self {
            version: STATE_VERSION,
            name: name.to_string(),
            size,
            sha256: sha256.to_string(),
            chunk_size,
            done: vec![0; chunks.div_ceil(8)],
        }
    }

    pub fn chunk_count(&self) -> usize {
        self.size.div_ceil(self.chunk_size.max(1)) as usize
    }

    /// `(offset, length)` of chunk `i`; the last chunk may be shorter.
    pub fn chunk_range(&self, i: usize) -> (u64, u64) {
        let offset = i as u64 * self.chunk_size;
        (offset, self.chunk_size.min(self.size - offset))
    }

    pub fn is_done(&self, i: usize) -> bool {
        self.done[i / 8] & (1 << (i % 8)) != 0
    }

    pub fn mark_done(&mut self, i: usize) {
        self.done[i / 8] |= 1 << (i % 8);
    }

    pub fn done_bytes(&self) -> u64 {
        (0..self.chunk_count())
            .filter(|&i| self.is_done(i))
            .map(|i| self.chunk_range(i).1)
            .sum()
    }

    pub fn pending(&self) -> Vec<usize> {
        (0..self.chunk_count()).filter(|&i| !self.is_done(i)).collect()
    }

    /// Structurally sound (guards against hand-edited or truncated files).
    pub fn is_consistent(&self) -> bool {
        self.version == STATE_VERSION
            && self.chunk_size > 0
            && self.size > 0
            && self.done.len() == self.chunk_count().div_ceil(8)
    }

    pub fn matches(&self, name: &str, size: u64, sha256: &str) -> bool {
        self.name == name && self.size == size && self.sha256 == sha256
    }

    /// Atomic and durable: temp file, fsync, rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("state.tmp");
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    }

    /// `None` for a missing or unreadable state; the caller then starts over.
    pub fn load(path: &Path) -> Option<Self> {
        let state: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        state.is_consistent().then_some(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0e281a87af92a798297fc671f8b37004cb7874ac051e13876b7335be96649e59";

    #[test]
    fn hex_roundtrip_and_rejects_garbage() {
        assert_eq!(to_hex(&[0, 255, 16]), "00ff10");
        assert_eq!(from_hex("00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("é"), None);
    }

    #[test]
    fn chunk_geometry_with_short_last_chunk() {
        let s = State::new("f", 25, SHA, 10);
        assert_eq!(s.chunk_count(), 3);
        assert_eq!(s.chunk_range(0), (0, 10));
        assert_eq!(s.chunk_range(2), (20, 5));
        let exact = State::new("f", 20, SHA, 10);
        assert_eq!(exact.chunk_count(), 2);
        assert_eq!(exact.chunk_range(1), (10, 10));
    }

    #[test]
    fn marking_and_counting() {
        let mut s = State::new("f", 25, SHA, 10);
        assert_eq!(s.pending(), vec![0, 1, 2]);
        s.mark_done(2);
        s.mark_done(0);
        assert!(s.is_done(0) && !s.is_done(1) && s.is_done(2));
        assert_eq!(s.done_bytes(), 15);
        assert_eq!(s.pending(), vec![1]);
    }

    #[test]
    fn bitmap_works_across_byte_boundaries() {
        let mut s = State::new("f", 100, SHA, 1);
        for i in [0, 7, 8, 99] {
            s.mark_done(i);
        }
        assert_eq!(s.done_bytes(), 4);
        assert!(s.is_done(8) && !s.is_done(9));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x.part.state");
        let mut s = State::new("f", 25, SHA, 10);
        s.mark_done(1);
        s.save(&p).unwrap();
        assert_eq!(State::load(&p), Some(s));
        assert!(!d.path().join("x.part.state.tmp").exists() && !d.path().join("x.part.state.tmp").exists());
    }

    #[test]
    fn unreadable_or_inconsistent_state_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s");
        assert_eq!(State::load(&p), None, "missing");
        fs::write(&p, "not json").unwrap();
        assert_eq!(State::load(&p), None, "garbage");
        let mut s = State::new("f", 25, SHA, 10);
        s.done.push(0);
        s.save(&p).unwrap();
        assert_eq!(State::load(&p), None, "bitmap length does not fit the file size");
        let mut s = State::new("f", 25, SHA, 10);
        s.version = 99;
        s.save(&p).unwrap();
        assert_eq!(State::load(&p), None, "unknown version");
    }

    #[test]
    fn matches_requires_same_identity() {
        let s = State::new("f", 25, SHA, 10);
        assert!(s.matches("f", 25, SHA));
        assert!(!s.matches("g", 25, SHA));
        assert!(!s.matches("f", 26, SHA));
        assert!(!s.matches("f", 25, &SHA.replace('0', "1")));
    }
}
