//! Range requests over HTTP(S) with `ureq`.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use super::{FetchError, RangeSource, RateLimiter};

const PIECE: usize = 64 * 1024;

pub struct UreqRange {
    agent: ureq::Agent,
    limiter: Arc<RateLimiter>,
    jobs: usize,
}

impl UreqRange {
    /// `jobs` is the number of parallel chunk downloads; it scales the timeout under a rate limit.
    pub fn new(limiter: Arc<RateLimiter>, jobs: usize) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            // Status codes are inspected by hand so Retry-After and 4xx/5xx can be told apart.
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .user_agent(concat!("emergency-kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            agent,
            limiter,
            jobs: jobs.max(1),
        }
    }

    /// Time allowed for one whole chunk: generous enough for the rate limit, tight enough to
    /// notice a stalled connection.
    fn chunk_timeout(&self, len: u64, limited_rate: Option<u64>) -> Duration {
        let slowest = limited_rate.map_or(50_000, |r| (r / self.jobs as u64).max(1));
        Duration::from_secs(60 + 2 * len / slowest)
    }
}

/// `bytes 0-99/1000` -> `(0, 99, 1000)`
fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?, total.parse().ok()?))
}

fn retry_after(v: &str) -> Option<Duration> {
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

impl RangeSource for UreqRange {
    fn read_range(
        &self,
        url: &str,
        start: u64,
        len: u64,
        total: u64,
        buf: &mut Vec<u8>,
    ) -> Result<(), FetchError> {
        let end = start + len - 1;
        let timeout = self.chunk_timeout(len, self.limiter.rate());
        let result = self
            .agent
            .get(url)
            .header("Range", format!("bytes={start}-{end}"))
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call();
        let mut resp = match result {
            Ok(r) => r,
            Err(ureq::Error::BadUri(m)) => {
                return Err(FetchError::SourceUnusable(format!("bad URL {url}: {m}")));
            }
            Err(e) => return Err(FetchError::transient(format!("{url}: {e}"))),
        };

        let status = resp.status().as_u16();
        match status {
            206 => {}
            429 | 408 | 500..=599 => {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(retry_after);
                return Err(FetchError::Transient {
                    message: format!("{url}: HTTP {status}"),
                    retry_after,
                });
            }
            200 => {
                return Err(FetchError::SourceUnusable(format!(
                    "{url} ignores Range requests (HTTP 200)"
                )));
            }
            _ => return Err(FetchError::SourceUnusable(format!("{url}: HTTP {status}"))),
        }

        let range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range);
        match range {
            Some((a, b, t)) if a == start && b == end && t == total => {}
            Some((_, _, t)) if t != total => {
                return Err(FetchError::SourceUnusable(format!(
                    "{url} has {t} bytes, expected {total}"
                )));
            }
            _ => return Err(FetchError::transient(format!("{url}: unexpected Content-Range"))),
        }

        buf.clear();
        buf.reserve(len as usize);
        let mut reader = resp.body_mut().as_reader();
        let mut piece = [0u8; PIECE];
        while (buf.len() as u64) < len {
            let want = PIECE.min((len - buf.len() as u64) as usize);
            match reader.read(&mut piece[..want]) {
                Ok(0) => break,
                Ok(n) => {
                    self.limiter.acquire(n as u64);
                    buf.extend_from_slice(&piece[..n]);
                }
                Err(e) => return Err(FetchError::transient(format!("{url}: connection lost: {e}"))),
            }
        }
        if (buf.len() as u64) != len {
            return Err(FetchError::transient(format!(
                "{url}: short body ({} of {len} bytes)",
                buf.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 99, 1000)));
        assert_eq!(parse_content_range(" bytes 5-5/6 "), Some((5, 5, 6)));
        assert_eq!(parse_content_range("bytes 0-99/*"), None);
        assert_eq!(parse_content_range("items 0-99/1000"), None);
        assert_eq!(parse_content_range("bytes a-b/c"), None);
    }

    #[test]
    fn retry_after_seconds_only() {
        assert_eq!(retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    #[test]
    fn timeout_scales_with_rate_limit() {
        let fast = UreqRange::new(Arc::new(RateLimiter::new(None)), 4);
        let slow = UreqRange::new(Arc::new(RateLimiter::new(Some(100_000))), 4);
        let len = 8 * 1024 * 1024;
        assert!(fast.chunk_timeout(len, None) < Duration::from_secs(400));
        assert!(slow.chunk_timeout(len, Some(100_000)) > Duration::from_secs(600));
    }
}
