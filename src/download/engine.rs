//! The download loop: prepare or resume `.part`, fetch pending chunks in parallel with retry and
//! mirror fallback, verify the checksum, then atomically move the file into place.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{FallocateFlags, FlockOperation};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use super::state::{State, to_hex};
use super::{DownloadError, FetchError, Options, Outcome, Progress, RangeSource, Target, io_err};

/// Exponential backoff with "equal jitter": between half and all of `base * 2^attempt`, capped.
pub fn backoff_delay(attempt: u32, base: Duration, cap: Duration, jitter: f64) -> Duration {
    let factor = 1u32.checked_shl(attempt.min(30)).unwrap_or(u32::MAX);
    base.saturating_mul(factor)
        .min(cap)
        .mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
}

fn jitter() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (nanos, std::thread::current().id()).hash(&mut h);
    (h.finish() % 1000) as f64 / 1000.0
}

pub fn download(
    target: &Target,
    opts: &Options,
    src: &dyn RangeSource,
    progress: &dyn Progress,
) -> Result<Outcome, DownloadError> {
    target.validate()?;
    if opts.chunk_size == 0 || opts.jobs == 0 {
        return Err(DownloadError::BadTarget(
            "jobs and chunk size must be greater than 0".into(),
        ));
    }
    fs::create_dir_all(&target.dir).map_err(io_err("cannot create", &target.dir))?;
    let _lock = acquire_lock(&target.lock_path())?;

    let final_path = target.final_path();
    if let Ok(meta) = fs::metadata(&final_path) {
        if meta.is_file() && meta.len() == target.size {
            progress.on_verify();
            if hash_file(&final_path)? == target.sha256 {
                remove_partials(target);
                return Ok(Outcome::AlreadyPresent);
            }
        }
        // Wrong size or checksum: it cannot be trusted, fetch it again.
        fs::remove_file(&final_path).map_err(io_err("cannot remove corrupt file", &final_path))?;
    }

    let (state, part) = prepare(target, opts)?;
    let resumed_bytes = state.done_bytes();
    progress.on_progress(resumed_bytes, target.size);

    let pending: VecDeque<usize> = state.pending().into();
    if !pending.is_empty() {
        let workers = opts.jobs.min(pending.len());
        let ctx = Context {
            target,
            opts,
            src,
            progress,
            file: &part,
            queue: Mutex::new(pending),
            state: Mutex::new(state),
            dead: target.sources.iter().map(|_| AtomicBool::new(false)).collect(),
            preferred: AtomicUsize::new(0),
            abort: AtomicBool::new(false),
            error: Mutex::new(None),
        };
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| ctx.worker());
            }
        });
        if let Some(e) = ctx.error.into_inner().unwrap_or_else(|e| e.into_inner()) {
            return Err(e);
        }
    }

    progress.on_verify();
    let part_path = target.part_path();
    part.sync_all().map_err(io_err("cannot sync", &part_path))?;
    let actual = hash_file(&part_path)?;
    if actual != target.sha256 {
        remove_partials(target);
        return Err(DownloadError::Corrupt {
            expected: target.sha256.clone(),
            actual,
        });
    }
    fs::rename(&part_path, &final_path).map_err(io_err("cannot move into place", &final_path))?;
    if let Ok(dir) = File::open(&target.dir) {
        let _ = dir.sync_all();
    }
    let _ = fs::remove_file(target.state_path());
    Ok(Outcome::Downloaded { resumed_bytes })
}

fn acquire_lock(path: &Path) -> Result<File, DownloadError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io_err("cannot open lock file", path))?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(Errno::WOULDBLOCK) => Err(DownloadError::Locked(path.to_path_buf())),
        Err(e) => Err(DownloadError::Io {
            context: format!("cannot lock {}", path.display()),
            source: e.into(),
        }),
    }
}

fn remove_partials(target: &Target) {
    let _ = fs::remove_file(target.part_path());
    let _ = fs::remove_file(target.state_path());
}

