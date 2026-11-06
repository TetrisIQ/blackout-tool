//! The real `kit` binary against a localhost server that serves catalog archives: add, list,
//! priority, update and the checksum refusal. Config and data live in temp dirs.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::Arc;
use std::thread;

use emergency_kit::catalog::remote::sha256_hex;

fn files_yaml(id: &str, name: &str, deps: &str) -> String {
    format!(
        "id: {id}\nname: {name}\ndescription: d\ncategory: games\ntype: files\n\
         source: {{url: \"https://example.org/{id}.zip\"}}\nsha256: \"{}\"\nsize_bytes: 10\n\
         license: {{spdx: MIT}}\ndepends_on: [{deps}]\n",
        "a".repeat(64)
    )
}

fn targz(files: &[(&str, String)]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (path, content) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, path, content.as_bytes()).unwrap();
    }
    b.into_inner().unwrap().finish().unwrap()
}

/// Serves `(path, body)` pairs forever on a free port; returns the port.
fn serve(routes: Vec<(String, Vec<u8>)>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(routes);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let routes = routes.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => {
                        let _ = write!(
                            s,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = s.write_all(body);
                    }
                    None => {
                        let _ = s.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
            });
        }
    });
    port
}

fn route(path: &str, archive: &[u8]) -> [(String, Vec<u8>); 2] {
    [
        (path.to_string(), archive.to_vec()),
        (
            format!("{path}.sha256"),
            format!("{}  catalog.tar.gz\n", sha256_hex(archive)).into_bytes(),
        ),
    ]
}

