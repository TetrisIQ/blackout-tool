//! End to end with real `kiwix-serve` and `caddy` (opt-in). Nothing on the host changes: no hosts
//! file, no certificate store, no nftables, no systemd. Both servers run as child processes on
//! random loopback ports, and the client resolves the mirrored name to Caddy itself.
//!
//!   KIT_E2E_ZIM=/path/to/small.zim cargo test --test e2e_net -- --ignored --nocapture
//!
//! Needs `kiwix-manage`, `kiwix-serve` and `caddy` on PATH.

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use emergency_kit::netcfg::ca::Ca;
use emergency_kit::netcfg::caddy::{CaddySpec, Mirror, render_caddyfile};
use emergency_kit::netcfg::check::{Fetch, PinnedFetch, check};

const HOST: &str = "wiki.example.org";
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
    let hosts = vec![HOST.to_string()];
    let ca = Ca::create(&ca_dir, &hosts, NOW).unwrap();
    ca.ensure_leaf(&hosts, NOW).unwrap();
    let listen_port = free_port();
    let mirrors = [Mirror {
        host: HOST.into(),
        book: book.clone(),
    }];
    let caddyfile = root.join("Caddyfile");
    fs::write(
        &caddyfile,
        render_caddyfile(&CaddySpec {
            mirrors: &mirrors,
            cert: ca.files.leaf_cert(),
            key: ca.files.leaf_key(),
            listen_port,
            kiwix_port,
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

    // The front page redirects to the book's main page, with a certificate the CA vouches for.
    let home = client.get(&format!("https://{HOST}/")).unwrap();
    assert_eq!(home.status, 200, "{}", home.final_url);
    assert!(
        home.final_url
            .starts_with(&format!("https://{HOST}/content/{book}/")),
        "{}",
        home.final_url
    );

    // A Wikipedia-style article URL maps onto the same page.
    let title = home.final_url.rsplit('/').next().unwrap();
    let article = client.get(&format!("https://{HOST}/wiki/{title}")).unwrap();
    assert_eq!(article.status, 200);
    assert_eq!(article.body.len(), home.body.len());

    // Search works under both the Wikipedia and the kiwix spelling.
    let search = client
        .get(&format!("https://{HOST}/w/index.php?search=a"))
        .unwrap();
    assert_eq!(search.status, 200, "{}", search.final_url);

    // Pages that do not exist offline give a friendly answer, not a connection error.
    let missing = client
        .get(&format!("https://{HOST}/w/index.php?title=X&action=edit"))
        .unwrap();
    assert_eq!(missing.status, 404);

    // Other hosts cannot be reached at all.
    assert!(client.get("https://example.com/").is_err());

    // The link checker finds no leaks on the real book.
    let report = check(&client, &hosts, HOST, 15, 7);
    println!("{report:?}");
    assert!(report.pages_checked > 1);
    let leaks: Vec<_> = report
        .problems
        .iter()
        .filter(|p| p.contains("outside the machine"))
        .collect();
    assert!(leaks.is_empty(), "{leaks:?}");
}