/// Resumes a trusted partial download or starts a fresh, fully allocated one.
fn prepare(target: &Target, opts: &Options) -> Result<(State, File), DownloadError> {
    let part_path = target.part_path();
    let state_path = target.state_path();
    let part_ok = fs::metadata(&part_path).is_ok_and(|m| m.is_file() && m.len() == target.size);
    let loaded = State::load(&state_path).filter(|s| s.matches(&target.name, target.size, &target.sha256));
    if let (Some(state), true) = (loaded, part_ok) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&part_path)
            .map_err(io_err("cannot open", &part_path))?;
        return Ok((state, file));
    }

    remove_partials(target);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&part_path)
        .map_err(io_err("cannot create", &part_path))?;
    // Reserve the space now so a full disk is reported before hours of downloading, not after.
    match rustix::fs::fallocate(&file, FallocateFlags::empty(), 0, target.size) {
        Ok(()) => {}
        Err(Errno::NOSPC | Errno::FBIG) => {
            let _ = fs::remove_file(&part_path);
            return Err(DownloadError::NoSpace(target.dir.clone()));
        }
        // Filesystem without fallocate support: fall back to a sparse file.
        Err(_) => file
            .set_len(target.size)
            .map_err(io_err("cannot size", &part_path))?,
    }
    let state = State::new(&target.name, target.size, &target.sha256, opts.chunk_size);
    state
        .save(&state_path)
        .map_err(io_err("cannot write", &state_path))?;
    Ok((state, file))
}

