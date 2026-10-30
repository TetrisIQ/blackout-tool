//! Serves a deterministic generated file with optional faults, for the manual 5 GB acceptance
//! test (`test/accept_5gb.sh`). Not part of the product.
//!
//!   fake_server <size-bytes> [--seed N] [--print-sha256] [--reset-every N] [--outage SECS:EVERY]
//!               [--piece-delay-ms N]   (pause after every 64 KiB per connection, to throttle)
//!
//! Prints `PORT <n>` and then serves `/data.bin` until killed. `--outage 5:60` takes the server
//! down for 5 s every 60 s. `--print-sha256` prints `SHA256 <hex>` first (generating the stream
//! once; takes a while for large sizes).

#[path = "../tests/common/server.rs"]
mod server;

use std::sync::atomic::Ordering;
use std::time::Duration;

use server::{Content, Generated, Server};

fn main() {
    let mut args = std::env::args().skip(1);
    let size: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .expect("usage: fake_server <size-bytes> [options]");
    let (mut seed, mut print_sha, mut reset_every, mut outage, mut delay) =
        (1u64, false, 0u64, None::<(u64, u64)>, 0u64);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--seed" => seed = args.next().unwrap().parse().unwrap(),
            "--print-sha256" => print_sha = true,
            "--piece-delay-ms" => delay = args.next().unwrap().parse().unwrap(),
            "--reset-every" => reset_every = args.next().unwrap().parse().unwrap(),
            "--outage" => {
                let v = args.next().unwrap();
                let (d, e) = v.split_once(':').expect("--outage SECS:EVERY");
                outage = Some((d.parse().unwrap(), e.parse().unwrap()));
            }
            other => panic!("unknown option {other}"),
        }
    }
    let content = Generated { len: size, seed };
    assert_eq!(content.len(), size);
    if print_sha {
        println!("SHA256 {}", content.sha256());
    }
    let srv = Server::start(content);
    srv.faults.reset_every.store(reset_every, Ordering::SeqCst);
    srv.faults.piece_delay_ms.store(delay, Ordering::SeqCst);
    println!("PORT {}", srv.port);
    if let Some((down_for, every)) = outage {
        let faults = srv.faults.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(every));
                faults.down.store(true, Ordering::SeqCst);
                eprintln!("fake_server: network DOWN for {down_for}s");
                std::thread::sleep(Duration::from_secs(down_for));
                faults.down.store(false, Ordering::SeqCst);
                eprintln!("fake_server: network back");
            }
        });
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
