//! Turns a manifest into concrete size / checksum / URLs, reading Kiwix content live.

use serde::Serialize;

use super::http::Http;
use super::kiwix::{DEFAULT_BASE, KiwixClient, KiwixError};
use super::manifest::Manifest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Resolved {
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub url: Option<String>,
    pub mirrors: Vec<String>,
    /// Upstream version marker (Kiwix: date of the catalog entry).
    pub version: Option<String>,
    /// True when the values were read from the Kiwix catalog rather than the manifest.
    pub live: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Kiwix(#[from] KiwixError),
    #[error("manifest declares no size")]
    NoSize,
}

pub trait Resolver {
    fn resolve(&self, m: &Manifest) -> Result<Resolved, ResolveError>;

    /// Caveats collected while resolving (e.g. answers served from an expired cache).
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct LiveResolver<'a, H: Http + ?Sized> {
    http: &'a H,
    base: String,
}

impl<'a, H: Http + ?Sized> LiveResolver<'a, H> {
    pub fn new(http: &'a H) -> Self {
        Self::with_base(http, DEFAULT_BASE)
    }

    pub fn with_base(http: &'a H, base: &str) -> Self {
        Self {
            http,
            base: base.to_string(),
        }
    }
}

impl<H: Http + ?Sized> Resolver for LiveResolver<'_, H> {
    fn resolve(&self, m: &Manifest) -> Result<Resolved, ResolveError> {
        if let Some(k) = &m.source.kiwix {
            let d = KiwixClient::new(self.http, &self.base).lookup(k)?;
            return Ok(Resolved {
                size_bytes: d.size_bytes,
                sha256: Some(d.sha256),
                url: Some(d.url),
                mirrors: d.mirrors,
                version: Some(d.updated),
                live: true,
            });
        }
        Ok(Resolved {
            size_bytes: m.size_bytes.ok_or(ResolveError::NoSize)?,
            sha256: m.sha256.clone(),
            url: m.source.url.clone(),
            mirrors: m.source.mirrors.clone(),
            version: None,
            live: false,
        })
    }

    fn warnings(&self) -> Vec<String> {
        self.http
            .stale_urls()
            .into_iter()
            .map(|u| format!("used outdated cached data for {u} (no network); sizes may have changed"))
            .collect()
    }
}
