//! Installer for `model` packages: a language model held by a standard Ollama. kit does not
//! install or run Ollama; it needs the `ollama` program and a running server (`ollama serve`, or
//! the distribution's service). Ollama verifies the SHA-256 of every layer it pulls against the
//! registry manifest, so there is no separate checksum step, as with Flatpak.

use serde::Deserialize;

use super::state::{ModelRecord, Record, State, StateGuard};
use super::tools::{require, s};
use super::{Check, Ctx, InstallError, InstallInput, Installer, io_err};
use crate::catalog::PackageType;
use crate::catalog::manifest::OllamaRef;

/// Where the Ollama server listens by default.
pub const OLLAMA_PORT: u16 = 11434;
pub const OLLAMA_URL: &str = "http://127.0.0.1:11434";

pub struct ModelInstaller;

#[derive(Deserialize)]
struct Tags {
    models: Vec<Tag>,
}

#[derive(Deserialize)]
struct Tag {
    name: String,
}

fn need_ref(input: &InstallInput) -> Result<&OllamaRef, InstallError> {
    input
        .ollama
        .as_ref()
        .ok_or_else(|| InstallError::Corrupt(format!("{}: the manifest has no source.ollama", input.id)))
}

fn server_hint() -> String {
    "start it with `systemctl --user start ollama` or `sudo systemctl enable --now ollama` \
     (distribution package), or run `ollama serve` in another terminal"
        .into()
}

/// The models the server holds, or an actionable error when it is not running.
fn held_models(ctx: &Ctx) -> Result<Vec<String>, InstallError> {
    let url = format!("{OLLAMA_URL}/api/tags");
    let body = ctx.probe.get(&url).map_err(|_| {
        InstallError::Unsupported(format!(
            "the Ollama server does not answer on {OLLAMA_URL}; {}",
            server_hint()
        ))
    })?;
    let tags: Tags = serde_json::from_str(&body)
        .map_err(|e| InstallError::Corrupt(format!("{url}: unexpected answer: {e}")))?;
    Ok(tags.models.into_iter().map(|t| t.name).collect())
}

fn holds(models: &[String], name: &str) -> bool {
    models.iter().any(|m| m == name)
}

fn run(ctx: &Ctx, tool: &str, args: &[&str]) -> Result<(), InstallError> {
    let out = ctx
        .runner
        .run(tool, args)
        .map_err(io_err("cannot run", std::path::Path::new(tool)))?;
    if out.success {
        Ok(())
    } else {
        Err(InstallError::Command {
            command: format!("{tool} {}", args.join(" ")),
            stderr: out.stderr.trim().to_string(),
        })
    }
}

impl Installer for ModelInstaller {
    fn plan(&self, input: &InstallInput, _ctx: &Ctx, state: &State) -> Result<Vec<String>, InstallError> {
        let name = need_ref(input)?.name();
        if state.packages.get(&input.id).is_some_and(|r| r.model.is_some()) {
            return Ok(vec![format!("{}: already installed, nothing to do", input.id)]);
        }
        Ok(vec![
            format!("check that the Ollama server answers on {OLLAMA_URL}"),
            format!("ollama pull {name} (needs the internet once; Ollama checks every layer's SHA-256)"),
            format!("try it with: ollama run {name}"),
        ])
    }

    fn install(
        &self,
        input: &InstallInput,
        ctx: &Ctx,
        state: &mut StateGuard,
    ) -> Result<Vec<String>, InstallError> {
        let name = need_ref(input)?.name();
        let tool = require("ollama", &ctx.search_path)?;
        let tool = s(&tool);
        let held = held_models(ctx)?;
        let was_there = holds(&held, &name);
        // A record from an earlier install decides who removes the model again.
        let by_kit = match state.packages.get(&input.id).and_then(|r| r.model.as_ref()) {
            Some(m) => m.by_kit,
            None => !was_there,
        };
        if !was_there {
            run(ctx, tool, &["pull", &name])?;
            if !holds(&held_models(ctx)?, &name) {
                return Err(InstallError::Corrupt(format!(
                    "{}: `ollama pull {name}` finished but the server does not list the model",
                    input.id
                )));
            }
        }
        let installed_at = state.packages.get(&input.id).map_or(ctx.now, |r| r.installed_at);
        state.packages.insert(
            input.id.clone(),
            Record {
                id: input.id.clone(),
                name: input.name.clone(),
                kind: PackageType::Model,
                version: input.version.clone(),
                license: input.license.clone(),
                installed_at,
                category: None,
                files: vec![],
                service: None,
                app: None,
                map: None,
                model: Some(ModelRecord {
                    name: name.clone(),
                    size: input.size,
                    by_kit,
                    thinking: input.model_spec.as_ref().is_some_and(|m| m.thinking),
                }),
            },
        );
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(vec![format!(
            "{}: try it with `ollama run {name}`; small models can be wrong, so check first-aid advice against the library",
            input.id
        )])
    }

