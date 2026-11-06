//! A minimal reader for ZIM files (the format Kiwix uses), enough to walk the content entries and
//! read their clusters. It supports the current layout (version 6: everything in namespace `C`)
//! with uncompressed or Zstandard clusters, which is what the Kiwix map files use.
//! Nothing here writes, and nothing needs `kiwix-serve` to be running.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;

const MAGIC: u32 = 0x044D_495A;
const HEADER_LEN: usize = 80;
/// A cluster that unpacks to more than this is treated as damaged.
const MAX_CLUSTER: u64 = 512 * 1024 * 1024;
/// Longest directory entry we read in one go: 16 bytes of header plus path and title.
const DIRENT_READ: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum ZimError {
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path} is not a usable ZIM file: {reason}")]
    Format { path: String, reason: String },
}

/// A content entry: where its bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub cluster: u32,
    pub blob: u32,
}

pub struct ZimFile {
    file: File,
    name: String,
    /// Identifies the file's content; changes whenever the file is rebuilt.
    uuid: [u8; 16],
    entry_count: u32,
    cluster_count: u32,
    url_ptr_pos: u64,
    cluster_ptr_pos: u64,
    checksum_pos: u64,
}

/// One unpacked cluster: its blobs lie one after the other.
pub struct Cluster {
    data: Vec<u8>,
    offsets: Vec<usize>,
}

