//! Phase 2 acceptance scenario on the real `kit` binary: `kill -9` mid-download and a network
//! outage during a run, both ending in a verified file. Uses a local server and temp dirs only.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::server::{Generated, Server};
use emergency_kit::download::state::State;

const CHUNK: u64 = 1024 * 1024;
const SIZE: u64 = 12 * CHUNK + 4321;

struct Setup {
    _dir: tempfile::TempDir,
    catalog: PathBuf,
    data: PathBuf,
    server: Server,
    sha: String,
}

impl Setup {
    fn new() -> Self {
        let content = Generated { len: SIZE, seed: 7 };
        let sha = content.sha256();
        let server = Server::start(content);
        // ~3 MB/s per connection: a chunk takes a few hundred ms, the whole file a few seconds.
        server.faults.piece_delay_ms.store(20, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::write(
            catalog.join("big.yaml"),
            format!(
                "id: big\nname: Big\ndescription: d\ncategory: test\ntype: files\n\
                 source: {{url: '{}'}}\nsha256: {sha}\nsize_bytes: {SIZE}\nlicense: {{spdx: CC0-1.0}}\n",
                server.url("/big.bin")
            ),
        )
        .unwrap();
        let data = dir.path().join("data");
        Self {
            _dir: dir,
            catalog,
            data,
            server,
            sha,
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kit"));
        c.args([
            "--catalog",
            self.catalog.to_str().unwrap(),
            "--offline",
            "download",
            "--select",
            "big",
        ])
        .args([
            "--data-dir",
            self.data.to_str().unwrap(),
            "--jobs",
            "2",
            "--chunk-size",
            &CHUNK.to_string(),
        ]);
        c
    }

    fn dir(&self) -> PathBuf {
        self.data.join("downloads/big")
    }

    fn state(&self) -> Option<State> {
        State::load(&self.dir().join("big.bin.part.state"))
    }

    fn chunks_done(&self) -> usize {
        self.state().map_or(0, |s| s.chunk_count() - s.pending().len())
    }

    fn final_sha(&self) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(self.dir().join("big.bin")).unwrap();
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

fn wait_until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn(cmd: &mut Command) -> Child {
    cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
}

fn assert_no_leftovers(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".lock"))
        .collect();
    assert_eq!(names, vec!["big.bin"], "only the verified file remains");
}

#[test]
fn kill_9_mid_download_then_resume_finishes_with_verified_checksum() {
    let s = Setup::new();
    let mut child = spawn(&mut s.command());
    wait_until("3 chunks to be stored", 30, || s.chunks_done() >= 3);

    child.kill().unwrap(); // SIGKILL: no cleanup code runs
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(
        !s.dir().join("big.bin").exists(),
        "nothing is installed under the final name"
    );
    assert!(s.dir().join("big.bin.part").exists());

    let state = s.state().expect("progress survived the kill");
    let pending = state.pending().len();
    assert!(
        pending > 0 && pending < state.chunk_count(),
        "interrupted mid-way, {pending} chunks left"
    );
    let before = s.server.requests();

    let out = s.command().output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("resumed after"));
    assert_eq!(
        s.server.requests() - before,
        pending as u64,
        "only the missing chunks were requested"
    );
    assert_eq!(s.final_sha(), s.sha);
    assert_no_leftovers(&s.dir());

    // A third run is a no-op.
    let before = s.server.requests();
    let out = s.command().output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("already downloaded and verified"));
    assert_eq!(s.server.requests(), before);
}

#[test]
fn repeated_kill_9_still_converges() {
    let s = Setup::new();
    for round in 1..=3 {
        let mut child = spawn(&mut s.command());
        wait_until("progress", 30, || {
            s.chunks_done() >= round * 2 || child.try_wait().unwrap().is_some()
        });
        child.kill().ok();
        child.wait().unwrap();
    }
    let out = s.command().output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(s.final_sha(), s.sha);
    assert_no_leftovers(&s.dir());
}

#[test]
fn network_outage_during_the_run_is_survived() {
    let s = Setup::new();
    let mut child = spawn(&mut s.command());
    wait_until("2 chunks to be stored", 30, || s.chunks_done() >= 2);

    s.server.faults.down.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "kit keeps retrying instead of exiting"
    );
    s.server.faults.down.store(false, Ordering::SeqCst);

    wait_until("kit to finish", 60, || child.try_wait().unwrap().is_some());
    assert!(child.wait().unwrap().success());
    assert_eq!(s.final_sha(), s.sha);
    assert_no_leftovers(&s.dir());
}

#[test]
fn second_kit_on_the_same_file_is_refused_while_the_first_runs() {
    let s = Setup::new();
    let mut first = spawn(&mut s.command());
    wait_until("first process to start downloading", 30, || s.chunks_done() >= 1);
    let out = s.command().output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("another kit process"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    first.kill().ok();
    first.wait().unwrap();
}

#[test]
fn download_refuses_when_disk_space_is_insufficient() {
    let s = Setup::new();
    // Rewrite the package as absurdly large so no disk can hold it; nothing must be fetched.
    let yaml = std::fs::read_to_string(s.catalog.join("big.yaml")).unwrap();
    std::fs::write(
        s.catalog.join("big.yaml"),
        yaml.replace(&format!("size_bytes: {SIZE}"), "size_bytes: 9000000000000000000"),
    )
    .unwrap();
    let out = s.command().output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("INSUFFICIENT DISK SPACE"));
    assert_eq!(s.server.requests(), 0);
    assert!(!s.dir().exists() || std::fs::read_dir(s.dir()).unwrap().count() == 0);
}
