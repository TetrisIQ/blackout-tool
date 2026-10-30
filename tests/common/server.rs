#![allow(dead_code)]
//! A tiny HTTP/1.1 server with `Range` support and switchable faults, for download tests.
//! Standard library only; one thread per connection; `Connection: close` on every response.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub trait Content: Send + Sync + 'static {
    fn len(&self) -> u64;
    /// Fills `buf` with the bytes starting at `offset`.
    fn fill(&self, offset: u64, buf: &mut [u8]);
}

pub struct Bytes(pub Vec<u8>);

impl Content for Bytes {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn fill(&self, offset: u64, buf: &mut [u8]) {
        buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
    }
}

/// Deterministic pseudo-random bytes of any length, without storing them.
pub struct Generated {
    pub len: u64,
    pub seed: u64,
}

impl Generated {
    fn byte(&self, i: u64) -> u8 {
        // splitmix64 over the 8-byte word index
        let mut z = (i / 8)
            .wrapping_add(self.seed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> ((i % 8) * 8)) as u8
    }

    /// SHA-256 of the whole stream, computed by generating it.
    pub fn sha256(&self) -> String {
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut off = 0;
        while off < self.len {
            let n = buf.len().min((self.len - off) as usize);
            self.fill(off, &mut buf[..n]);
            h.update(&buf[..n]);
            off += n as u64;
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl Content for Generated {
    fn len(&self) -> u64 {
        self.len
    }
    fn fill(&self, offset: u64, buf: &mut [u8]) {
        for (k, b) in buf.iter_mut().enumerate() {
            *b = self.byte(offset + k as u64);
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Test data that is not trivially periodic.
pub fn sample(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    Generated {
        len: len as u64,
        seed: 42,
    }
    .fill(0, &mut v);
    v
}

/// Faults can be changed while the server runs.
pub struct Faults {
    /// Close every connection immediately (network down).
    pub down: AtomicBool,
    /// Answer the next n requests with `503` and `Retry-After: 0`.
    pub status_burst: AtomicUsize,
    /// For the next n requests: send half of the body, then drop the connection.
    pub reset_mid_body: AtomicUsize,
    /// Every n-th request is reset mid-body (0 = off).
    pub reset_every: AtomicU64,
    /// Answer `200` with the full body, as a server without range support would.
    pub ignore_range: AtomicBool,
    /// Flip the bits of the byte at this absolute offset when serving it (`u64::MAX` = off).
    pub flip_byte_at: AtomicU64,
    /// Pause after every 64 KiB piece, to make downloads slow enough to interrupt.
    pub piece_delay_ms: AtomicU64,
    /// Number of requests received (including failed ones).
    pub requests: AtomicU64,
}

impl Default for Faults {
    fn default() -> Self {
        Self {
            down: AtomicBool::new(false),
            status_burst: AtomicUsize::new(0),
            reset_mid_body: AtomicUsize::new(0),
            reset_every: AtomicU64::new(0),
            ignore_range: AtomicBool::new(false),
            flip_byte_at: AtomicU64::new(u64::MAX),
            piece_delay_ms: AtomicU64::new(0),
            requests: AtomicU64::new(0),
        }
    }
}

pub struct Server {
    pub port: u16,
    pub faults: Arc<Faults>,
    stop: Arc<AtomicBool>,
}

impl Server {
    pub fn start(content: impl Content) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let faults = Arc::new(Faults::default());
        let stop = Arc::new(AtomicBool::new(false));
        let content: Arc<dyn Content> = Arc::new(content);
        let (f, s) = (faults.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (c, f) = (content.clone(), f.clone());
                std::thread::spawn(move || {
                    let _ = handle(conn, &*c, &f);
                });
            }
        });
        Self { port, faults, stop }
    }

    /// URL of the served file. Paths starting with `/missing` answer 404.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    pub fn requests(&self) -> u64 {
        self.faults.requests.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wake the accept loop
    }
}

fn take(counter: &AtomicUsize) -> bool {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

fn handle(mut conn: TcpStream, content: &dyn Content, faults: &Faults) -> std::io::Result<()> {
    let n = faults.requests.fetch_add(1, Ordering::SeqCst) + 1;
    if faults.down.load(Ordering::SeqCst) {
        return conn.shutdown(Shutdown::Both);
    }
    conn.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut range = None;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range:") {
            range = Some(v.trim().to_string());
        }
    }

    let reply = |conn: &mut TcpStream, status: &str, extra: &str, len: usize| {
        write!(
            conn,
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {len}\r\nConnection: close\r\n\r\n"
        )
    };

    if path.starts_with("/missing") {
        reply(&mut conn, "404 Not Found", "", 0)?;
        return Ok(());
    }
    if take(&faults.status_burst) {
        reply(&mut conn, "503 Service Unavailable", "Retry-After: 0\r\n", 0)?;
        return Ok(());
    }

    let total = content.len();
    let (start, end, partial) = match range
        .as_deref()
        .filter(|_| !faults.ignore_range.load(Ordering::SeqCst))
    {
        Some(r) => {
            let spec = r.strip_prefix("bytes=").unwrap_or("");
            let (a, b) = spec.split_once('-').unwrap_or(("", ""));
            let start: u64 = a.parse().unwrap_or(u64::MAX);
            let end: u64 = if b.is_empty() {
                total - 1
            } else {
                b.parse().unwrap_or(0)
            }
            .min(total - 1);
            if start > end {
                reply(
                    &mut conn,
                    "416 Range Not Satisfiable",
                    &format!("Content-Range: bytes */{total}\r\n"),
                    0,
                )?;
                return Ok(());
            }
            (start, end, true)
        }
        None => (0, total - 1, false),
    };
    let len = (end - start + 1) as usize;
    let (status, extra) = if partial {
        (
            "206 Partial Content",
            format!("Content-Range: bytes {start}-{end}/{total}\r\nAccept-Ranges: bytes\r\n"),
        )
    } else {
        ("200 OK", String::new())
    };
    reply(&mut conn, status, &extra, len)?;

    let reset_every = faults.reset_every.load(Ordering::SeqCst);
    let cut_at = if take(&faults.reset_mid_body) || (reset_every > 0 && n.is_multiple_of(reset_every)) {
        len / 2
    } else {
        len
    };
    let flip = faults.flip_byte_at.load(Ordering::SeqCst);
    let mut sent = 0usize;
    let mut piece = vec![0u8; 64 * 1024];
    while sent < cut_at {
        let k = piece.len().min(cut_at - sent);
        content.fill(start + sent as u64, &mut piece[..k]);
        if flip >= start + sent as u64 && flip < start + (sent + k) as u64 {
            piece[(flip - start - sent as u64) as usize] ^= 0xFF;
        }
        conn.write_all(&piece[..k])?;
        sent += k;
        let delay = faults.piece_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
    }
    conn.flush()?;
    conn.shutdown(Shutdown::Both)
}
