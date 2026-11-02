//! End to end for the assistant page's plumbing (opt-in): real `kiwix-serve`, `caddy` and a
//! running Ollama that already holds the model. Nothing on the host changes: Caddy and Kiwix run as
//! child processes on random loopback ports; Ollama is only asked to answer one tiny question.
//!
//!   KIT_E2E_ZIM=/path/small.zim [KIT_E2E_MODEL=granite4.2:3b] \
//!     cargo test --test e2e_ai -- --ignored --nocapture
//!
//! Needs `kiwix-manage`, `kiwix-serve`, `caddy` and `curl` on PATH, and Ollama on 127.0.0.1:11434.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use emergency_kit::install::Paths;
use emergency_kit::install::model::OLLAMA_PORT;
use emergency_kit::install::portal;
use emergency_kit::install::state::{ModelRecord, Record, State};
use emergency_kit::netcfg::ca::Ca;
use emergency_kit::netcfg::caddy::{CaddySpec, render_caddyfile};

const NOW: u64 = 1_788_000_000;

struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for(port: u16) {
    let end = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < end, "nothing listens on port {port}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn spawn(cmd: &mut Command, log: &Path) -> Proc {
    let f = fs::File::create(log).unwrap();
    Proc(
        cmd.stdout(Stdio::from(f.try_clone().unwrap()))
            .stderr(Stdio::from(f))
            .spawn()
            .expect("tool not found on PATH"),
    )
}

/// `(status, body)` of a request to `https://kit.local:<port><path>` through the local CA.
fn curl(ca: &Path, port: u16, path: &str, post: Option<&str>) -> (u16, String) {
    let mut c = Command::new("curl");
    c.args(["-s", "--max-time", "120", "--cacert"])
        .arg(ca)
        .args(["--resolve", &format!("kit.local:{port}:127.0.0.1")])
        .args(["-w", "\n%{http_code}"])
        .arg(format!("https://kit.local:{port}{path}"));
    if let Some(body) = post {
        c.args(["-H", "Content-Type: application/json", "-d", body]);
    }
    let out = c.output().expect("curl not found");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

#[test]
#[ignore = "needs kiwix-tools, caddy, curl, a running Ollama with the model and KIT_E2E_ZIM"]
fn the_front_serves_the_assistant_and_limits_the_ollama_api() {
    let zim = std::env::var_os("KIT_E2E_ZIM").expect("set KIT_E2E_ZIM to a small .zim file");
    let model = std::env::var("KIT_E2E_MODEL").unwrap_or_else(|_| "granite4.2:3b".into());
    let d = tempfile::tempdir().unwrap();
    let root = d.path();

    let lib = root.join("library.xml");
    assert!(
        Command::new("kiwix-manage")
            .arg(&lib)
            .arg("add")
            .arg(&zim)
            .status()
            .unwrap()
            .success()
    );
    let kiwix_port = free_port();
    let _kiwix = spawn(
        Command::new("kiwix-serve")
            .args([
                "--library",
                "--address=127.0.0.1",
                &format!("--port={kiwix_port}"),
            ])
            .arg(&lib),
        &root.join("kiwix.log"),
    );
    wait_for(kiwix_port);

    // The portal files exactly as the installer writes them.
    let paths = Paths::new(root.join("data"), root.join("units")).unwrap();
    let mut state = State::default();
    state.packages.insert(
        "llm".into(),
        Record {
            id: "llm".into(),
            name: "Assistant".into(),
            kind: emergency_kit::catalog::PackageType::Model,
            version: None,
            license: "Apache-2.0".into(),
            installed_at: NOW,
            category: None,
            files: vec![],
            service: None,
            app: None,
            map: None,
            model: Some(ModelRecord {
                name: model.clone(),
                size: 1,
                by_kit: false,
                thinking: false,
            }),
        },
    );
    portal::write(&paths, &state).unwrap();

    let hosts = vec!["kit.local".to_string()];
    let ca = Ca::create(&root.join("ca"), &hosts, NOW).unwrap();
    ca.ensure_leaf(&hosts, NOW).unwrap();
    let listen_port = free_port();
    let caddyfile = root.join("Caddyfile");
    fs::write(
        &caddyfile,
        render_caddyfile(&CaddySpec {
            mirrors: &[],
            services: &[],
            cert: ca.files.leaf_cert(),
            key: ca.files.leaf_key(),
            listen_port,
            http_port: listen_port + 1,
            kiwix_port,
            portal_dir: paths.portal_dir(),
            portal_host: "kit.local".into(),
            ollama_port: Some(OLLAMA_PORT),
        }),
    )
    .unwrap();
    let valid = Command::new("caddy")
        .args(["validate", "--adapter", "caddyfile", "--config"])
        .arg(&caddyfile)
        .output()
        .unwrap();
    assert!(
        valid.status.success(),
        "caddy rejects the Caddyfile:\n{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let _caddy = spawn(
        Command::new("caddy")
            .args(["run", "--adapter", "caddyfile", "--config"])
            .arg(&caddyfile),
        &root.join("caddy.log"),
    );
    wait_for(listen_port);
    let ca_pem = ca.files.cert();

    // The page, its script and its config come from the portal directory.
    let (status, page) = curl(&ca_pem, listen_port, "/ai/", None);
    assert_eq!(
        (status, page.contains("Offline assistant")),
        (200, true),
        "{page}"
    );
    let (status, js) = curl(&ca_pem, listen_port, "/ai/rag.js", None);
    assert_eq!((status, js.contains("parseSearch")), (200, true));
    let (_, cfg) = curl(&ca_pem, listen_port, "/ai/config.json", None);
    assert!(cfg.contains(&model), "{cfg}");

    // The page's retrieval path: Kiwix search as XML through the front.
    let (status, xml) = curl(
        &ca_pem,
        listen_port,
        "/search?pattern=a&format=xml&pageLength=3",
        None,
    );
    assert_eq!(status, 200);
    assert!(xml.contains("<item>") && xml.contains("/content/"), "{xml}");

    // The Ollama API: exactly tags and chat, nothing else.
    let (status, tags) = curl(&ca_pem, listen_port, "/ai/api/tags", None);
    assert_eq!(status, 200, "{tags}");
    assert!(tags.contains(&model), "the model must be pulled first: {tags}");
    let (status, _) = curl(&ca_pem, listen_port, "/ai/api/pull", Some(r#"{"model":"x"}"#));
    assert_eq!(status, 403);
    let (status, _) = curl(
        &ca_pem,
        listen_port,
        "/ai/api/delete",
        Some(&format!(r#"{{"model":"{model}"}}"#)),
    );
    assert_eq!(status, 403);
    let chat = format!(
        r#"{{"model":"{model}","stream":false,"options":{{"num_predict":8}},"messages":[{{"role":"user","content":"Say OK."}}]}}"#
    );
    let (status, answer) = curl(&ca_pem, listen_port, "/ai/api/chat", Some(&chat));
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("\"message\""), "{answer}");
}
