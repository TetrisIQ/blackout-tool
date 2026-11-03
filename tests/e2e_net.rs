//! End to end with real `kiwix-serve` and `caddy` (opt-in). Nothing on the host changes: no hosts
//! file, no certificate store, no nftables, no systemd. Both servers run as child processes on
//! random loopback ports, and the client resolves kit.local to Caddy itself.
//!
//!   KIT_E2E_ZIM=/path/to/small.zim cargo test --test e2e_net -- --ignored --nocapture
//!
//! Needs `kiwix-manage`, `kiwix-serve` and `caddy` on PATH.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use emergency_kit::netcfg::ca::Ca;
use emergency_kit::netcfg::caddy::{CaddySpec, render_caddyfile};
use emergency_kit::netcfg::check::{Fetch, PinnedFetch, check};

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

#[test]
#[ignore = "needs kiwix-tools, caddy and KIT_E2E_ZIM=<small .zim file>"]
fn real_caddy_serves_the_zim_under_its_domain_with_the_local_ca() {
    let zim = std::env::var_os("KIT_E2E_ZIM").expect("set KIT_E2E_ZIM to a small .zim file");
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let book = Path::new(&zim).file_stem().unwrap().to_str().unwrap().to_string();

    // kiwix-serve with a library holding the ZIM.
    let lib = root.join("library.xml");
    let ok = Command::new("kiwix-manage")
        .arg(&lib)
        .arg("add")
        .arg(&zim)
        .status()
        .unwrap();
    assert!(ok.success());
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

    // CA, leaf and Caddyfile exactly as `kit net enable` makes them.
    let ca_dir = root.join("ca");
    let hosts = vec!["kit.local".to_string()];
    let portal = root.join("portal");
    fs::create_dir_all(&portal).unwrap();
    fs::write(
        portal.join("kit.html"),
        "<html><body>PORTAL-MARKER <a href=\"/search?pattern=a\">s</a></body></html>",
    )
    .unwrap();
    let ca = Ca::create(&ca_dir, &hosts, NOW).unwrap();
    ca.ensure_leaf(&hosts, NOW).unwrap();
    let listen_port = free_port();
    let kiwix_book = book.clone();
    let caddyfile = root.join("Caddyfile");
    fs::write(
        &caddyfile,
        render_caddyfile(&CaddySpec {
            services: &[],
            cert: ca.files.leaf_cert(),
            key: ca.files.leaf_key(),
            listen_port,
            http_port: listen_port + 1,
            kiwix_port,
            portal_dir: portal.clone(),
            portal_host: "kit.local".into(),
            ollama_port: None,
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
        "caddy rejects the generated Caddyfile:\n{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let _caddy = spawn(
        Command::new("caddy")
            .args(["run", "--adapter", "caddyfile", "--config"])
            .arg(&caddyfile),
        &root.join("caddy.log"),
    );
    wait_for(listen_port);

    let ca_pem = fs::read(ca.files.cert()).unwrap();
    let client = PinnedFetch::new(hosts.clone(), listen_port, &ca_pem).unwrap();

    // Other hosts cannot be reached at all.
    assert!(client.get("https://example.com/").is_err());

    // The portal answers on its own name: page from disk, search across all books through Kiwix.
    let front = client.get("https://kit.local/").unwrap();
    assert_eq!(
        (front.status, front.body.contains("PORTAL-MARKER")),
        (200, true),
        "{}",
        front.body
    );
    let all = client.get("https://kit.local/search?pattern=a").unwrap();
    assert_eq!(all.status, 200);
    assert!(
        all.body.contains("/content/"),
        "search results link into the books"
    );
    // The book itself is served under the portal host, with the skin it needs.
    let book_page = client
        .get(&format!("https://kit.local/content/{kiwix_book}/"))
        .unwrap();
    assert!(
        [200, 302].contains(&book_page.status),
        "{} {}",
        book_page.status,
        book_page.final_url
    );
    // Plain http on the portal host only redirects to https.
    let mut plain = TcpStream::connect(("127.0.0.1", listen_port + 1)).unwrap();
    plain
        .write_all(b"GET /search?pattern=x HTTP/1.0\r\nHost: kit.local\r\n\r\n")
        .unwrap();
    let mut reply = String::new();
    plain.read_to_string(&mut reply).unwrap();
    assert!(
        reply.starts_with("HTTP/1.0 308") || reply.starts_with("HTTP/1.1 308"),
        "{reply}"
    );
    assert!(
        reply.contains("ocation: https://kit.local/search?pattern=x"),
        "{reply}"
    );

    // The link checker finds no leaks on the real book.
    let report = check(&client, &hosts, "kit.local", 15, 7);
    println!("{report:?}");
    assert!(report.pages_checked > 1);
    let leaks: Vec<_> = report
        .problems
        .iter()
        .filter(|p| p.contains("outside the machine"))
        .collect();
    assert!(leaks.is_empty(), "{leaks:?}");
}