    fn verify(&self, record: &Record, ctx: &Ctx, _deep: bool) -> Vec<Check> {
        let Some(m) = &record.model else {
            return vec![Check {
                name: format!("{}: model record", record.id),
                ok: false,
                detail: "no model in the state".into(),
            }];
        };
        let (ok, detail) = match held_models(ctx) {
            Ok(held) if holds(&held, &m.name) => (true, format!("ollama run {}", m.name)),
            Ok(_) => (
                false,
                format!(
                    "Ollama does not hold it; reinstall with: kit install --select {}",
                    record.id
                ),
            ),
            Err(e) => (false, e.to_string()),
        };
        vec![Check {
            name: format!("{}: Ollama model {}", record.id, m.name),
            ok,
            detail,
        }]
    }

    fn uninstall(
        &self,
        id: &str,
        ctx: &Ctx,
        state: &mut StateGuard,
        _purge: bool,
    ) -> Result<Vec<String>, InstallError> {
        let Some(record) = state.packages.get(id).cloned() else {
            return Ok(vec![format!("{id} is not installed")]);
        };
        let mut notes = Vec::new();
        if let Some(m) = &record.model {
            if !m.by_kit {
                notes.push(format!("{} was there before kit; it stays in Ollama", m.name));
            } else if holds(&held_models(ctx)?, &m.name) {
                let tool = require("ollama", &ctx.search_path)?;
                run(ctx, s(&tool), &["rm", &m.name])?;
            }
        }
        state.packages.remove(id);
        state.save()?;
        super::portal::write(&ctx.paths, state)?;
        Ok(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::CmdOutput;
    use super::super::runner::testing::FakeRunner;
    use super::super::tools::fake_exe;
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Duration;

    const NAME: &str = "granite4.2:3b";

    struct Env {
        _dir: tempfile::TempDir,
        paths: super::super::Paths,
        bin: PathBuf,
    }

    impl Env {
        fn new(with_tool: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            if with_tool {
                fake_exe(&bin, "ollama");
            }
            let paths = super::super::Paths::new(dir.path().join("data"), dir.path().join("units")).unwrap();
            Self {
                _dir: dir,
                paths,
                bin,
            }
        }
    }

    fn input() -> InstallInput {
        InstallInput {
            id: "llm_granite_3b".into(),
            name: "Granite 4.2 3B".into(),
            license: "Apache-2.0".into(),
            version: Some("20e436143017".into()),
            source: PathBuf::new(),
            file_name: String::new(),
            size: 2_244_023_548,
            sha256: String::new(),
            verified: false,
            image: None,
            service: None,
            content: None,
            flatpak: None,
            group: None,
            map: None,
            ollama: Some(OllamaRef {
                model: "granite4.2".into(),
                tag: "3b".into(),
            }),
            category: None,
            model_spec: Some(crate::catalog::manifest::ModelSpec {
                thinking: true,
                context_k: 128,
            }),
        }
    }

    fn tags(names: &[String]) -> String {
        let models: Vec<_> = names.iter().map(|n| format!("{{\"name\":\"{n}\"}}")).collect();
        format!("{{\"models\":[{}]}}", models.join(","))
    }

    /// The server's model list. `ollama pull` / `rm` (the fake runner) change it; the probe reads it.
    #[derive(Clone)]
    struct Server {
        held: Rc<RefCell<Vec<String>>>,
        up: Rc<Cell<bool>>,
    }

    impl crate::catalog::http::Http for Server {
        fn get(&self, url: &str) -> Result<String, crate::catalog::http::HttpError> {
            if !self.up.get() || !url.ends_with("/api/tags") {
                return Err(crate::catalog::http::HttpError::Request {
                    url: url.into(),
                    message: "connection refused".into(),
                });
            }
            Ok(tags(&self.held.borrow()))
        }
    }

    fn host(initial: &[&str], pull_ok: bool) -> (Server, FakeRunner) {
        let server = Server {
            held: Rc::new(RefCell::new(initial.iter().map(|s| s.to_string()).collect())),
            up: Rc::new(Cell::new(true)),
        };
        let held = server.held.clone();
        let runner = FakeRunner::new(move |_, args| {
            Ok(match args {
                ["pull", name] if pull_ok => {
                    held.borrow_mut().push(name.to_string());
                    CmdOutput::ok()
                }
                ["pull", _] => CmdOutput::failed("pull model manifest: file does not exist"),
                ["rm", name] => {
                    held.borrow_mut().retain(|m| m != name);
                    CmdOutput::ok()
                }
                _ => CmdOutput::ok(),
            })
        });
        (server, runner)
    }

    fn ctx<'a>(e: &Env, runner: &'a FakeRunner, server: &'a Server) -> Ctx<'a> {
        Ctx {
            paths: e.paths.clone(),
            runner,
            probe: server,
            search_path: vec![e.bin.clone()],
            port: 6589,
            user: None,
            start_wait: Duration::ZERO,
            now: 1_790_000_000,
            check_port: false,
        }
    }

    #[test]
    fn a_missing_model_is_pulled_and_owned_by_kit() {
        let e = Env::new(true);
        let (srv, runner) = host(&[], true);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let plan = ModelInstaller
            .plan(&input(), &ctx(&e, &runner, &srv), &g)
            .unwrap();
        assert!(plan.iter().any(|l| l.contains("ollama pull granite4.2:3b")));
        let notes = ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap();
        assert_eq!(runner.count(&format!("pull {NAME}")), 1);
        let m = g.packages["llm_granite_3b"].model.clone().unwrap();
        assert_eq!((m.name.as_str(), m.by_kit, m.size), (NAME, true, 2_244_023_548));
        assert_eq!(g.packages["llm_granite_3b"].size_bytes(), 2_244_023_548);
        assert!(notes[0].contains("ollama run granite4.2:3b"));
        // Re-running changes nothing and keeps the owner.
        ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap();
        assert_eq!(runner.count("pull"), 1);
        assert!(g.packages["llm_granite_3b"].model.as_ref().unwrap().by_kit);
    }

    #[test]
    fn a_failed_pull_is_reported_and_nothing_is_recorded() {
        let e = Env::new(true);
        let (srv, runner) = host(&[], false);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("file does not exist"), "{err}");
        assert!(g.packages.is_empty());
    }

    #[test]
    fn missing_tool_and_stopped_server_are_explained() {
        let (srv, runner) = host(&[], true);
        let e = Env::new(false);
        let mut g = StateGuard::open(&e.paths).unwrap();
        let err = ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ollama") && err.contains("https://ollama.com"),
            "{err}"
        );

        let e = Env::new(true);
        srv.up.set(false);
        let err = ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ollama serve") && err.contains("11434"), "{err}");
        assert!(runner.calls().is_empty() && g.packages.is_empty());
    }

