//! The download manager's queue against a fake `kit` (a shell script), so nothing is installed.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use emergency_kit::catalog::PackageType;
use emergency_kit::ui::backend::{ArchiveRequest, Config, Event, JobState, Runner};
use emergency_kit::ui::model::Pkg;

/// Writing a script and running it while another test forks fails with ETXTBSY; run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls")).map_or(0, |s| s.lines().count())
}

fn pkg(id: &str) -> Pkg {
    Pkg {
        id: id.into(),
        name: format!("Package {id}"),
        desc: String::new(),
        category: "reference".into(),
        kind: PackageType::Zim,
        bytes: 1000,
        license: "CC0".into(),
        langs: Vec::new(),
        needs: Vec::new(),
        latest: None,
        file: None,
        sha256: None,
        host: None,
    }
}

/// A `kit` that logs its arguments and then behaves as `body` says.
fn fake_kit(dir: &Path, body: &str) -> Config {
    let script = dir.join("kit");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$@\" >> '{}/calls'\n{body}\n", dir.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    Config {
        catalog_dir: dir.join("catalog"),
        data_dir: dir.join("data"),
        cache_dir: dir.join("cache"),
        kit_bin: script,
    }
}

fn poll_until(
    r: &mut Runner,
    online: bool,
    mut done: impl FnMut(&mut Runner, &[Event]) -> bool,
) -> Vec<Event> {
    let mut all = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        let ev = r.poll(online);
        all.extend(ev.iter().cloned());
        if done(r, &ev) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out; events so far: {all:?}");
}

#[test]
fn a_finished_install_leaves_the_queue_and_installs_one_package_at_a_time() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "exit 0"));
    r.enqueue(&pkg("a"), false);
    r.enqueue(&pkg("b"), true);
    let events = poll_until(&mut r, true, |r, _| r.jobs.is_empty());
    let done: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Done { id, update, .. } => Some((id.as_str(), *update)),
            _ => None,
        })
        .collect();
    assert_eq!(done, [("a", false), ("b", true)]);
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("install --select a --data-dir"), "{}", lines[0]);
    assert!(lines[0].contains("--catalog") && !lines[0].contains("--offline"));
}

#[test]
fn a_failing_install_stays_in_the_queue_with_the_last_error_line() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(
        d.path(),
        "printf 'fetching\\rerror: disk is full\\n' >&2; exit 1",
    ));
    r.enqueue(&pkg("a"), false);
    let events = poll_until(&mut r, true, |_, ev| {
        ev.iter().any(|e| matches!(e, Event::Failed { .. }))
    });
    assert!(events.contains(&Event::Failed {
        id: "a".into(),
        message: "disk is full".into()
    }));
    assert_eq!(r.jobs[0].state, JobState::Failed("disk is full".into()));
    // Resuming retries it.
    r.toggle("a");
    assert_eq!(r.jobs[0].state, JobState::Queued);
}

#[test]
fn pause_stops_the_child_and_resume_starts_it_again() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "sleep 30"));
    r.enqueue(&pkg("a"), false);
    poll_until(&mut r, true, |r, _| {
        r.jobs[0].state == JobState::Running && calls(d.path()) == 1
    });
    r.toggle("a");
    assert_eq!(r.jobs[0].state, JobState::Paused);
    assert!(r.all_paused());
    // Paused jobs are not started again by polling.
    for _ in 0..5 {
        r.poll(true);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(calls(d.path()), 1);
    r.resume_all();
    poll_until(&mut r, true, |r, _| {
        r.jobs[0].state == JobState::Running && calls(d.path()) == 2
    });
    assert_eq!(calls(d.path()), 2);
}

#[test]
fn nothing_starts_offline_and_going_offline_stops_the_running_job() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "sleep 30"));
    r.enqueue(&pkg("a"), false);
    for _ in 0..5 {
        r.poll(false);
    }
    assert_eq!(r.jobs[0].state, JobState::Queued);
    assert!(!d.path().join("calls").exists());
    poll_until(&mut r, true, |r, _| r.jobs[0].state == JobState::Running);
    r.poll(false);
    assert_eq!(
        r.jobs[0].state,
        JobState::Queued,
        "the connection dropped; it waits"
    );
}

#[test]
fn removing_runs_uninstall_offline_and_reports_it() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "exit 0"));
    r.remove(vec!["a".into(), "b".into()]);
    let events = poll_until(&mut r, true, |_, ev| {
        ev.iter().any(|e| matches!(e, Event::Removed(_)))
    });
    assert!(events.contains(&Event::Removed(vec!["a".into(), "b".into()])));
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    assert!(
        calls.contains("--offline") && calls.contains("uninstall a b --data-dir"),
        "{calls}"
    );
}

#[test]
fn progress_is_read_from_the_package_folder_of_the_downloads_dir() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let cfg = fake_kit(d.path(), "sleep 30");
    let folder = cfg.data_dir.join("downloads/a");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("a.zim"), vec![0u8; 1000]).unwrap();
    let mut r = Runner::new(cfg);
    let mut p = pkg("a");
    p.file = Some("a.zim".into());
    p.sha256 = Some("0".repeat(64));
    r.enqueue(&p, false);
    poll_until(&mut r, true, |r, _| r.jobs[0].state == JobState::Running);
    r.poll(true);
    assert_eq!(r.jobs[0].got, 1000, "a finished file counts as fully downloaded");
}

#[test]
fn cancel_stops_the_running_child_and_the_next_job_takes_over() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "sleep 30"));
    r.enqueue(&pkg("a"), false);
    r.enqueue(&pkg("b"), false);
    poll_until(&mut r, true, |r, _| {
        r.jobs[0].state == JobState::Running && calls(d.path()) == 1
    });
    r.cancel("a");
    assert_eq!(r.jobs.len(), 1);
    assert_eq!(r.jobs[0].id, "b");
    poll_until(&mut r, true, |r, _| {
        r.jobs[0].state == JobState::Running && calls(d.path()) == 2
    });
    r.cancel("nope");
    assert_eq!(r.jobs.len(), 1);
}