struct Home {
    config: tempfile::TempDir,
    data: tempfile::TempDir,
    cache: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            config: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
            cache: tempfile::tempdir().unwrap(),
        }
    }

    fn kit(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kit"))
            .args(args)
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("KIT_DATA_DIR", self.data.path())
            .env("KIT_CACHE_DIR", self.cache.path())
            .env_remove("KIT_CATALOG")
            .output()
            .unwrap()
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn two_catalogs_are_added_ordered_updated_and_removed() {
    let a = targz(&[
        ("catalog.yaml", "name: a\nformat: 1\n".into()),
        ("games/one.yaml", files_yaml("one", "One from A", "")),
    ]);
    let b = targz(&[
        ("catalog.yaml", "name: b\nformat: 1\n".into()),
        ("games/one.yaml", files_yaml("one", "One from B", "")),
        ("games/two.yaml", files_yaml("two", "Two", "one")),
    ]);
    let routes: Vec<_> = route("/a.tar.gz", &a)
        .into_iter()
        .chain(route("/b.tar.gz", &b))
        .collect();
    let port = serve(routes);
    let url = |n: &str| format!("http://127.0.0.1:{port}/{n}.tar.gz");
    let home = Home::new();

    let o = home.kit(&["catalog", "add", "a", &url("a")]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    let o = home.kit(&["catalog", "add", "b", &url("b")]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));

    // the default catalog was kept in front, so the list has three entries
    let o = home.kit(&["catalog", "list"]);
    let listing = text(&o.stdout);
    assert!(
        listing.contains("main") && listing.contains("not downloaded"),
        "{listing}"
    );
    assert!(listing.contains(" a ") && listing.contains(" b "), "{listing}");

    // main is not reachable here; take it out of the way
    assert_eq!(home.kit(&["catalog", "disable", "main"]).status.code(), Some(0));

    let o = home.kit(&["--offline", "show", "one"]);
    let shown = text(&o.stdout);
    assert!(
        shown.contains("One from A") && shown.contains("catalog:    a"),
        "{shown} // {}",
        text(&o.stderr)
    );
    assert!(text(&o.stderr).contains("hidden by \"a\""), "{}", text(&o.stderr));

    let o = home.kit(&["--offline", "list"]);
    assert!(text(&o.stdout).contains("CATALOG"), "{}", text(&o.stdout));

    assert_eq!(home.kit(&["catalog", "move", "b", "1"]).status.code(), Some(0));
    let shown = text(&home.kit(&["--offline", "show", "one"]).stdout);
    assert!(
        shown.contains("One from B") && shown.contains("catalog:    b"),
        "{shown}"
    );

    let o = home.kit(&["catalog", "update", "a"]);
    assert!(text(&o.stdout).contains("up to date"), "{}", text(&o.stdout));
    // The pages are rewritten from the state; with nothing installed there is no portal.
    assert!(
        text(&o.stdout).contains("portal pages refreshed"),
        "{}",
        text(&o.stdout)
    );
    assert!(!home.data.path().join("portal").exists());

    assert_eq!(home.kit(&["catalog", "remove", "b"]).status.code(), Some(0));
    assert!(!home.data.path().join("catalogs/b").exists());
    let shown = text(&home.kit(&["--offline", "show", "one"]).stdout);
    assert!(shown.contains("One from A"), "{shown}");
}

#[test]
fn a_wrong_checksum_adds_nothing() {
    let a = targz(&[
        ("catalog.yaml", "name: a\nformat: 1\n".into()),
        ("games/one.yaml", files_yaml("one", "One", "")),
    ]);
    let port = serve(route("/a.tar.gz", &a).into());
    let home = Home::new();
    let o = home.kit(&[
        "catalog",
        "add",
        "a",
        &format!("http://127.0.0.1:{port}/a.tar.gz"),
        "--sha256",
        &"0".repeat(64),
    ]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        text(&o.stderr).contains("checksum mismatch"),
        "{}",
        text(&o.stderr)
    );
    assert!(!home.config.path().join("kit/catalogs.json").exists());
    assert!(!home.data.path().join("catalogs/a").exists());
}

#[test]
fn check_validates_a_catalog_directory() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("games")).unwrap();
    std::fs::write(d.path().join("games/one.yaml"), files_yaml("one", "One", "")).unwrap();
    let home = Home::new();
    let o = home.kit(&["catalog", "check", d.path().to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("ok: 1 packages"), "{}", text(&o.stdout));
    std::fs::write(d.path().join("games/bad.yaml"), "id: bad\n").unwrap();
    assert_eq!(
        home.kit(&["catalog", "check", d.path().to_str().unwrap()])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn a_local_folder_is_added_read_in_place_and_removed_without_touching_it() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("games")).unwrap();
    std::fs::write(d.path().join("games/one.yaml"), files_yaml("one", "One", "")).unwrap();
    let dir = d.path().to_str().unwrap();
    let home = Home::new();

    let o = home.kit(&["catalog", "add", "dev", dir]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("local folder") && text(&o.stdout).contains("1 packages"));
    let list = text(&home.kit(&["catalog", "list"]).stdout);
    assert!(
        list.contains("dev") && list.contains("ok") && list.contains(dir),
        "{list}"
    );

    // Edits show up without any update, and `update` has nothing to fetch.
    std::fs::write(d.path().join("games/two.yaml"), files_yaml("two", "Two", "one")).unwrap();
    let o = home.kit(&["catalog", "update", "dev"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("read in place"));
    let json = text(&home.kit(&["catalog", "list", "--json"]).stdout);
    assert!(json.contains("\"packages\": 2"), "{json}");
    assert!(!home.data.path().join("catalogs/dev").exists());

    // Not a folder, a pin, and a broken catalog are refused and nothing is saved.
    let o = home.kit(&["catalog", "add", "gone", "/nonexistent/katalog"]);
    assert_eq!(o.status.code(), Some(1));
    let o = home.kit(&["catalog", "add", "pinned", dir, "--sha256", &"a".repeat(64)]);
    assert!(text(&o.stderr).contains("--sha256"), "{}", text(&o.stderr));
    std::fs::write(d.path().join("games/bad.yaml"), "id: bad\n").unwrap();
    assert_eq!(
        home.kit(&["catalog", "add", "broken", dir]).status.code(),
        Some(1)
    );
    let list = text(&home.kit(&["catalog", "list"]).stdout);
    assert!(
        !list.contains("gone") && !list.contains("pinned") && !list.contains("broken"),
        "{list}"
    );

    // Removing the entry never deletes the folder.
    std::fs::remove_file(d.path().join("games/bad.yaml")).unwrap();
    let o = home.kit(&["catalog", "remove", "dev"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(d.path().join("games/one.yaml").exists());
}
