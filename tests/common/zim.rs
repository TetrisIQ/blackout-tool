//! A tiny ZIM writer for tests.

use std::fs;
use std::path::Path;

/// A tiny ZIM (version 6, uncompressed clusters): `(path, title, html)` per article, one cluster each.
pub fn write_zim(path: &Path, pages: &[(&str, &str, &str)]) {
    let mut pages: Vec<_> = pages.to_vec();
    pages.sort_by_key(|p| p.0);
    let mime = b"text/html\0\0";
    let mut dirents = Vec::new();
    let mut offsets = Vec::new();
    let base = 80 + mime.len() as u64;
    for (i, (url, title, _)) in pages.iter().enumerate() {
        offsets.push(base + dirents.len() as u64);
        dirents.extend_from_slice(&0u16.to_le_bytes()); // mimetype 0: text/html
        dirents.push(0); // parameter length
        dirents.push(b'C');
        dirents.extend_from_slice(&0u32.to_le_bytes()); // revision
        dirents.extend_from_slice(&(i as u32).to_le_bytes()); // cluster
        dirents.extend_from_slice(&0u32.to_le_bytes()); // blob
        dirents.extend_from_slice(url.as_bytes());
        dirents.push(0);
        dirents.extend_from_slice(title.as_bytes());
        dirents.push(0);
    }
    let url_ptr_pos = base + dirents.len() as u64;
    let clusters_pos = url_ptr_pos + 8 * pages.len() as u64;
    let mut clusters = Vec::new();
    let mut cluster_offsets = Vec::new();
    for (_, _, html) in &pages {
        cluster_offsets.push(clusters_pos + clusters.len() as u64);
        clusters.push(1u8); // not compressed
        clusters.extend_from_slice(&8u32.to_le_bytes());
        clusters.extend_from_slice(&(8 + html.len() as u32).to_le_bytes());
        clusters.extend_from_slice(html.as_bytes());
    }
    let cluster_ptr_pos = clusters_pos + clusters.len() as u64;
    let checksum_pos = cluster_ptr_pos + 8 * pages.len() as u64;
    let mut h = Vec::new();
    h.extend_from_slice(&0x044D_495Au32.to_le_bytes());
    h.extend_from_slice(&6u16.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&[7u8; 16]); // uuid
    h.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    h.extend_from_slice(&(pages.len() as u32).to_le_bytes());
    for v in [
        url_ptr_pos,
        0,
        cluster_ptr_pos,
        80,
        0xffff_ffff,
        0xffff_ffff,
        checksum_pos,
    ] {
        h.extend_from_slice(&v.to_le_bytes());
    }
    h.resize(80, 0);
    let mut f = h;
    f.extend_from_slice(mime);
    f.extend_from_slice(&dirents);
    for o in &offsets {
        f.extend_from_slice(&o.to_le_bytes());
    }
    f.extend_from_slice(&clusters);
    for o in &cluster_offsets {
        f.extend_from_slice(&o.to_le_bytes());
    }
    f.extend_from_slice(&[0u8; 16]);
    fs::write(path, f).unwrap();
}
