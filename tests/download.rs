//! The download engine against a real HTTP server on localhost (with injected faults).

mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::server::{Bytes, Generated, Server, sample, sha256_hex};
use emergency_kit::download::{
    DownloadError, NoProgress, Options, Outcome, RateLimiter, Target, UreqRange, download, remaining_bytes,
};

const KIB: u64 = 1024;

fn opts(chunk: u64, jobs: usize) -> Options {
    Options {
        jobs,
        chunk_size: chunk,
        retries: 6,
        backoff_base: Duration::from_millis(5),
        backoff_cap: Duration::from_millis(40),
    }
}

fn client(jobs: usize) -> UreqRange {
    UreqRange::new(Arc::new(RateLimiter::new(None)), jobs)
}

fn target(dir: &std::path::Path, bytes: &[u8], sources: Vec<String>) -> Target {
    Target {
        name: "data.bin".into(),
        dir: dir.into(),
        sources,
        size: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    }
}

#[test]
fn downloads_and_verifies_a_multi_chunk_file() {
    let bytes = sample(3 * 1024 * 1024 + 123);
    let server = Server::start(Bytes(bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    let out = download(&t, &opts(256 * KIB, 4), &client(4), &NoProgress).unwrap();
    assert_eq!(out, Outcome::Downloaded { resumed_bytes: 0 });
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    assert!(!t.part_path().exists() && !t.state_path().exists());
    assert_eq!(remaining_bytes(&t), 0);
}

#[test]
fn survives_dropped_connections_and_503s() {
    let bytes = sample(2 * 1024 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    server.faults.reset_mid_body.store(3, Ordering::SeqCst);
    server.faults.status_burst.store(2, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    download(&t, &opts(256 * KIB, 2), &client(2), &NoProgress).unwrap();
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    assert!(
        server.requests() > 8,
        "chunks were requested again after the faults"
    );
}

#[test]
fn resets_on_every_third_request_still_finish() {
    let bytes = sample(2 * 1024 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    server.faults.reset_every.store(3, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    download(&t, &opts(128 * KIB, 4), &client(4), &NoProgress).unwrap();
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
}

#[test]
fn network_loss_then_recovery_on_the_next_run_resumes() {
    let bytes = sample(2 * 1024 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);

    // Take the server down after a few requests; the run must give up cleanly and keep progress.
    server.faults.piece_delay_ms.store(5, Ordering::SeqCst);
    let faults = server.faults.clone();
    let killer = std::thread::spawn(move || {
        while faults.requests.load(Ordering::SeqCst) < 4 {
            std::thread::sleep(Duration::from_millis(1));
        }
        faults.down.store(true, Ordering::SeqCst);
    });
    let first = download(&t, &opts(128 * KIB, 1), &client(1), &NoProgress);
    killer.join().unwrap();
    assert!(matches!(first, Err(DownloadError::Failed(_))), "{first:?}");
    assert!(t.part_path().exists() && !t.final_path().exists());
    let left = remaining_bytes(&t);
    assert!(left > 0 && left < t.size, "partial progress is on disk: {left}");

    // Network back: continues and ends with a verified file.
    server.faults.down.store(false, Ordering::SeqCst);
    server.faults.piece_delay_ms.store(0, Ordering::SeqCst);
    let out = download(&t, &opts(128 * KIB, 4), &client(4), &NoProgress).unwrap();
    assert!(
        matches!(out, Outcome::Downloaded { resumed_bytes } if resumed_bytes > 0),
        "{out:?}"
    );
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
}

#[test]
fn flipped_byte_on_the_server_is_caught_by_the_checksum() {
    let bytes = sample(1024 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    server.faults.flip_byte_at.store(700_000, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    let err = download(&t, &opts(256 * KIB, 2), &client(2), &NoProgress).unwrap_err();
    assert!(matches!(err, DownloadError::Corrupt { .. }), "{err:?}");
    assert!(!t.final_path().exists() && !t.part_path().exists() && !t.state_path().exists());
}

#[test]
fn server_without_range_support_is_rejected() {
    let bytes = sample(300 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    server.faults.ignore_range.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    let err = download(&t, &opts(64 * KIB, 2), &client(2), &NoProgress).unwrap_err();
    assert!(
        matches!(&err, DownloadError::Failed(m) if m.contains("no usable source")),
        "{err:?}"
    );
}

#[test]
fn wrong_file_size_on_the_server_is_rejected() {
    let bytes = sample(300 * 1024);
    let server = Server::start(Bytes(sample(300 * 1024 + 1)));
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    let err = download(&t, &opts(64 * KIB, 2), &client(2), &NoProgress).unwrap_err();
    assert!(
        matches!(&err, DownloadError::Failed(m) if m.contains("expected")),
        "{err:?}"
    );
}

#[test]
fn dead_primary_falls_back_to_mirror() {
    let bytes = sample(1024 * 1024);
    let primary = Server::start(Bytes(bytes.clone()));
    let mirror = Server::start(Bytes(bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let t = target(
        dir.path(),
        &bytes,
        vec![primary.url("/missing/data.bin"), mirror.url("/data.bin")],
    );
    download(&t, &opts(128 * KIB, 3), &client(3), &NoProgress).unwrap();
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    assert!(mirror.requests() >= 8);
}

#[test]
fn unreachable_primary_falls_back_to_mirror() {
    let bytes = sample(512 * 1024);
    let primary = Server::start(Bytes(bytes.clone()));
    primary.faults.down.store(true, Ordering::SeqCst);
    let mirror = Server::start(Bytes(bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let t = target(
        dir.path(),
        &bytes,
        vec![primary.url("/data.bin"), mirror.url("/data.bin")],
    );
    download(&t, &opts(128 * KIB, 1), &client(1), &NoProgress).unwrap();
    assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
}

#[test]
fn bandwidth_limit_is_respected() {
    let bytes = sample(400 * 1024);
    let server = Server::start(Bytes(bytes.clone()));
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), &bytes, vec![server.url("/data.bin")]);
    let limited = UreqRange::new(Arc::new(RateLimiter::new(Some(200_000))), 2);
    let started = Instant::now();
    download(&t, &opts(100 * KIB, 2), &limited, &NoProgress).unwrap();
    let secs = started.elapsed().as_secs_f64();
    // 409,600 bytes at 200,000 B/s is about 2 s; the first piece is free.
    assert!(secs > 1.5, "finished too fast for the limit: {secs:.2}s");
    assert!(secs < 8.0, "far slower than the limit: {secs:.2}s");
}

#[test]
fn generated_stream_checksum_matches_what_is_served() {
    let content = Generated {
        len: 5 * 1024 * 1024 + 7,
        seed: 1,
    };
    let sha = content.sha256();
    let server = Server::start(content);
    let dir = tempfile::tempdir().unwrap();
    let t = Target {
        name: "gen.bin".into(),
        dir: dir.path().into(),
        sources: vec![server.url("/gen.bin")],
        size: 5 * 1024 * 1024 + 7,
        sha256: sha,
    };
    download(&t, &opts(512 * KIB, 4), &client(4), &NoProgress).unwrap();
}
