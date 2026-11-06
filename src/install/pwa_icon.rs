//! The app icon of the game hub (a play triangle on a rounded square), drawn by hand and encoded as
//! PNG so phones accept it as an installable icon without an image library.

use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

const BG_TOP: [f32; 3] = [0.20, 0.18, 0.15];
const BG_BOTTOM: [f32; 3] = [0.10, 0.09, 0.08];
const ACCENT: [f32; 3] = [0.84, 0.63, 0.30];

/// Coverage (0..1) of a rounded square of half-size `h` and corner radius `r`, centred at 0.
fn rounded_square(x: f32, y: f32, h: f32, r: f32) -> bool {
    let (qx, qy) = (x.abs() - (h - r), y.abs() - (h - r));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) <= r
}

/// Point inside the triangle (a, b, c)?
fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let side = |p1: (f32, f32), p2: (f32, f32), p3: (f32, f32)| {
        (p1.0 - p3.0) * (p2.1 - p3.1) - (p2.0 - p3.0) * (p1.1 - p3.1)
    };
    let (d1, d2, d3) = (side(p, a, b), side(p, b, c), side(p, c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

/// RGBA pixels (row-major) of the icon at `size` x `size`. `maskable` fills the whole square, so
/// the system may crop it to any shape.
fn render(size: usize, maskable: bool) -> Vec<u8> {
    const SS: usize = 3;
    let mut out = Vec::with_capacity(size * size * 4);
    let s = size as f32;
    for py in 0..size {
        for px in 0..size {
            let (mut acc, mut cov) = ([0.0f32; 3], 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    // Unit coordinates in -1..1.
                    let x = ((px * SS + sx) as f32 + 0.5) / (s * SS as f32) * 2.0 - 1.0;
                    let y = ((py * SS + sy) as f32 + 0.5) / (s * SS as f32) * 2.0 - 1.0;
                    if !maskable && !rounded_square(x, y, 1.0, 0.42) {
                        continue;
                    }
                    let t = (y + 1.0) / 2.0;
                    let mut c: [f32; 3] = std::array::from_fn(|i| BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t);
                    // The glyph stays inside the central 80 % (the safe zone of maskable icons).
                    let k = if maskable { 0.8 } else { 1.0 };
                    let (gx, gy) = (x / k, y / k);
                    let ring = (gx * gx + gy * gy).sqrt();
                    let tri = in_triangle((gx, gy), (-0.2, -0.38), (-0.2, 0.38), (0.44, 0.0));
                    if tri || (0.66..=0.74).contains(&ring) {
                        c = ACCENT;
                    }
                    for i in 0..3 {
                        acc[i] += c[i];
                    }
                    cov += 1.0;
                }
            }
            if cov == 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                for a in acc {
                    out.push((a / cov * 255.0).round() as u8);
                }
                out.push((cov / (SS * SS) as f32 * 255.0).round() as u8);
            }
        }
    }
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut crc = flate2::Crc::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// The icon as PNG bytes.
pub fn png(size: usize, maskable: bool) -> Vec<u8> {
    let pixels = render(size, maskable);
    let mut raw = Vec::with_capacity(pixels.len() + size);
    for row in pixels.chunks(size * 4) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(&raw).expect("writing to a Vec cannot fail");
    let idat = z.finish().expect("writing to a Vec cannot fail");

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bit RGBA
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &idat);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_signature_size_and_transparent_corners() {
        let p = png(64, false);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&p[16..24], &[0, 0, 0, 64, 0, 0, 0, 64]);
        assert_eq!(&p[p.len() - 8..p.len() - 4], b"IEND");
        assert_eq!(render(64, false)[3], 0, "corner is transparent");
        assert_eq!(render(64, true)[3], 255, "maskable is opaque");
    }
}
