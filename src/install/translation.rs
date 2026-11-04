//! Installer for `translation` packages: the models of one language pair for the portal's page
//! translation. kit fetches the files itself from Mozilla's attachment CDN (the list with sizes
//! and SHA-256 comes from Remote Settings, see `catalog::translation`), accepts a file only when
//! its checksum matches, and moves the finished set into `<data>/translate/<from>-<to>/`. The
//! portal links that folder and offers the translate button for it. Nothing needs a program.

use std::fs;
use std::path::Path;

use super::state::{FileRecord, Record, State, StateGuard};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::TranslationRef;
use crate::catalog::translation as models;

pub struct TranslationInstaller;

fn need_ref(input: &InstallInput) -> Result<&TranslationRef, InstallError> {
    input
        .translation
        .as_ref()
        .ok_or_else(|| InstallError::Corrupt(format!("{}: the manifest has no source.translation", input.id)))
}

/// A pair folder is usable when it holds a model, a lexicon and the vocabulary or vocabularies.
pub fn is_complete(dir: &Path) -> bool {
    let has = |n: &str| dir.join(n).is_file();
    has("model.bin") && has("lex.bin") && (has("vocab.spm") || (has("srcvocab.spm") && has("trgvocab.spm")))
}

fn remove_dir(dir: &Path) -> Result<(), InstallError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err("cannot remove", dir)(e)),
    }
}