    #[test]
    fn a_model_that_was_there_before_is_kept_and_an_owned_one_is_removed() {
        let e = Env::new(true);
        let (srv, runner) = host(&[NAME], true);
        let mut g = StateGuard::open(&e.paths).unwrap();
        ModelInstaller
            .install(&input(), &ctx(&e, &runner, &srv), &mut g)
            .unwrap();
        assert_eq!(runner.count("pull"), 0);
        let rec = g.packages["llm_granite_3b"].clone();
        assert!(!rec.model.as_ref().unwrap().by_kit);
        assert!(
            ModelInstaller
                .verify(&rec, &ctx(&e, &runner, &srv), false)
                .iter()
                .all(|c| c.ok)
        );

        let notes = ModelInstaller
            .uninstall("llm_granite_3b", &ctx(&e, &runner, &srv), &mut g, false)
            .unwrap();
        assert_eq!(runner.count("rm"), 0);
        assert!(notes[0].contains("stays in Ollama") && g.packages.is_empty());
        ModelInstaller
            .uninstall("llm_granite_3b", &ctx(&e, &runner, &srv), &mut g, false)
            .unwrap();

        let mut owned = rec.clone();
        owned.model.as_mut().unwrap().by_kit = true;
        g.packages.insert(owned.id.clone(), owned);
        ModelInstaller
            .uninstall("llm_granite_3b", &ctx(&e, &runner, &srv), &mut g, false)
            .unwrap();
        assert_eq!(runner.count(&format!("rm {NAME}")), 1);
        assert!(
            !ModelInstaller
                .verify(&rec, &ctx(&e, &runner, &srv), false)
                .iter()
                .all(|c| c.ok)
        );
    }
}
