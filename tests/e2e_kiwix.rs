//! End to end with the real `kiwix-manage` and `kiwix-serve` (opt-in).
//!
//!   KIT_E2E_ZIM=/path/to/small.zim cargo test --test e2e_kiwix -- --ignored --nocapture
//!
//! Needs `kiwix-tools` on PATH. systemd is NOT touched: a shim `systemctl` in a temp directory
//! starts and stops `kiwix-serve` from the unit's `ExecStart` line, and everything else
//! (data dir, unit dir) lives in temp directories too.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

const SHIM: &str = r#"#!/bin/sh
# Minimal stand-in for `systemctl --user` that runs the unit's ExecStart as a background process.
shift                                  # --user
cmd="$1"
unit="$XDG_CONFIG_HOME/systemd/user/kit-kiwix.service"
pidf="$SHIM_STATE/pid"
running() { [ -f "$pidf" ] && kill -0 "$(cat "$pidf")" 2>/dev/null; }
stop() { if running; then kill "$(cat "$pidf")" 2>/dev/null; sleep 0.3; fi; rm -f "$pidf"; }
start() {
  running && return 0
  line=$(sed -n 's/^ExecStart=//p' "$unit")
  sh -c "exec $line" >"$SHIM_STATE/serve.log" 2>&1 &
  echo $! > "$pidf"
}
case "$cmd" in
  daemon-reload) exit 0 ;;
  enable) start ;;
  restart) stop; start ;;
  disable) stop ;;
  is-active) running ;;
  *) exit 0 ;;
esac
"#;

struct Sandbox {
    root: tempfile::TempDir,
    port: u16,
    zim: PathBuf,
}

impl Sandbox {
    fn path_dir(&self) -> PathBuf {
        self.root.path().join("shim")
    }
    fn data(&self) -> PathBuf {
        self.root.path().join("data")
    }
    fn state(&self) -> PathBuf {
        self.root.path().join("shimstate")
    }

    fn kit(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.path_dir().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_kit"))
            .args([
                "--catalog",
                self.root.path().join("catalog").to_str().unwrap(),
                "--offline",
            ])
            .args(args)
            .args(["--data-dir", self.data().to_str().unwrap()])
            .env("PATH", path)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("SHIM_STATE", self.state())
            .output()
            .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.state().join("pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
    }
}

fn setup(zim: &Path) -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let bytes = fs::read(zim).unwrap();
    let sha: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = zim.file_name().unwrap().to_str().unwrap();

    let shim = root.path().join("shim");
    fs::create_dir_all(&shim).unwrap();
    fs::write(shim.join("systemctl"), SHIM).unwrap();
    fs::set_permissions(shim.join("systemctl"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::create_dir_all(root.path().join("shimstate")).unwrap();

    fs::create_dir_all(root.path().join("catalog")).unwrap();
    fs::write(
        root.path().join("catalog/e2e.yaml"),
        format!(
            "id: e2e\nname: End to end test ZIM\ndescription: d\ncategory: test\ntype: zim\n\
             source: {{url: 'http://127.0.0.1:1/{name}'}}\nsha256: {sha}\nsize_bytes: {}\n\
             license: {{spdx: CC0-1.0}}\n",
            bytes.len()
        ),
    )
    .unwrap();

    let dl = root.path().join("data/downloads/e2e");
    fs::create_dir_all(&dl).unwrap();
    fs::copy(zim, dl.join(name)).unwrap();
    Sandbox {
        root,
        port,
        zim: zim.into(),
    }
}

fn http_get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    body
}

fn text(o: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
#[ignore = "needs kiwix-tools and KIT_E2E_ZIM=<small .zim file>"]
fn install_serve_verify_uninstall_with_real_kiwix_tools() {
    let Some(zim) = std::env::var_os("KIT_E2E_ZIM") else {
        panic!("set KIT_E2E_ZIM to the path of a small .zim file");
    };
    let sb = setup(Path::new(&zim));
    let port = sb.port.to_string();

    let out = sb.kit(&["install", "--select", "e2e", "--no-download", "--port", &port]);
    assert!(out.status.success(), "install failed:\n{}", text(&out));
    let log = fs::read_to_string(sb.state().join("serve.log")).unwrap_or_default();
    assert!(
        sb.data().join("library/library.xml").exists(),
        "{}\n{log}",
        text(&out)
    );
    let unit = fs::read_to_string(sb.root.path().join("config/systemd/user/kit-kiwix.service")).unwrap();
    assert!(unit.contains(&format!("--port={port}")), "{unit}");

    // The server really serves the book.
    let deadline = Instant::now() + Duration::from_secs(15);
    let feed = loop {
        let feed = TcpStream::connect(("127.0.0.1", sb.port))
            .map(|_| http_get(sb.port, "/catalog/v2/entries?count=1"));
        match feed {
            Ok(f) if f.contains("<totalResults>1</totalResults>") => break f,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(300)),
            other => panic!("server never listed the book: {other:?}\n{log}"),
        }
    };
    println!("served feed:\n{}", &feed[..feed.len().min(600)]);

    let out = sb.kit(&["verify", "--deep"]);
    assert!(out.status.success(), "verify failed:\n{}", text(&out));
    println!("{}", text(&out));

    // Idempotent: a second install changes nothing and the server keeps running.
    let out = sb.kit(&["install", "--select", "e2e", "--no-download", "--port", &port]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("already installed"));
    assert!(http_get(sb.port, "/catalog/v2/entries?count=1").contains("<totalResults>1</totalResults>"));

    let out = sb.kit(&["uninstall", "e2e", "--purge"]);
    assert!(out.status.success(), "{}", text(&out));
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        TcpStream::connect(("127.0.0.1", sb.port)).is_err(),
        "server is still running after uninstall"
    );
    assert!(
        !sb.root
            .path()
            .join("config/systemd/user/kit-kiwix.service")
            .exists()
    );
    assert!(!sb.data().join("library/library.xml").exists());
    let _ = &sb.zim;
}