impl Installer for TranslationInstaller {
    fn plan(&self, input: &InstallInput, ctx: &Ctx, _state: &State) -> Result<Vec<String>, InstallError> {
        let pair = need_ref(input)?.pair();
        let dir = ctx.paths.translate_dir().join(&pair);
        if is_complete(&dir) {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!("list the {pair} models in Mozilla's Remote Settings (needs the internet once)"),
            format!(
                "download model, lexicon and vocabulary into {}, each checked against its SHA-256",
                dir.display()
            ),
            "the portal then shows the translate button for articles in this language".into(),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let t = need_ref(input)?;
        let pair = t.pair();
        let dir = ctx.paths.translate_dir().join(&pair);
        let found = models::lookup(ctx.probe, &t.from, &t.to)
            .map_err(|e| InstallError::Unsupported(format!("{}: cannot list the models: {e}", input.id)))?;
        // Download into a scratch folder; the pair folder appears only once everything is verified.
        let tmp = ctx.paths.translate_dir().join(format!(".{pair}.part"));
        remove_dir(&tmp)?;
        fs::create_dir_all(&tmp).map_err(io_err("cannot create", &tmp))?;
        let mut files = Vec::new();
        for f in &found.files {
            let part = tmp.join(&f.name);
            let (size, sha256) = ctx.probe.get_file(&f.url, &part).map_err(|e| {
                let _ = fs::remove_dir_all(&tmp);
                InstallError::Unsupported(format!(
                    "{}: cannot download {} (needs the internet): {e}",
                    input.id, f.name
                ))
            })?;
            if sha256 != f.sha256 || size != f.size {
                let _ = fs::remove_dir_all(&tmp);
                return Err(InstallError::Corrupt(format!(
                    "{}: {} does not match Mozilla's checksum (got {sha256}, expected {}); nothing was installed",
                    input.id, f.name, f.sha256
                )));
            }
            files.push(FileRecord {
                name: format!("translate/{pair}/{}", f.name),
                size,
                sha256,
                source: None,
                moved: false,
            });
        }
        remove_dir(&dir)?;
        fs::rename(&tmp, &dir).map_err(io_err("cannot move into place", &dir))?;
        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Translation,
                version: Some(found.version.clone()),
                license: input.license.clone(),
                installed_at,
                category: Some("translation".into()),
                files,
                service: None,
                app: None,
                model: None,
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(vec![format!(
            "{}: {} models (version {}) are in {}; open an article in the portal and use Translate",
            input.id,
            pair,
            found.version,
            dir.display()
        )])
    }

    fn verify(&self, record: &Record, ctx: &Ctx, deep: bool) -> Vec<Check> {
        let data = &ctx.paths.data_dir;
        let mut checks = Vec::new();
        for f in &record.files {
            let path = data.join(&f.name);
            let (ok, detail) = match fs::metadata(&path) {
                Ok(m) if m.len() == f.size => {
                    if deep {
                        match crate::download::hash_file(&path) {
                            Ok(h) if h == f.sha256 => (true, "checksum ok".to_string()),
                            Ok(_) => (false, "checksum differs".to_string()),
                            Err(e) => (false, e.to_string()),
                        }
                    } else {
                        (true, "size ok".to_string())
                    }
                }
                Ok(_) => (false, "wrong size".into()),
                Err(_) => (
                    false,
                    format!("missing; reinstall with: kit install --select {}", record.id),
                ),
            };
            checks.push(Check {
                name: format!("{}: {}", record.id, f.name),
                ok,
                detail,
            });
        }
        checks
    }

    fn uninstall(
        &self,
        id: &str,
        ctx: &Ctx,
        state: &mut StateGuard,
        _purge: bool,
    ) -> Result<Vec<String>, InstallError> {
        let Some(record) = state.packages.remove(id) else {
            return Ok(vec![format!("{id} is not installed")]);
        };
        for dir in record
            .files
            .iter()
            .filter_map(|f| Path::new(&f.name).parent().map(|p| ctx.paths.data_dir.join(p)))
            .collect::<std::collections::BTreeSet<_>>()
        {
            remove_dir(&dir)?;
        }
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::super::runner::testing::FakeRunner;
    use super::*;
    use crate::catalog::http::{Http, HttpError};

    /// Answers the Remote Settings query and serves canned attachment bodies.
    struct Mozilla {
        list: String,
        files: HashMap<String, Vec<u8>>,
    }

    impl Http for Mozilla {
        fn get(&self, url: &str) -> Result<String, HttpError> {
            if url.starts_with(models::RECORDS_URL) {
                Ok(self.list.clone())
            } else {
                Err(HttpError::Request {
                    url: url.into(),
                    message: "404".into(),
                })
            }
        }

        fn get_file(&self, url: &str, dest: &Path) -> Result<(u64, String), HttpError> {
            let body = self.files.get(url).ok_or_else(|| HttpError::Request {
                url: url.into(),
                message: "404".into(),
            })?;
            fs::write(dest, body).unwrap();
            Ok((body.len() as u64, hex(&Sha256::digest(body))))
        }
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A pair whose files have the given bodies; `lie` makes the list claim another checksum.
    fn mozilla(lie: bool) -> Mozilla {
        let mut recs = Vec::new();
        let mut files = HashMap::new();
        for (t, body) in [("model", "M"), ("lex", "LL"), ("vocab", "VVV")] {
            let location = format!("main-workspace/x/{t}");
            let sha = if lie {
                "0".repeat(64)
            } else {
                hex(&Sha256::digest(body))
            };
            recs.push(format!(
                r#"{{"fileType":"{t}","fromLang":"en","toLang":"de","version":"2.1","attachment":{{"hash":"{sha}","size":{},"location":"{location}"}}}}"#,
                body.len()
            ));
            files.insert(
                format!("{}{location}", models::ATTACHMENTS),
                body.as_bytes().to_vec(),
            );
        }
        Mozilla {
            list: format!(r#"{{"data":[{}]}}"#, recs.join(",")),
            files,
        }
    }

    fn input() -> InstallInput {
        InstallInput {
            id: "translate_en_de".into(),
            name: "Page translation English to German".into(),
            license: "MPL-2.0".into(),
            version: None,
            source: PathBuf::new(),
            file_name: String::new(),
            size: 0,
            sha256: String::new(),
            verified: false,
            image: None,
            service: None,
            content: None,
            flatpak: None,
            group: None,
            collection: None,
            ollama: None,
            translation: Some(TranslationRef {
                from: "en".into(),
                to: "de".into(),
            }),
            model_spec: None,
            category: None,
        }
    }

    fn ctx<'a>(paths: &super::super::Paths, runner: &'a FakeRunner, probe: &'a dyn Http) -> Ctx<'a> {
        Ctx {
            paths: paths.clone(),
            runner,
            probe,
            search_path: vec![],
            port: 6589,
            user: None,
            start_wait: Duration::ZERO,
            now: 1_790_000_000,
            check_port: false,
        }
    }

    #[test]
    fn installs_verified_files_links_them_in_the_portal_and_uninstalls() {
        let dir = tempfile::tempdir().unwrap();
        let paths = super::super::Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        let (r, m) = (
            FakeRunner::new(|_, _| Ok(super::super::runner::CmdOutput::ok())),
            mozilla(false),
        );
        let ctx = ctx(&paths, &r, &m);
        let mut g = StateGuard::open(&paths).unwrap();
        TranslationInstaller.install(&input(), &ctx, &mut g).unwrap();
        let pair = paths.translate_dir().join("en-de");
        assert_eq!(fs::read(pair.join("lex.bin")).unwrap(), b"LL");
        assert!(is_complete(&pair) && !paths.translate_dir().join(".en-de.part").exists());
        let rec = &g.packages["translate_en_de"];
        assert_eq!(
            (rec.kind, rec.version.as_deref(), rec.size_bytes()),
            (PackageType::Translation, Some("2.1"), 6)
        );
        // The portal offers it and links the files for the browser.
        let portal = paths.portal_dir();
        assert!(
            fs::read_to_string(portal.join("read.html"))
                .unwrap()
                .contains("[\"en-de\"]")
        );
        assert!(portal.join("translate/en-de/model.bin").exists());
        // Idempotent, verifiable, removable.
        TranslationInstaller.install(&input(), &ctx, &mut g).unwrap();
        let rec = g.packages["translate_en_de"].clone();
        assert!(TranslationInstaller.verify(&rec, &ctx, true).iter().all(|c| c.ok));
        TranslationInstaller
            .uninstall("translate_en_de", &ctx, &mut g, false)
            .unwrap();
        assert!(!pair.exists() && g.packages.is_empty());
    }

    #[test]
    fn a_wrong_checksum_installs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = super::super::Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
        let (r, m) = (
            FakeRunner::new(|_, _| Ok(super::super::runner::CmdOutput::ok())),
            mozilla(true),
        );
        let ctx = ctx(&paths, &r, &m);
        let mut g = StateGuard::open(&paths).unwrap();
        let err = TranslationInstaller.install(&input(), &ctx, &mut g).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
        assert!(g.packages.is_empty());
        assert!(
            !paths.translate_dir().join("en-de").exists()
                && !paths.translate_dir().join(".en-de.part").exists()
        );
    }
}