pub fn hash_file(path: &Path) -> Result<String, DownloadError> {
    let mut file = File::open(path).map_err(io_err("cannot open", path))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(io_err("cannot read", path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(to_hex(&hasher.finalize()))
}

struct Context<'a> {
    target: &'a Target,
    opts: &'a Options,
    src: &'a dyn RangeSource,
    progress: &'a dyn Progress,
    file: &'a File,
    queue: Mutex<VecDeque<usize>>,
    state: Mutex<State>,
    /// Sources that proved unable to serve this file during this run.
    dead: Vec<AtomicBool>,
    /// Source that worked last; new chunks start there.
    preferred: AtomicUsize,
    abort: AtomicBool,
    error: Mutex<Option<DownloadError>>,
}

impl Context<'_> {
    fn worker(&self) {
        let mut buf = Vec::new();
        while !self.abort.load(Ordering::SeqCst) {
            let Some(chunk) = self.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() else {
                break;
            };
            if let Err(e) = self.fetch_and_store(chunk, &mut buf) {
                self.fail(e);
                break;
            }
        }
    }

    fn fail(&self, e: DownloadError) {
        self.error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(e);
        self.abort.store(true, Ordering::SeqCst);
    }

    fn fetch_and_store(&self, chunk: usize, buf: &mut Vec<u8>) -> Result<(), DownloadError> {
        let (offset, len) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chunk_range(chunk);
        self.fetch_chunk(offset, len, buf)?;

        let part_path = self.target.part_path();
        self.file
            .write_all_at(buf, offset)
            .and_then(|()| self.file.sync_data())
            .map_err(|e| {
                if e.raw_os_error() == Some(Errno::NOSPC.raw_os_error()) {
                    DownloadError::NoSpace(self.target.dir.clone())
                } else {
                    DownloadError::Io {
                        context: format!("cannot write {}", part_path.display()),
                        source: e,
                    }
                }
            })?;

        // Data is durable; only now record the chunk as done.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.mark_done(chunk);
        state
            .save(&self.target.state_path())
            .map_err(io_err("cannot write", &self.target.state_path()))?;
        self.progress.on_progress(state.done_bytes(), self.target.size);
        Ok(())
    }

    /// Fetches one chunk, retrying with backoff and falling back across sources.
    fn fetch_chunk(&self, offset: u64, len: u64, buf: &mut Vec<u8>) -> Result<(), DownloadError> {
        let sources = &self.target.sources;
        let mut current = self.preferred.load(Ordering::SeqCst) % sources.len();
        let mut attempts = 0u32;
        let mut failures_on_current = 0u32;
        let mut last = String::from("no attempt made");

        loop {
            if self.abort.load(Ordering::SeqCst) {
                return Err(DownloadError::Failed("aborted".into()));
            }
            let Some(idx) = (0..sources.len())
                .map(|i| (current + i) % sources.len())
                .find(|&i| !self.dead[i].load(Ordering::SeqCst))
            else {
                return Err(DownloadError::Failed(format!("no usable source left ({last})")));
            };
            if idx != current {
                current = idx;
                failures_on_current = 0;
            }

            match self
                .src
                .read_range(&sources[idx], offset, len, self.target.size, buf)
            {
                Ok(()) if buf.len() as u64 == len => {
                    self.preferred.store(idx, Ordering::SeqCst);
                    return Ok(());
                }
                Ok(()) => last = format!("{}: short read", sources[idx]),
                Err(FetchError::SourceUnusable(why)) => {
                    self.dead[idx].store(true, Ordering::SeqCst);
                    last = why;
                    continue;
                }
                Err(FetchError::Transient { message, retry_after }) => {
                    attempts += 1;
                    failures_on_current += 1;
                    last = message;
                    if attempts >= self.opts.retries {
                        return Err(DownloadError::Failed(format!(
                            "giving up after {attempts} attempts: {last}. Progress is kept; run the command again to continue"
                        )));
                    }
                    // Two strikes and the next mirror gets a try.
                    if failures_on_current >= 2 && sources.len() > 1 {
                        current = (idx + 1) % sources.len();
                        failures_on_current = 0;
                    }
                    let wait = backoff_delay(
                        attempts - 1,
                        self.opts.backoff_base,
                        self.opts.backoff_cap,
                        jitter(),
                    );
                    self.sleep(retry_after.map_or(wait, |r| r.min(self.opts.backoff_cap).max(wait)));
                }
            }
        }
    }

    /// Sleeps in short slices so an abort elsewhere is noticed quickly.
    fn sleep(&self, total: Duration) {
        let slice = Duration::from_millis(50);
        let mut left = total;
        while !left.is_zero() && !self.abort.load(Ordering::SeqCst) {
            let d = left.min(slice);
            std::thread::sleep(d);
            left -= d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::NoProgress;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;

    const SMALL: Duration = Duration::from_millis(1);

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn sha(bytes: &[u8]) -> String {
        to_hex(&Sha256::digest(bytes))
    }

    type Script = Box<dyn Fn(&str, u64) -> Option<FetchError> + Sync>;

    /// In-memory server. `script` can inject a failure per request (url, request counter).
    struct Fake {
        files: HashMap<String, Vec<u8>>,
        calls: AtomicU64,
        ranges: Mutex<Vec<(String, u64)>>,
        script: Script,
    }

    impl Fake {
        fn new(url: &str, bytes: Vec<u8>) -> Self {
            Self {
                files: HashMap::from([(url.to_string(), bytes)]),
                calls: AtomicU64::new(0),
                ranges: Mutex::new(Vec::new()),
                script: Box::new(|_, _| None),
            }
        }

        fn serve(mut self, url: &str, bytes: Vec<u8>) -> Self {
            self.files.insert(url.into(), bytes);
            self
        }

        fn script(mut self, f: impl Fn(&str, u64) -> Option<FetchError> + Sync + 'static) -> Self {
            self.script = Box::new(f);
            self
        }

        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl RangeSource for Fake {
        fn read_range(
            &self,
            url: &str,
            start: u64,
            len: u64,
            _total: u64,
            buf: &mut Vec<u8>,
        ) -> Result<(), FetchError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.ranges.lock().unwrap().push((url.to_string(), start));
            if let Some(e) = (self.script)(url, n) {
                return Err(e);
            }
            let file = self
                .files
                .get(url)
                .ok_or_else(|| FetchError::SourceUnusable("404".into()))?;
            buf.clear();
            buf.extend_from_slice(&file[start as usize..(start + len) as usize]);
            Ok(())
        }
    }

    fn opts(chunk: u64, jobs: usize) -> Options {
        Options {
            jobs,
            chunk_size: chunk,
            retries: 5,
            backoff_base: SMALL,
            backoff_cap: Duration::from_millis(5),
        }
    }

    fn target(dir: &Path, bytes: &[u8], sources: &[&str]) -> Target {
        Target {
            name: "file.bin".into(),
            dir: dir.into(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            size: bytes.len() as u64,
            sha256: sha(bytes),
        }
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "file.bin.lock")
            .collect();
        v.sort();
        v
    }

    #[test]
    fn downloads_in_parallel_and_leaves_only_the_final_file() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(1000);
        let fake = Fake::new("u", bytes.clone());
        let t = target(d.path(), &bytes, &["u"]);
        let out = download(&t, &opts(64, 4), &fake, &NoProgress).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_bytes: 0 });
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
        assert_eq!(leftovers(d.path()), vec!["file.bin"]);
        assert_eq!(fake.calls(), 16, "1000 bytes in 64-byte chunks");
    }

    #[test]
    fn single_short_chunk_and_exact_multiple() {
        for len in [1usize, 64, 128, 129] {
            let d = tempfile::tempdir().unwrap();
            let bytes = data(len);
            let fake = Fake::new("u", bytes.clone());
            let t = target(d.path(), &bytes, &["u"]);
            download(&t, &opts(64, 3), &fake, &NoProgress).unwrap();
            assert_eq!(fs::read(t.final_path()).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn resumes_and_requests_only_missing_chunks() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(400);
        let t = target(d.path(), &bytes, &["u"]);
        // Earlier run: chunks 0 and 2 (of 100 bytes) were stored, the rest is garbage.
        let mut part = vec![0xEEu8; 400];
        part[..100].copy_from_slice(&bytes[..100]);
        part[200..300].copy_from_slice(&bytes[200..300]);
        fs::write(t.part_path(), &part).unwrap();
        let mut st = State::new(&t.name, t.size, &t.sha256, 100);
        st.mark_done(0);
        st.mark_done(2);
        st.save(&t.state_path()).unwrap();

        let fake = Fake::new("u", bytes.clone());
        let out = download(&t, &opts(100, 2), &fake, &NoProgress).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_bytes: 200 });
        let mut requested: Vec<u64> = fake.ranges.lock().unwrap().iter().map(|r| r.1).collect();
        requested.sort();
        assert_eq!(requested, vec![100, 300]);
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    }

    #[test]
    fn resume_uses_the_chunk_size_from_the_state() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(400);
        let t = target(d.path(), &bytes, &["u"]);
        fs::write(t.part_path(), &bytes).unwrap();
        let mut st = State::new(&t.name, t.size, &t.sha256, 100);
        for i in 0..3 {
            st.mark_done(i);
        }
        st.save(&t.state_path()).unwrap();
        let fake = Fake::new("u", bytes.clone());
        download(&t, &opts(37, 2), &fake, &NoProgress).unwrap();
        assert_eq!(
            fake.ranges
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.1)
                .collect::<Vec<_>>(),
            vec![300]
        );
    }

    #[test]
    fn state_for_a_different_file_is_discarded() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let t = target(d.path(), &bytes, &["u"]);
        fs::write(t.part_path(), vec![0xEE; 300]).unwrap();
        let mut st = State::new(&t.name, t.size, &sha(b"some older version"), 100);
        (0..3).for_each(|i| st.mark_done(i));
        st.save(&t.state_path()).unwrap();
        let fake = Fake::new("u", bytes.clone());
        let out = download(&t, &opts(100, 2), &fake, &NoProgress).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_bytes: 0 });
        assert_eq!(fake.calls(), 3, "everything is fetched again");
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    }

    #[test]
    fn part_file_with_wrong_length_is_not_trusted() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let t = target(d.path(), &bytes, &["u"]);
        fs::write(t.part_path(), vec![0u8; 10]).unwrap();
        let mut st = State::new(&t.name, t.size, &t.sha256, 100);
        (0..3).for_each(|i| st.mark_done(i));
        st.save(&t.state_path()).unwrap();
        let fake = Fake::new("u", bytes.clone());
        download(&t, &opts(100, 1), &fake, &NoProgress).unwrap();
        assert_eq!(fake.calls(), 3);
    }

    #[test]
    fn corrupted_data_is_detected_and_nothing_is_installed() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let mut bad = bytes.clone();
        bad[150] ^= 0xFF;
        let fake = Fake::new("u", bad);
        let t = target(d.path(), &bytes, &["u"]);
        match download(&t, &opts(100, 2), &fake, &NoProgress) {
            Err(DownloadError::Corrupt { expected, actual }) => {
                assert_eq!(expected, t.sha256);
                assert_ne!(actual, expected);
            }
            other => panic!("{other:?}"),
        }
        assert!(leftovers(d.path()).is_empty(), "{:?}", leftovers(d.path()));
    }

    #[test]
    fn transient_errors_are_retried() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let fake =
            Fake::new("u", bytes.clone()).script(|_, n| (n < 3).then(|| FetchError::transient("reset")));
        let t = target(d.path(), &bytes, &["u"]);
        download(&t, &opts(100, 1), &fake, &NoProgress).unwrap();
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
        assert_eq!(fake.calls(), 6);
    }

    #[test]
    fn giving_up_keeps_progress_for_the_next_run() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let t = target(d.path(), &bytes, &["u"]);
        // First two calls succeed (chunks 0 and 1, sequentially), then the network is gone.
        let down =
            Fake::new("u", bytes.clone()).script(|_, n| (n >= 2).then(|| FetchError::transient("no route")));
        match download(&t, &opts(100, 1), &down, &NoProgress) {
            Err(DownloadError::Failed(m)) => assert!(
                m.contains("no route") && m.contains("run the command again"),
                "{m}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(!t.final_path().exists());
        assert!(t.part_path().exists());
        assert_eq!(State::load(&t.state_path()).unwrap().done_bytes(), 200);

        // Network is back: only the missing chunk is fetched.
        let up = Fake::new("u", bytes.clone());
        let out = download(&t, &opts(100, 1), &up, &NoProgress).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_bytes: 200 });
        assert_eq!(up.calls(), 1);
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    }

    #[test]
    fn unusable_primary_falls_back_to_mirror() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let fake = Fake::new("mirror", bytes.clone()); // "primary" has no file -> 404
        let t = target(d.path(), &bytes, &["primary", "mirror"]);
        download(&t, &opts(100, 2), &fake, &NoProgress).unwrap();
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
        let to_primary = fake
            .ranges
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.0 == "primary")
            .count();
        assert!(
            to_primary <= 2,
            "dead source is tried at most once per worker, got {to_primary}"
        );
    }

    #[test]
    fn flaky_primary_switches_to_mirror_after_two_failures() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(500);
        let fake = Fake::new("primary", bytes.clone())
            .serve("mirror", bytes.clone())
            .script(|u, _| (u == "primary").then(|| FetchError::transient("timeout")));
        let t = target(d.path(), &bytes, &["primary", "mirror"]);
        download(&t, &opts(100, 1), &fake, &NoProgress).unwrap();
        let ranges = fake.ranges.lock().unwrap();
        let primary = ranges.iter().filter(|r| r.0 == "primary").count();
        assert_eq!(
            primary, 2,
            "only the first chunk probes the broken primary; later chunks start at the mirror"
        );
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    }

    #[test]
    fn all_sources_unusable_is_a_clear_error() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(100);
        let fake = Fake::new("elsewhere", bytes.clone());
        let t = target(d.path(), &bytes, &["a", "b"]);
        match download(&t, &opts(100, 1), &fake, &NoProgress) {
            Err(DownloadError::Failed(m)) => assert!(m.contains("no usable source"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mixing_chunks_from_mirrors_is_fine_because_the_checksum_decides() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(400);
        let mut evil = bytes.clone();
        evil[250] ^= 1;
        let fake = Fake::new("a", bytes.clone()).serve("b", evil);
        let t = target(d.path(), &bytes, &["a", "b"]);
        // Only "a" is ever used here, so this must succeed...
        download(&t, &opts(100, 1), &fake, &NoProgress).unwrap();
        // ...while a mirror serving different bytes is caught.
        let d2 = tempfile::tempdir().unwrap();
        let t2 = target(d2.path(), &bytes, &["b"]);
        assert!(matches!(
            download(&t2, &opts(100, 1), &fake, &NoProgress),
            Err(DownloadError::Corrupt { .. })
        ));
    }

    #[test]
    fn existing_verified_file_is_not_downloaded_again() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let fake = Fake::new("u", bytes.clone());
        let t = target(d.path(), &bytes, &["u"]);
        download(&t, &opts(100, 2), &fake, &NoProgress).unwrap();
        let before = fake.calls();
        assert_eq!(
            download(&t, &opts(100, 2), &fake, &NoProgress).unwrap(),
            Outcome::AlreadyPresent
        );
        assert_eq!(fake.calls(), before);
    }

    #[test]
    fn existing_corrupt_file_is_replaced() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let t = target(d.path(), &bytes, &["u"]);
        let mut bad = bytes.clone();
        bad[0] ^= 1;
        fs::write(t.final_path(), &bad).unwrap();
        let fake = Fake::new("u", bytes.clone());
        assert!(matches!(
            download(&t, &opts(100, 2), &fake, &NoProgress).unwrap(),
            Outcome::Downloaded { .. }
        ));
        assert_eq!(fs::read(t.final_path()).unwrap(), bytes);
    }

    #[test]
    fn crash_between_last_chunk_and_rename_completes_without_fetching() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(300);
        let t = target(d.path(), &bytes, &["u"]);
        fs::write(t.part_path(), &bytes).unwrap();
        let mut st = State::new(&t.name, t.size, &t.sha256, 100);
        (0..3).for_each(|i| st.mark_done(i));
        st.save(&t.state_path()).unwrap();
        let fake = Fake::new("u", bytes.clone());
        let out = download(&t, &opts(100, 2), &fake, &NoProgress).unwrap();
        assert_eq!(out, Outcome::Downloaded { resumed_bytes: 300 });
        assert_eq!(fake.calls(), 0);
        assert_eq!(leftovers(d.path()), vec!["file.bin"]);
    }

    #[test]
    fn second_process_is_locked_out() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(100);
        let t = target(d.path(), &bytes, &["u"]);
        let _held = acquire_lock(&t.lock_path()).unwrap();
        let fake = Fake::new("u", bytes);
        assert!(matches!(
            download(&t, &opts(100, 1), &fake, &NoProgress),
            Err(DownloadError::Locked(_))
        ));
    }

    #[test]
    fn invalid_options_and_targets_are_rejected() {
        let d = tempfile::tempdir().unwrap();
        let bytes = data(10);
        let fake = Fake::new("u", bytes.clone());
        let t = target(d.path(), &bytes, &["u"]);
        assert!(download(&t, &opts(0, 1), &fake, &NoProgress).is_err());
        assert!(download(&t, &opts(10, 0), &fake, &NoProgress).is_err());
        let mut t = t;
        t.sources.clear();
        assert!(matches!(
            download(&t, &opts(10, 1), &fake, &NoProgress),
            Err(DownloadError::BadTarget(_))
        ));
    }

    #[test]
    fn progress_is_reported_up_to_the_total() {
        struct Rec(Mutex<Vec<u64>>, AtomicBool);
        impl Progress for Rec {
            fn on_progress(&self, done: u64, _total: u64) {
                self.0.lock().unwrap().push(done);
            }
            fn on_verify(&self) {
                self.1.store(true, Ordering::SeqCst);
            }
        }
        let d = tempfile::tempdir().unwrap();
        let bytes = data(250);
        let fake = Fake::new("u", bytes.clone());
        let rec = Rec(Mutex::new(vec![]), AtomicBool::new(false));
        download(&target(d.path(), &bytes, &["u"]), &opts(100, 1), &fake, &rec).unwrap();
        assert_eq!(*rec.0.lock().unwrap(), vec![0, 100, 200, 250]);
        assert!(rec.1.load(Ordering::SeqCst));
    }

    #[test]
    fn backoff_grows_is_capped_and_jittered() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_delay(0, base, cap, 1.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, base, cap, 1.0), Duration::from_secs(8));
        assert_eq!(backoff_delay(10, base, cap, 1.0), cap);
        assert_eq!(backoff_delay(99, base, cap, 1.0), cap, "no overflow");
        assert_eq!(
            backoff_delay(3, base, cap, 0.0),
            Duration::from_secs(4),
            "jitter halves at most"
        );
        assert_eq!(
            backoff_delay(3, base, cap, 7.0),
            Duration::from_secs(8),
            "jitter is clamped"
        );
    }
}