#[test]
fn a_move_runs_data_dir_set_and_later_installs_use_the_new_place() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "exit 0"));
    let new = d.path().join("disk/kit");
    assert!(r.can_relocate());
    r.relocate(new.clone(), true);
    assert!(r.is_relocating() && !r.can_relocate());
    // A queued install waits for the move instead of racing it.
    r.enqueue(&pkg("a"), false);
    let events = poll_until(&mut r, true, |_, ev| {
        ev.iter().any(|e| matches!(e, Event::Relocated(_)))
    });
    assert!(events.contains(&Event::Relocated(new.clone())));
    poll_until(&mut r, true, |r, _| r.jobs.is_empty());
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert!(
        lines[0].contains(&format!("data-dir set {} --move --data-dir", new.display())),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!("install --select a --data-dir {}", new.display())),
        "the install started after the move, in the new place: {}",
        lines[1]
    );
}

#[test]
fn a_failed_move_reports_the_reason_and_keeps_the_old_place() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(
        d.path(),
        "case \"$*\" in *\"data-dir set\"*) echo 'error: /x: exists and is not empty' >&2; exit 1;; esac",
    ));
    r.relocate(d.path().join("elsewhere"), false);
    let events = poll_until(&mut r, true, |_, ev| {
        ev.iter().any(|e| matches!(e, Event::RelocateFailed(_)))
    });
    assert!(events.contains(&Event::RelocateFailed("/x: exists and is not empty".into())));
    r.enqueue(&pkg("a"), false);
    poll_until(&mut r, true, |r, _| r.jobs.is_empty());
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    assert!(
        calls
            .lines()
            .last()
            .unwrap()
            .contains(&format!("--data-dir {}", d.path().join("data").display()))
    );
    assert!(
        !calls.lines().next().unwrap().contains("--move"),
        "no --move unless asked"
    );
}

#[test]
fn typed_locations_are_checked_like_kit_data_dir_set() {
    use emergency_kit::ui::backend::check_location;
    let cur = Path::new("/home/u/.local/share/kit");
    assert!(check_location(cur, "  ").is_err());
    assert!(check_location(cur, "disk/kit").unwrap_err().contains("full path"));
    assert!(check_location(cur, "/run/media/u/My Disk/kit").is_err());
    assert!(check_location(cur, "/home/u/.local/share/kit/sub").is_err());
    assert_eq!(
        check_location(cur, " /run/media/u/disk/kit ").unwrap(),
        Path::new("/run/media/u/disk/kit")
    );
}

fn request() -> ArchiveRequest {
    ArchiveRequest {
        url: " https://projekt-gutenberg.org/bibliothek/ ".into(),
        id: "gutenberg-de".into(),
        name: "Projekt Gutenberg-DE".into(),
        license: "personal copy".into(),
        page_limit: "50".into(),
    }
}

#[test]
fn an_archive_request_is_checked_like_the_cli_and_suggests_an_id() {
    assert!(request().check().is_ok());
    for bad in [
        ArchiveRequest {
            license: " ".into(),
            ..request()
        },
        ArchiveRequest {
            id: "Bad Id".into(),
            ..request()
        },
        ArchiveRequest {
            url: "ftp://x".into(),
            ..request()
        },
        ArchiveRequest {
            page_limit: "abc".into(),
            ..request()
        },
        ArchiveRequest {
            page_limit: "0".into(),
            ..request()
        },
    ] {
        assert!(bad.check().is_err(), "{bad:?}");
    }
    assert!(
        ArchiveRequest {
            page_limit: String::new(),
            ..request()
        }
        .check()
        .is_ok()
    );
    assert_eq!(
        ArchiveRequest::suggested_id("https://www.Projekt-Gutenberg.org/bibliothek/"),
        "projekt-gutenberg-org"
    );
    assert_eq!(ArchiveRequest::suggested_id(""), "");
}

#[test]
fn an_archive_job_runs_kit_archive_shows_its_output_and_leaves_the_queue_when_done() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "echo 'crawled 3 of 50 pages'; sleep 1"));
    r.enqueue_archive(&request());
    assert!(r.jobs[0].archive && r.jobs[0].bytes == 0);
    // Seen while running: the crawler's last line.
    let end = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < end && !seen.contains("crawled 3") {
        r.poll(true);
        seen = r.status_of("gutenberg-de");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(seen, "crawled 3 of 50 pages");
    let events = poll_until(&mut r, true, |r, _| r.jobs.is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Done { id, .. } if id == "gutenberg-de"))
    );
    let calls = std::fs::read_to_string(d.path().join("calls")).unwrap();
    assert!(
        calls.contains(
            "archive https://projekt-gutenberg.org/bibliothek/ --id gutenberg-de --name Projekt Gutenberg-DE \
             --license personal copy --page-limit 50 --data-dir"
        ),
        "{calls}"
    );
}

#[test]
fn a_failed_crawl_stays_in_the_queue_with_the_reason() {
    let _serial = serial();
    let d = tempfile::tempdir().unwrap();
    let mut r = Runner::new(fake_kit(d.path(), "echo 'error: the crawl failed' >&2; exit 1"));
    r.enqueue_archive(&request());
    poll_until(&mut r, true, |_, ev| {
        ev.iter().any(|e| matches!(e, Event::Failed { .. }))
    });
    assert_eq!(r.jobs[0].state, JobState::Failed("the crawl failed".into()));
}
