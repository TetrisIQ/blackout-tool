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

/// Where the model manifests are read from.
pub const OLLAMA_REGISTRY: &str = "https://registry.ollama.ai";

#[derive(serde::Deserialize)]
struct OllamaManifest {
    config: OllamaLayer,
    layers: Vec<OllamaLayer>,
}

#[derive(serde::Deserialize)]
struct OllamaLayer {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Ollama registry answer for {name} is unusable: {message}")]
    Ollama { name: String, message: String },
    #[error(transparent)]
    Kiwix(#[from] KiwixError),
    #[error("{name}: {message}")]
    Translation { name: String, message: String },
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
    ollama_base: String,
}

impl<'a, H: Http + ?Sized> LiveResolver<'a, H> {
    pub fn new(http: &'a H) -> Self {
        Self::with_base(http, DEFAULT_BASE)
    }

    pub fn with_base(http: &'a H, base: &str) -> Self {
        Self {
            http,
            base: base.to_string(),
            ollama_base: OLLAMA_REGISTRY.to_string(),
        }
    }

    pub fn with_ollama_base(mut self, base: &str) -> Self {
        self.ollama_base = base.to_string();
        self
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
        if let Some(o) = &m.source.ollama {
            let name = o.name();
            let bad = |message: String| ResolveError::Ollama {
                name: name.clone(),
                message,
            };
            let url = format!("{}/v2/library/{}/manifests/{}", self.ollama_base, o.model, o.tag);
            let body = self.http.get(&url).map_err(|e| bad(e.to_string()))?;
            let manifest: OllamaManifest = serde_json::from_str(&body).map_err(|e| bad(e.to_string()))?;
            let weights = manifest
                .layers
                .iter()
                .find(|l| l.media_type.ends_with("image.model"))
                .ok_or_else(|| bad("no model layer".into()))?;
            let digest = weights.digest.strip_prefix("sha256:").unwrap_or(&weights.digest);
            return Ok(Resolved {
                size_bytes: manifest.config.size + manifest.layers.iter().map(|l| l.size).sum::<u64>(),
                sha256: None,
                url: None,
                mirrors: Vec::new(),
                version: Some(digest.chars().take(12).collect()),
                live: true,
            });
        }
        if let Some(t) = &m.source.translation {
            let files = super::translation::lookup(self.http, &t.from, &t.to).map_err(|e| {
                ResolveError::Translation {
                    name: t.pair(),
                    message: e.to_string(),
                }
            })?;
            return Ok(Resolved {
                size_bytes: files.size(),
                sha256: None,
                url: None,
                mirrors: Vec::new(),
                version: Some(files.version),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_models_are_sized_from_the_registry_manifest() {
        use crate::catalog::http::testing::FakeHttp;
        let body = r#"{"schemaVersion":2,
            "config":{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:0946fa725932","size":100},
            "layers":[
              {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:20e436143017abcdef","size":2000},
              {"mediaType":"application/vnd.ollama.image.license","digest":"sha256:58d1e17ffe51","size":30}]}"#;
        let http = FakeHttp::default().with("http://reg/v2/library/granite4.2/manifests/3b", body);
        let m: Manifest = serde_yaml_ng::from_str(
            "id: llm\nname: n\ndescription: d\ncategory: ai\ntype: model\n\
             source: {ollama: {model: granite4.2, tag: 3b}}\nlicense: {spdx: Apache-2.0}\n\
             model: {context_k: 128}\n",
        )
        .unwrap();
        assert!(m.validate().is_empty(), "{:?}", m.validate());
        let r = LiveResolver::new(&http)
            .with_ollama_base("http://reg")
            .resolve(&m)
            .unwrap();
        assert_eq!((r.size_bytes, r.url, r.sha256), (2130, None, None));
        assert_eq!(r.version.as_deref(), Some("20e436143017"));
        assert!(r.live);

        let missing = FakeHttp::default();
        let err = LiveResolver::new(&missing)
            .with_ollama_base("http://reg")
            .resolve(&m)
            .unwrap_err();
        assert!(err.to_string().contains("granite4.2:3b"), "{err}");
    }
}