impl Cluster {
    pub fn blob(&self, index: u32) -> Option<&[u8]> {
        let i = index as usize;
        self.data.get(*self.offsets.get(i)?..*self.offsets.get(i + 1)?)
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

impl ZimFile {
    pub fn open(path: &Path) -> Result<Self, ZimError> {
        let name = path.display().to_string();
        let io = |source| ZimError::Io {
            path: name.clone(),
            source,
        };
        let file = File::open(path).map_err(io)?;
        let mut h = [0u8; HEADER_LEN];
        let got = file.read_at(&mut h, 0).map_err(io)?;
        let bad = |reason: &str| ZimError::Format {
            path: name.clone(),
            reason: reason.into(),
        };
        if got < HEADER_LEN {
            return Err(bad("too short for a header"));
        }
        if u32_at(&h, 0) != Some(MAGIC) {
            return Err(bad("wrong magic number"));
        }
        let major = u16_at(&h, 4).unwrap_or(0);
        if major != 6 && major != 5 {
            return Err(bad(&format!("version {major} is not supported")));
        }
        let field = |v: Option<u64>| v.ok_or_else(|| bad("short header"));
        Ok(Self {
            uuid: h[8..24].try_into().expect("16 bytes"),
            entry_count: u32_at(&h, 24).ok_or_else(|| bad("short header"))?,
            cluster_count: u32_at(&h, 28).ok_or_else(|| bad("short header"))?,
            url_ptr_pos: field(u64_at(&h, 32))?,
            cluster_ptr_pos: field(u64_at(&h, 48))?,
            checksum_pos: field(u64_at(&h, 72))?,
            file,
            name,
        })
    }

    /// The file's identity as hex, for "is the index of this very file".
    pub fn uuid_hex(&self) -> String {
        self.uuid.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn io(&self, source: std::io::Error) -> ZimError {
        ZimError::Io {
            path: self.name.clone(),
            source,
        }
    }

    fn bad(&self, reason: &str) -> ZimError {
        ZimError::Format {
            path: self.name.clone(),
            reason: reason.into(),
        }
    }

    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// Content entries (namespace `C`, no redirects) whose path `keep` accepts.
    pub fn entries(&self, mut keep: impl FnMut(&str) -> bool) -> Result<Vec<Entry>, ZimError> {
        let mut ptrs = vec![0u8; self.entry_count as usize * 8];
        self.file
            .read_exact_at(&mut ptrs, self.url_ptr_pos)
            .map_err(|e| self.io(e))?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; DIRENT_READ];
        for p in ptrs.as_chunks::<8>().0 {
            let pos = u64::from_le_bytes(*p);
            let n = self.read_up_to(&mut buf, pos)?;
            let d = &buf[..n];
            // Redirects (mimetype 0xFFFF) have a shorter header and carry no data.
            if u16_at(d, 0) == Some(0xFFFF) || d.get(3) != Some(&b'C') {
                continue;
            }
            let (Some(cluster), Some(blob), Some(rest)) = (u32_at(d, 8), u32_at(d, 12), d.get(16..)) else {
                continue;
            };
            let Some(end) = rest.iter().position(|b| *b == 0) else {
                continue;
            };
            let Ok(path) = std::str::from_utf8(&rest[..end]) else {
                continue;
            };
            if keep(path) {
                out.push(Entry {
                    path: path.to_string(),
                    cluster,
                    blob,
                });
            }
        }
        Ok(out)
    }

    /// Reads as much as is there, up to `buf.len()` (the last entries sit near the end of the file).
    fn read_up_to(&self, buf: &mut [u8], pos: u64) -> Result<usize, ZimError> {
        let mut done = 0;
        while done < buf.len() {
            match self.file.read_at(&mut buf[done..], pos + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(self.io(e)),
            }
        }
        Ok(done)
    }

    fn cluster_ptr(&self, n: u32) -> Result<u64, ZimError> {
        let mut b = [0u8; 8];
        self.file
            .read_exact_at(&mut b, self.cluster_ptr_pos + u64::from(n) * 8)
            .map_err(|e| self.io(e))?;
        Ok(u64::from_le_bytes(b))
    }

    /// Reads and unpacks one cluster.
    pub fn cluster(&self, n: u32) -> Result<Cluster, ZimError> {
        if n >= self.cluster_count {
            return Err(self.bad("an entry points past the last cluster"));
        }
        let start = self.cluster_ptr(n)?;
        let end = if n + 1 < self.cluster_count {
            self.cluster_ptr(n + 1)?
        } else {
            self.checksum_pos
        };
        let len = end
            .checked_sub(start)
            .filter(|l| (1..=MAX_CLUSTER).contains(l))
            .ok_or_else(|| self.bad("a cluster has an impossible size"))?;
        let mut raw = vec![0u8; len as usize];
        self.file.read_exact_at(&mut raw, start).map_err(|e| self.io(e))?;
        let info = raw[0];
        let wide = info & 0x10 != 0;
        let body = &raw[1..];
        let data = match info & 0x0f {
            0 | 1 => body.to_vec(),
            5 => {
                let mut dec = ruzstd::decoding::StreamingDecoder::new(body)
                    .map_err(|e| self.bad(&format!("cluster {n}: {e}")))?;
                let mut out = Vec::new();
                (&mut dec)
                    .take(MAX_CLUSTER)
                    .read_to_end(&mut out)
                    .map_err(|e| self.bad(&format!("cluster {n}: {e}")))?;
                out
            }
            c => return Err(self.bad(&format!("cluster compression {c} is not supported"))),
        };
        let width = if wide { 8 } else { 4 };
        let first = if wide {
            u64_at(&data, 0)
        } else {
            u32_at(&data, 0).map(u64::from)
        }
        .ok_or_else(|| self.bad("a cluster is too short"))? as usize;
        if first == 0 || !first.is_multiple_of(width) || first > data.len() {
            return Err(self.bad("a cluster has a broken offset table"));
        }
        let offsets = (0..first / width)
            .map(|i| {
                if wide {
                    u64_at(&data, i * 8).map(|v| v as usize)
                } else {
                    u32_at(&data, i * 4).map(|v| v as usize)
                }
            })
            .collect::<Option<Vec<_>>>()
            .filter(|o| o.windows(2).all(|w| w[0] <= w[1]) && o.last().is_some_and(|l| *l <= data.len()))
            .ok_or_else(|| self.bad("a cluster has a broken offset table"))?;
        Ok(Cluster { data, offsets })
    }
}
