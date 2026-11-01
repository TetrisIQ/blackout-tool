//! Minimal HTTP abstraction so catalog code can be tested without a network,
//! plus a disk cache that lets `kit plan` keep working offline.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request to {url} failed: {message}")]
    Request { url: String, message: String },
    #[error("offline mode and {url} is not in the cache")]
    NotCached { url: String },
}

pub trait Http {
    fn get(&self, url: &str) -> Result<String, HttpError>;

    /// POSTs a JSON body and returns the response body. Only needed by first-run provisioning of
    /// services; clients that cannot do it say so.
    fn post_json(&self, url: &str, _body: &str) -> Result<String, HttpError> {
        Err(HttpError::Request {
            url: url.to_string(),
            message: "POST is not supported by this client".into(),
        })
    }

    /// URLs that were answered from an expired cache entry because the network failed.
    fn stale_urls(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Blocking client backed by `ureq`; follows redirects.
pub struct UreqHttp {
    agent: ureq::Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("emergency-kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }
}

impl Http for UreqHttp {
    fn get(&self, url: &str) -> Result<String, HttpError> {
        let fail = |e: ureq::Error| HttpError::Request {
            url: url.to_string(),
            message: e.to_string(),
        };
        let mut resp = self.agent.get(url).call().map_err(fail)?;
        resp.body_mut().read_to_string().map_err(fail)
    }

    fn post_json(&self, url: &str, body: &str) -> Result<String, HttpError> {
        let fail = |e: ureq::Error| HttpError::Request {
            url: url.to_string(),
            message: e.to_string(),
        };
        let mut resp = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(body)
            .map_err(fail)?;
        resp.body_mut().read_to_string().map_err(fail)
    }
}

/// Caches response bodies under `dir`, keyed by URL.
///
/// * fresh entry (younger than `ttl`) -> served without a request
/// * otherwise a request is made; on failure an expired entry is served and reported via
///   [`Http::stale_urls`]
/// * `offline` never makes a request and serves any cached entry
pub struct CachedHttp<H> {
    inner: H,
    dir: PathBuf,
    ttl: Duration,
    offline: bool,
    stale: Mutex<Vec<String>>,
}

impl<H: Http> CachedHttp<H> {
    pub fn new(inner: H, dir: PathBuf, ttl: Duration, offline: bool) -> Self {
        Self {
            inner,
            dir,
            ttl,
            offline,
            stale: Mutex::new(Vec::new()),
        }
    }

    fn path_for(&self, url: &str) -> PathBuf {
        let digest = Sha256::digest(url.as_bytes());
        let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.cache"))
    }

    fn read(&self, url: &str) -> Option<(String, Duration)> {
        let path = self.path_for(url);
        let body = fs::read_to_string(&path).ok()?;
        let age = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or(Duration::MAX);
        Some((body, age))
    }

    fn write(&self, url: &str, body: &str) {
        // Caching is best effort: a read-only cache dir must not break the command.
        let path = self.path_for(url);
        let tmp = path.with_extension("tmp");
        let result = fs::create_dir_all(&self.dir)
            .and_then(|()| fs::File::create(&tmp))
            .and_then(|mut f| f.write_all(body.as_bytes()))
            .and_then(|()| fs::rename(&tmp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    fn mark_stale(&self, url: &str) {
        let mut stale = self.stale.lock().unwrap_or_else(|e| e.into_inner());
        if !stale.iter().any(|u| u == url) {
            stale.push(url.to_string());
        }
    }
}

impl<H: Http> Http for CachedHttp<H> {
    fn get(&self, url: &str) -> Result<String, HttpError> {
        let cached = self.read(url);
        if self.offline {
            return match cached {
                Some((body, age)) => {
                    if age > self.ttl {
                        self.mark_stale(url);
                    }
                    Ok(body)
                }
                None => Err(HttpError::NotCached { url: url.to_string() }),
            };
        }
        if let Some((body, age)) = &cached
            && *age <= self.ttl
        {
            return Ok(body.clone());
        }
        match self.inner.get(url) {
            Ok(body) => {
                self.write(url, &body);
                Ok(body)
            }
            Err(e) => match cached {
                Some((body, _)) => {
                    self.mark_stale(url);
                    Ok(body)
                }
                None => Err(e),
            },
        }
    }

    fn stale_urls(&self) -> Vec<String> {
        self.stale.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Percent-encodes a query component.
pub fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves canned bodies and counts requests.
    #[derive(Default)]
    pub struct FakeHttp {
        pub bodies: HashMap<String, String>,
        pub calls: AtomicUsize,
        pub fail: bool,
        /// `(url, body)` of every POST.
        pub posted: std::sync::Mutex<Vec<(String, String)>>,
        pub post_fails: bool,
    }

    impl FakeHttp {
        pub fn with(mut self, url: &str, body: impl Into<String>) -> Self {
            self.bodies.insert(url.to_string(), body.into());
            self
        }
    }

    impl Http for FakeHttp {
        fn get(&self, url: &str) -> Result<String, HttpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(HttpError::Request {
                    url: url.into(),
                    message: "network down".into(),
                });
            }
            self.bodies.get(url).cloned().ok_or_else(|| HttpError::Request {
                url: url.into(),
                message: "404".into(),
            })
        }

        fn post_json(&self, url: &str, body: &str) -> Result<String, HttpError> {
            self.posted.lock().unwrap().push((url.into(), body.into()));
            if self.post_fails {
                return Err(HttpError::Request {
                    url: url.into(),
                    message: "403".into(),
                });
            }
            Ok("{}".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeHttp;
    use super::*;
    use std::sync::atomic::Ordering;

    const HOUR: Duration = Duration::from_secs(3600);

    fn age_file(cache: &CachedHttp<FakeHttp>, url: &str, secs: u64) {
        let f = fs::File::options().write(true).open(cache.path_for(url)).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn fresh_cache_avoids_second_request() {
        let dir = tempfile::tempdir().unwrap();
        let c = CachedHttp::new(
            FakeHttp::default().with("u", "body"),
            dir.path().into(),
            HOUR,
            false,
        );
        assert_eq!(c.get("u").unwrap(), "body");
        assert_eq!(c.get("u").unwrap(), "body");
        assert_eq!(c.inner.calls.load(Ordering::SeqCst), 1);
        assert!(c.stale_urls().is_empty());
    }

    #[test]
    fn expired_entry_is_refetched() {
        let dir = tempfile::tempdir().unwrap();
        let c = CachedHttp::new(
            FakeHttp::default().with("u", "new"),
            dir.path().into(),
            HOUR,
            false,
        );
        c.write("u", "old");
        age_file(&c, "u", 2 * 3600);
        assert_eq!(c.get("u").unwrap(), "new");
        assert!(c.stale_urls().is_empty());
    }

    #[test]
    fn network_failure_falls_back_to_stale_entry() {
        let dir = tempfile::tempdir().unwrap();
        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let c = CachedHttp::new(down, dir.path().into(), HOUR, false);
        c.write("u", "old");
        age_file(&c, "u", 2 * 3600);
        assert_eq!(c.get("u").unwrap(), "old");
        assert_eq!(c.stale_urls(), vec!["u".to_string()]);
    }

    #[test]
    fn network_failure_without_cache_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let down = FakeHttp {
            fail: true,
            ..Default::default()
        };
        let c = CachedHttp::new(down, dir.path().into(), HOUR, false);
        assert!(matches!(c.get("u"), Err(HttpError::Request { .. })));
    }

    #[test]
    fn offline_never_requests() {
        let dir = tempfile::tempdir().unwrap();
        let c = CachedHttp::new(
            FakeHttp::default().with("u", "net"),
            dir.path().into(),
            HOUR,
            true,
        );
        assert!(matches!(c.get("u"), Err(HttpError::NotCached { .. })));
        c.write("u", "cached");
        age_file(&c, "u", 99 * 3600);
        assert_eq!(c.get("u").unwrap(), "cached");
        assert_eq!(c.inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(c.stale_urls(), vec!["u".to_string()]);
    }

    #[test]
    fn unwritable_cache_dir_does_not_fail_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, "x").unwrap();
        let c = CachedHttp::new(
            FakeHttp::default().with("u", "b"),
            blocker.join("sub"),
            HOUR,
            false,
        );
        assert_eq!(c.get("u").unwrap(), "b");
    }

    #[test]
    fn encoding() {
        assert_eq!(encode_component("wikipedia_de_all"), "wikipedia_de_all");
        assert_eq!(encode_component("a b&c"), "a%20b%26c");
    }
}
