//! Translation models for page translation in the browser (Bergamot). Mozilla publishes them in
//! its Remote Settings collection `translations-models`: per language pair a model, a lexical
//! shortlist and a vocabulary (or one vocabulary per language), each with size and SHA-256.
//! Everything is read live, so the catalog only names the pair.

use serde::Deserialize;

use super::http::{Http, HttpError};

pub const RECORDS_URL: &str =
    "https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records";
pub const ATTACHMENTS: &str = "https://firefox-settings-attachments.cdn.mozilla.net/";

#[derive(Debug, thiserror::Error)]
pub enum TranslationError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("Mozilla's model list is unusable: {0}")]
    Unusable(String),
    #[error("no complete model found for {from}-{to} (missing: {missing})")]
    Incomplete {
        from: String,
        to: String,
        missing: String,
    },
}

/// One file of a pair, as it is stored in `<data>/translate/<from>-<to>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFile {
    /// `model.bin`, `lex.bin`, `vocab.spm`, or `srcvocab.spm` / `trgvocab.spm`.
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairFiles {
    pub version: String,
    pub files: Vec<ModelFile>,
}

impl PairFiles {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Deserialize)]
struct Answer {
    data: Vec<Rec>,
}

#[derive(Deserialize)]
struct Rec {
    #[serde(rename = "fileType")]
    file_type: String,
    #[serde(rename = "fromLang")]
    from: String,
    #[serde(rename = "toLang")]
    to: String,
    version: String,
    attachment: Option<Attachment>,
}

#[derive(Deserialize)]
struct Attachment {
    hash: String,
    size: u64,
    location: String,
}

/// `2.1` > `2.0` > `1.0`; anything unparsable sorts lowest.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn local_name(file_type: &str) -> Option<&'static str> {
    Some(match file_type {
        "model" => "model.bin",
        "lex" => "lex.bin",
        "vocab" => "vocab.spm",
        "srcvocab" => "srcvocab.spm",
        "trgvocab" => "trgvocab.spm",
        _ => return None,
    })
}

/// The newest complete set of files for `from` -> `to` in the answer of [`RECORDS_URL`].
pub fn pair_from_records(json: &str, from: &str, to: &str) -> Result<PairFiles, TranslationError> {
    let answer: Answer = serde_json::from_str(json).map_err(|e| TranslationError::Unusable(e.to_string()))?;
    let mine: Vec<&Rec> = answer
        .data
        .iter()
        .filter(|r| r.from == from && r.to == to && r.attachment.is_some())
        .collect();
    // The newest version that has a model and a lexicon plus a vocabulary of one kind or the other.
    let mut versions: Vec<&str> = mine.iter().map(|r| r.version.as_str()).collect();
    versions.sort_by_key(|v| std::cmp::Reverse(version_key(v)));
    versions.dedup();
    let mut missing = "model, lex, vocab".to_string();
    for v in versions {
        let of = |t: &str| mine.iter().copied().find(|r| r.version == v && r.file_type == t);
        let vocab: Vec<&Rec> = if let Some(r) = of("vocab") {
            vec![r]
        } else {
            [of("srcvocab"), of("trgvocab")].into_iter().flatten().collect()
        };
        let (Some(model), Some(lex)) = (of("model"), of("lex")) else {
            missing = "model or lex".into();
            continue;
        };
        if vocab.is_empty() || (vocab.len() == 2) != (vocab[0].file_type != "vocab") {
            missing = "vocab".into();
            continue;
        }
        let files = [model, lex]
            .into_iter()
            .chain(vocab)
            .filter_map(|r| {
                let a = r.attachment.as_ref()?;
                Some(ModelFile {
                    name: local_name(&r.file_type)?.into(),
                    url: format!("{ATTACHMENTS}{}", a.location),
                    sha256: a.hash.clone(),
                    size: a.size,
                })
            })
            .collect();
        return Ok(PairFiles {
            version: v.to_string(),
            files,
        });
    }
    Err(TranslationError::Incomplete {
        from: from.into(),
        to: to.into(),
        missing,
    })
}

/// Asks Mozilla for the files of one pair (the query is filtered on the server).
pub fn lookup(http: &(impl Http + ?Sized), from: &str, to: &str) -> Result<PairFiles, TranslationError> {
    let url = format!("{RECORDS_URL}?fromLang={from}&toLang={to}&_limit=100");
    pair_from_records(&http.get(&url)?, from, to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(t: &str, v: &str, name: &str) -> String {
        format!(
            r#"{{"fileType":"{t}","fromLang":"en","toLang":"de","version":"{v}","name":"{name}",
            "attachment":{{"hash":"{h}","size":10,"location":"main-workspace/x/{name}"}}}}"#,
            h = "ab".repeat(32)
        )
    }

    #[test]
    fn newest_complete_version_wins_and_files_get_fixed_names() {
        let json = format!(
            r#"{{"data":[{},{},{},{},{},{}]}}"#,
            rec("model", "2.0", "m2"),
            rec("lex", "2.0", "l2"),
            rec("vocab", "2.0", "v2"),
            rec("model", "2.1", "m21"),
            rec("lex", "1.0", "l1"),
            rec("vocab", "2.1", "v21")
        );
        // 2.1 has no lexicon, so 2.0 is the newest complete set.
        let p = pair_from_records(&json, "en", "de").unwrap();
        assert_eq!(p.version, "2.0");
        let names: Vec<_> = p.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["model.bin", "lex.bin", "vocab.spm"]);
        assert_eq!(p.size(), 30);
        assert!(p.files[0].url.ends_with("main-workspace/x/m2"));
    }

    #[test]
    fn separate_vocabularies_need_both_and_missing_pairs_are_reported() {
        let ok = format!(
            r#"{{"data":[{},{},{},{}]}}"#,
            rec("model", "1.0", "m"),
            rec("lex", "1.0", "l"),
            rec("srcvocab", "1.0", "s"),
            rec("trgvocab", "1.0", "t")
        );
        assert_eq!(pair_from_records(&ok, "en", "de").unwrap().files.len(), 4);
        let half = format!(
            r#"{{"data":[{},{},{}]}}"#,
            rec("model", "1.0", "m"),
            rec("lex", "1.0", "l"),
            rec("srcvocab", "1.0", "s")
        );
        assert!(pair_from_records(&half, "en", "de").is_err());
        assert!(pair_from_records(r#"{"data":[]}"#, "en", "de").is_err());
        assert!(pair_from_records(&ok, "en", "fr").is_err());
        assert!(pair_from_records("nope", "en", "de").is_err());
    }
}
