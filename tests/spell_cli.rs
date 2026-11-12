//! `kit spell` against a real (tiny) ZIM in a temp data dir: estimate, enable at a level, list,
//! the files the portal serves, disable. Starts no service and touches nothing outside the dir.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use emergency_kit::catalog::PackageType;
use emergency_kit::install::Paths;
use emergency_kit::install::state::{FileRecord, Record, StateGuard};

fn kit(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kit"))
        .arg("spell")
        .args(args)
        .arg("--data-dir")
        .arg(data)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn install_zim(data: &Path, id: &str, category: Option<&str>, pages: &[(&str, &str, &str)]) {
    let paths = Paths::new(data.to_path_buf(), data.join("units")).unwrap();
    std::fs::create_dir_all(paths.library_dir()).unwrap();
    let file = format!("{id}_2026-10.zim");
    common::zim::write_zim(&paths.library_dir().join(&file), pages);
    let mut guard = StateGuard::open(&paths).unwrap();
    guard.packages.insert(
        id.into(),
        Record {
            id: id.into(),
            name: format!("Name of {id}"),
            kind: PackageType::Zim,
            version: None,
            license: "CC0-1.0".into(),
            installed_at: 0,
            category: category.map(String::from),
            files: vec![FileRecord {
                name: file,
                size: 1,
                sha256: String::new(),
                source: None,
                moved: false,
            }],
            service: None,
            app: None,
            model: None,
        },
    );
    guard.save().unwrap();
}

#[test]
fn spelling_index_per_zim_with_levels_and_estimates() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();
    install_zim(
        data,
        "ready_en",
        None,
        &[
            (
                "a",
                "Earthquake safety",
                "<p>Drop, cover and hold on during an earthquake.</p>",
            ),
            ("b", "Tornado watch", "<p>Go to the lowest floor.</p>"),
        ],
    );
    install_zim(data, "map_lu", Some("maps"), &[("a", "Luxembourg", "<p>x</p>")]);

    // Everything is off at first; maps are not offered (they have their own place search).
    let out = kit(data, &["list"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let listing = text(&out.stdout);
    assert!(
        listing.contains("ready_en") && listing.contains("off"),
        "{listing}"
    );
    assert!(!listing.contains("map_lu"), "{listing}");

    let out = kit(data, &["estimate", "ready_en"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let est = text(&out.stdout);
    for level in ["titles", "sample", "full"] {
        assert!(est.contains(level), "{est}");
    }
    assert!(est.contains("DISK") && est.contains("TIME"), "{est}");
    let only = text(&kit(data, &["estimate", "ready_en", "--level", "full"]).stdout);
    assert!(only.contains("full") && !only.contains("sample"), "{only}");

    // Enabling builds the index and the portal serves it.
    let out = kit(data, &["enable", "ready_en", "--level", "2"]);
    assert!(out.status.success(), "{}{}", text(&out.stdout), text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("level sample") && said.contains("words indexed"),
        "{said}"
    );
    let index = data.join("spell/ready_en_2026-10");
    assert!(index.join("meta.json").exists() && index.join("w10.tsv").exists());
    let books = std::fs::read_to_string(data.join("portal/spell/books.json")).unwrap();
    assert!(
        books.contains("ready_en_2026-10") && books.contains("sample"),
        "{books}"
    );
    assert!(
        data.join("portal/spell/ready_en_2026-10/w10.tsv").exists(),
        "linked into the portal"
    );

    let out = kit(data, &["list", "--json"]);
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rows[0]["id"], "ready_en");
    assert_eq!(
        (rows[0]["level"].as_str(), rows[0]["current"].as_bool()),
        (Some("sample"), Some(true))
    );
    assert_eq!(rows[0]["estimates"].as_array().unwrap().len(), 3);
    assert!(rows[0]["estimates"][2]["secs"].is_u64() && rows[0]["estimates"][2]["bytes"].is_u64());
    assert!(rows[0]["index"]["words"].as_u64().unwrap() >= 4);

    // Running it again is a no-op; a higher level rebuilds.
    let again = text(&kit(data, &["index"]).stdout);
    assert!(!again.contains("building"), "current index is kept: {again}");
    let out = kit(data, &["enable", "ready_en", "--level", "full"]);
    assert!(
        text(&out.stdout).contains("building the spelling index (full"),
        "{}",
        text(&out.stdout)
    );
    assert!(text(&kit(data, &["list"]).stdout).contains("full"));

    // Choosing without building leaves the old index marked outdated.
    kit(data, &["enable", "ready_en", "--level", "titles", "--no-build"]);
    assert!(text(&kit(data, &["list"]).stdout).contains("outdated"));

    // Mistakes are clear errors.
    for bad in [
        &["enable", "nope", "--level", "full"][..],
        &["enable", "map_lu"][..],
    ] {
        let out = kit(data, bad);
        assert_eq!(out.status.code(), Some(1));
        assert!(
            text(&out.stderr).contains("is not an installed ZIM package"),
            "{}",
            text(&out.stderr)
        );
    }

    // Disabling deletes the index, the setting and the portal files.
    let out = kit(data, &["disable", "ready_en"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!index.exists() && !data.join("portal/spell").exists());
    let rows: serde_json::Value = serde_json::from_slice(&kit(data, &["list", "--json"]).stdout).unwrap();
    assert!(rows[0]["level"].is_null() && rows[0]["index"].is_null());
}
