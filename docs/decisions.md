# Technical Decisions

Status: **CONFIRMED** (Phase 0). Language: Rust (chosen by the project owner). Everything else follows the proposal and was accepted with it.

## D1. Language: Rust (stable, edition 2024)
Single static-ish binary, strong types for manifests (`type` is an enum), good ecosystem for the later phases. Toolchain via `rustup` (`rust-toolchain.toml` pins `stable` with `clippy` and `rustfmt`).
Go was the other candidate; it was dropped on the owner's preference.

Crates (Phase 0-1), kept small on purpose:

| Need | Crate | Note |
|---|---|---|
| CLI | `clap` (derive) | |
| Manifests | `serde`, `serde_yaml_ng` | `deny_unknown_fields` everywhere (`serde_yaml` is unmaintained) |
| JSON output | `serde_json` | `--json` for the later web UI |
| Errors | `thiserror` (library), `anyhow` (binary) | |
| XML | `roxmltree` | read-only DOM, enough for OPDS and metalink |
| HTTP | `ureq` (blocking, rustls) | no async runtime until Phase 2 needs one; hidden behind an `Http` trait |
| Hashing | `sha2` | cache keys now, checksum verification from Phase 2 |
| Free space | `rustix` (`fs`) | `statvfs` |
| Tests | `tempfile` | |

## D2. Names
Package/crate `emergency-kit`, binary `kit`. No remote configured; no module path needed in Rust.

## D3. Layout
The roadmap's Go-style `cmd/` + `internal/` is mapped to a Rust lib + bin crate:
`src/main.rs` (thin), `src/cli.rs`, `src/catalog/{manifest,profile,loader,kiwix,http,resolve}.rs`, `src/plan.rs`, `src/diskspace.rs`, and placeholder modules `download`, `install`, `netcfg`, `services`, `ui` (doc comment only until their phase). Also `catalog/`, `docs/`, `test/` (integration environment, later), `tests/` (cargo integration tests and fixtures).

## D4. Manifest format: YAML, one file per package in `catalog/<category>/<id>.yaml`
Fields: `id, name, description, category, type (zim|files|container|apt|model), source{kiwix|url, mirrors, metalink, torrent, image, packages}, sha256, size_bytes, license{spdx, url, attribution}, languages, depends_on, post_install`.

Validation rules enforced by the loader (all errors are collected and reported together, with file paths):
- `id` is `[a-z0-9][a-z0-9_-]*` and equals the file stem; unique across the catalog.
- `license.spdx` is non-empty (roadmap rule 6); `name`, `description`, `category` non-empty.
- `sha256`, if present, is 64 lowercase hex chars; `size_bytes`, if present, is > 0.
- Kiwix-backed packages (`source.kiwix`, type `zim` only) must **not** declare `url`, `mirrors`, `metalink`, `sha256` or `size_bytes` (roadmap rule 5: those come from the live catalog).
- Any other download (`source.url`) must be `https://`, and must declare `sha256` and `size_bytes` (rule 7). Mirrors and metalink must be `https://`; torrent is `https://` or `magnet:`.
- `container` needs `source.image`, `apt` needs `source.packages`; both need an estimated `size_bytes`.
- `depends_on` ids exist, no self-reference, no cycles.
- `post_install` is a list of hook ids; the set of valid hooks is defined in Phase 3.

## D5. Kiwix catalog access (verified against the live service, 2026-10-30)
- `library.kiwix.org` redirects (301) to `opds.library.kiwix.org`; the client follows redirects.
- Query: `/catalog/v2/entries?name=<name>&lang=<iso639-3>&count=50`. The `name` is shared by all variants (`wikipedia_de_all`); the variant is the `<flavour>` element (`mini`, `nopic`, `maxi`; empty for many ZIMs). `name=wikipedia_de_all_nopic` returns nothing. The server **ignores** a `flavour` filter, so flavour (and language) are filtered client-side. `lang` is ISO 639-3 (`deu`), and multi-language ZIMs list several codes.
- The manifest therefore says `source.kiwix: {name: wikipedia_de_all, flavour: nopic, lang: deu}`.
- Flavour omitted in the manifest: accepted only if the name has a single flavour; otherwise "ambiguous" error listing the options.
- The OPDS feed has no checksum and an approximate size. For the chosen entry the fetcher requests its `.meta4` metalink and takes the exact `size`, the `sha-256` and the mirror list (`<url priority=...>`) from there. The download URL is the `.meta4` href without the suffix.
- The newest `updated` entry wins.
- All HTTP goes through an `Http` trait. `CachedHttp` stores bodies in `$XDG_CACHE_HOME/kit/` with a TTL (default 24 h), falls back to stale cache when the network fails (reported as a warning), and `--offline` uses cache only.
- Tests use fixtures recorded from the live service (`tests/fixtures/kiwix/`, piece hashes stripped from metalinks) and never touch the network.

## D6. Profiles: YAML in `catalog/profiles/<id>.yaml`
`{id, name, description, budget_bytes, packages[]}`. Mini (~50 GB), Standard (~250 GB), Full (~1 TB). Exceeding the nominal budget is a warning. `--select a,b,c` is the custom profile.

## D7. `kit plan`
- Resolves `depends_on` transitively (dependencies first), dedupes, fetches live size per package.
- Required space = total + largest single package (temp space for the `.part` file) + 5 % margin on that sum. Free space is `statvfs` on the nearest existing ancestor of `--data-dir` (default `$XDG_DATA_HOME/kit`).
- **Refuses (exit code 3) when free space is insufficient.** Exit 1 for other errors, including a package whose size cannot be determined (no network and no cache): an unknown size never counts as zero.
- Warnings: stale cache used, budget exceeded, two variants of the same Kiwix content selected.
- Output: aligned table; `--json` for machine use. Installed packages are not subtracted yet (nothing is installed before Phase 3).
- Exit codes: 0 ok, 1 error, 3 insufficient disk space.

## D8. Tooling and CI
- `Makefile`: `build`, `test`, `lint` (`cargo fmt --check` + `cargo clippy -D warnings`), `fmt`, `clean`.
- GitHub Actions: lint, test, build.
- Tests use `tempfile` directories and a fake free-space provider; nothing touches the host's `/etc` or the network.

## D9. First release target: Debian/Ubuntu
Development happens on Arch; the binary has no distro dependency. Integration tests for later phases will run in a Debian container. Arch is not a release target.

## D10. Deferred
Podman, Caddy, dnsmasq, systemd user services, and web UI vs. TUI (Phase 8) stay as recommended in the roadmap; they are not needed before their phases. The async runtime (likely `tokio`) is decided in Phase 2.

---

# Phase 2 - Download engine (CONFIRMED, implemented)

## P2-1. Blocking I/O with worker threads, no async runtime
`ureq` is already in use and supports `Range` requests with streaming bodies. A pool of N threads (default 4) each fetches chunks and writes them with positional writes (`FileExt::write_at`) into a pre-allocated `.part` file. This avoids `tokio`, keeps the binary small, and downloads are bandwidth-bound, not connection-bound. Revisit in Phase 8 only if the web UI needs async; it can wrap the engine.

## P2-2. Files and state
- Target `<dir>/<name>`, partial data in `<name>.part`, progress in `<name>.part.state` (JSON, written atomically via temp + rename after every completed chunk).
- State holds: the source URL list, total size, expected sha256, chunk size (default 8 MiB), a bitmap of completed chunks, and the server validators (ETag / Last-Modified) of the first response.
- On restart the state is validated against the manifest (size, sha256) and the server (`If-Range` / validator compare). On mismatch the partial file is discarded and the download restarts. Never resumes onto data it cannot trust.
- Only whole chunks count as done, so a `kill -9` or power loss costs at most the chunks in flight. The `.part` file is `fsync`ed before the state records the chunk.
- An exclusive lock file (`flock`) per target prevents two `kit` processes writing the same file.

## P2-3. Retry, mirrors, bandwidth
- Per-chunk retry with exponential backoff and jitter (1 s -> 60 s cap, 8 attempts), honoring `Retry-After`.
- Source order: `url`, then `mirrors` in priority order. After repeated failures on one source a chunk switches to the next; a source that returns a different size or ignores `Range` is dropped for the rest of the run. All sources serve the same checksum-verified file, so mixing chunks from mirrors is safe.
- `--limit-rate <bytes/s>` via a shared token bucket across threads. Default unlimited.

## P2-4. Verification and finish
- After the last chunk: stream the file through SHA-256 and compare to the checksum from the catalog (rule 7). On mismatch: delete `.part` and the state, report corruption, exit non-zero (no half-trusted file is ever left under the final name).
- On success: `fsync`, atomic `rename` `.part` -> final, remove the state file. A file already present with the right size is re-verified, not re-downloaded; idempotent.

## P2-5. Disk preflight and data directory
- Data directory: `--data-dir` / `KIT_DATA_DIR`, default `$XDG_DATA_HOME/kit`, may be an external drive. Downloads go to `<data-dir>/downloads/`.
- Preflight reuses the Phase 1 free-space check, but counts only the bytes still missing (partial files already on disk are credited) plus the 5 % margin. Temp space is the `.part` file itself, so no extra reserve.

## P2-6. CLI
`kit download --profile <id> | --select a,b [--data-dir D] [--jobs N] [--limit-rate R]` resolves the plan (including the disk check and refusal, exit code 3), then downloads sequentially package by package, showing progress. `kit` re-run continues. Installing is Phase 3; this phase only fetches and verifies.

## P2-7. Testing (no real network, nothing on the host)
- A small in-process test HTTP server (std `TcpListener`) with `Range`/`If-Range` support and injectable faults: connection reset mid-body, 5xx bursts, truncated bodies, flipped bytes, ignoring `Range`, a dead primary with a working mirror.
- Unit tests: resume from state, state/server mismatch, corrupt checksum, mirror fallback, rate limiter, chunk bitmap, lock contention.
- Integration test running the real `kit` binary against the test server, `kill -9`ing it mid-download, restarting, and checking the checksum (64 MiB in CI so it stays fast).
- Acceptance script in `test/` for the roadmap's 5 GB case: generates a deterministic 5 GB stream on the fly (no disk needed on the server side), serves it locally, kills the client with `kill -9`, drops the connection repeatedly, and asserts the final checksum. Run manually, not in CI.
- Metalink/torrent (roadmap: optional) is deferred; the `.meta4` mirror list is already used.

## P2 deviations from the proposal (found while implementing)
1. **No ETag / Last-Modified validators.** Mirrors of one file carry different validators, and the catalog's size and SHA-256 are the authority anyway. A resume is trusted only if name, size and checksum in the state match the catalog **and** the `.part` file has the exact size; the final SHA-256 check catches anything else (a changed upstream file ends in "checksum mismatch", the partial data is deleted).
2. **The temp-space reserve in `kit plan` is gone.** Phase 1 reserved the largest package on top of the total. Since `<name>.part` is renamed to the final name, no extra space is needed, so required = total + 5 % margin. This is why Standard needs ~210 GB instead of 343 GB.
3. **Plain `http://` is accepted for loopback addresses only** (`127.0.0.1`, `localhost`, `[::1]`) in manifests, so tests can serve files locally. Everything else must be `https://`.
4. **Disk space is reserved up front** with `fallocate` when a download starts, so a full disk fails immediately (`kit` exits with code 3 from the preflight, or an error naming the directory) instead of hours in. Filesystems without `fallocate` fall back to a sparse file.
5. **Extra flags:** `--chunk-size` (advanced; a resumed download keeps the size it started with) and `--retries`. With the defaults (10 attempts, 1 s base, 60 s cap) a chunk survives an outage of roughly 3 to 5 minutes inside one run; after that `kit` exits non-zero and the next run continues from the saved state.
6. **Layout:** files go to `<data-dir>/downloads/<package-id>/<file-name>` with `<name>.part`, `<name>.part.state` and a `<name>.lock` (left in place after finishing; flock needs a stable inode).
7. Metalink/torrent support stays deferred; mirrors from the `.meta4` file are used.

---

# Phase 3 - Package installers, ZIM first (CONFIRMED, implemented)

Goal (roadmap): `kit install wikipedia_de_nopic` yields a working local Wikipedia at `http://localhost:6589`.

## P3-1. Installer interface
```rust
trait Installer {
    fn plan(&self, pkg, ctx) -> Result<Vec<Step>>;     // dry run: what would change, no side effects
    fn install(&self, pkg, ctx) -> Result<()>;         // idempotent
    fn verify(&self, pkg, ctx) -> Result<Vec<Check>>;  // read-only health checks
    fn uninstall(&self, pkg, ctx) -> Result<()>;       // idempotent, reversible
}
```
Installers are looked up by package `type`. Phase 3 implements `zim`; other types fail with "not supported yet" instead of half-working. Everything that touches the host goes through two small traits so tests never do: `CommandRunner` (runs `kiwix-manage`, `systemctl --user`, ...) and the filesystem paths in `Ctx` (all under the data dir, config dir and unit dir, all overridable).

## P3-2. Installed state is the source of truth
`<data-dir>/state/installed.json` (atomic write, flock): per package id, the version, files, and install time. Everything else (Kiwix library XML, portal page, service) is **reconciled from this state** on every install/uninstall, which makes each step idempotent by construction: running it twice converges to the same result, and a half-finished run is repaired by the next one.

## P3-3. Placing the ZIM: hard link, no copy
The verified file stays in `downloads/<id>/<name>.zim` (so `kit download` stays a no-op). `kit install` hard-links it to `<data-dir>/library/<name>.zim`. Same filesystem, no extra space, instant, even for 127 GB. `uninstall` removes the link; `uninstall --purge` also deletes the download. If hard-linking fails (data dir on a filesystem without hard links, e.g. exFAT on an external drive) it falls back to **moving** the file and leaving no duplicate, recorded in the state so `download` knows it is installed.

## P3-4. Kiwix library XML via `kiwix-manage`
`<data-dir>/library/library.xml` is rebuilt from the state with `kiwix-manage` (reads ZIM metadata from the file itself, so no OPDS needed offline). Rebuilding from scratch avoids stale or duplicate entries. A missing `kiwix-manage`/`kiwix-serve` is a clear error with the install command for the detected distro (`sudo apt install kiwix-tools`, `sudo pacman -S kiwix-tools`).

## P3-5. kiwix-tools is a system package; kit does not install it
Roadmap rule 2: root only for specific, explained steps. Installing packages is such a step but belongs to the distro, and a release `.deb` can simply depend on `kiwix-tools`. Alternative (not now): `kit` downloads the official static kiwix-tools build into `<data-dir>/bin`, unprivileged and distro-independent. I recommend doing that later for the live image and AppImage, not in Phase 3.

## P3-6. Service: systemd user unit `kit-kiwix.service`
- File: `~/.config/systemd/user/kit-kiwix.service`, first line `# Managed by kit (emergency-kit). Remove with: kit uninstall`.
- `ExecStart=<kiwix-serve> --library --monitorLibrary --address 127.0.0.1 --port 6589 <data-dir>/library/library.xml`; `Restart=on-failure`; no root.
- `--monitorLibrary` makes `kiwix-serve` pick up library changes without a restart (to verify against Debian's older kiwix-tools; if unsupported there, kit restarts the service instead).
- Bound to `127.0.0.1` for now. Phase 4 puts Caddy in front, Phase 9 opens it to the hotspot, deliberately, not by accident.
- Port **6589** by default (chosen by the project owner instead of the roadmap's 8080, which collides with many dev servers; it is below the ephemeral range, so outgoing connections will not take it). `--port` to change; an occupied port is a clear error, never a silent switch.
- Uninstalling the last ZIM stops, disables and removes the unit and runs `daemon-reload`, so nothing of ours is left behind.
- Starting at boot without a login session needs `loginctl enable-linger` (root); `kit` prints this as a hint, it is not done silently. It matters for the live image and the hotspot (later phases).

## P3-7. Landing page stub
`<data-dir>/portal/index.html` (static, generated from the state: title, size, version, license and a link per installed ZIM to `http://localhost:6589/viewer#<book>`) plus `installed.json` for the later portal. `kiwix-serve` already shows its own library page at `:6589`; the real portal on a friendly domain is Phase 5.

## P3-8. CLI
- `kit install --profile <id> | --select a,b [--data-dir] [--port] [--no-download]` downloads (Phase 2 engine, with its disk check) and installs. `--dry-run` prints the plan.
- `kit uninstall <id>... [--purge]`
- `kit verify [<id>...]`: file present with the right size, library entry, unit active, HTTP 200 from the server; `--deep` re-hashes the files. Exit code 1 on any failed check.
- `kit status`: installed packages, sizes, service state, URL.

## P3-9. Testing
- Unit tests with a fake `CommandRunner` and temp dirs: state transitions, idempotent reinstall, uninstall, crash-in-the-middle recovery (state written but service not started, etc.), unit-file golden test, portal HTML, hard-link fallback, missing tools error, port-in-use error.
- Opt-in end-to-end test (`cargo test -- --ignored`, needs `kiwix-tools`): installs a small real ZIM into a temp data dir and runs `kiwix-serve` as a plain child process on a random port (no systemd, no host change), checking `/catalog/v2/entries` and an article.
- **Manual acceptance on this machine** (the roadmap criterion): you install `kiwix-tools`, then `kit install --select archwiki_en` (38 MB) and later `wikipedia_de_nopic` (18.6 GB); this creates the one user unit above, removable with `kit uninstall`. Automated tests never touch your systemd or `~/.config`.

## P3 decisions and deviations
Confirmed: kiwix-tools as a system package, hard link with move fallback, a `kit-kiwix.service` user unit, and **port 6589** (supersedes the roadmap's `localhost:8080`).

Found while implementing:
1. **Tools are checked first.** `kit install` verifies `kiwix-manage`, `kiwix-serve` and `systemctl` before downloading anything, so a missing package costs seconds, not an 18 GB download.
2. **The state lock is not held while downloading**, only while installing; `kit status` and `kit verify` keep working during a long download. A second installer at the same time is refused ("another kit process ...").
3. **`kit install` skips packages that are already current** (same checksum recorded). A re-run is a no-op and downloads nothing.
4. **Install order for crash safety:** place the file, write the state, remove files of an older version, then reconcile (library, unit, service, portal). Uninstall saves the state first, then deletes files. A crash at any point is repaired by running the same command again. Orphaned files are never deleted automatically (after a move they could be the only copy).
5. **A unit file without kit's marker line is never overwritten or removed.**
6. **`--purge`** also removes the download engine's `.lock`, `.part` and `.part.state` leftovers and the empty `downloads/<id>/` directory.
7. **A moved (not linked) file is the only copy:** uninstall without `--purge` warns that the data was deleted and `kit download` fetches it again.
8. `--port` accepts 1024 and up. An occupied port is an error before the service is touched. If the service is already running, the check is skipped.
9. **Tests:** the host (commands, unit dir, tool search path, HTTP probe, port check) is injected, so all 50 new install and CLI tests run against fakes and temp dirs. `tests/e2e_kiwix.rs` (opt-in, `--ignored`) runs the real `kiwix-manage` and `kiwix-serve` with a shim `systemctl`, still without touching systemd.

---

# Phase 4 - Local domains over HTTPS (CONFIRMED, implemented; browser acceptance pending)

Goal (roadmap): with the cable unplugged, `https://de.wikipedia.org/wiki/Berlin` loads in Firefox and Chromium without warnings, and 50 random links work.

Already decided by the project owner: nftables redirect 443 -> 8443 (P4-4), `/etc/hosts` only in this phase (P4-5), start despite Phase 3 being accepted on ArchWiki only.

## P4-1. Local CA generated in Rust (`rcgen`), not via the `openssl` CLI
- Per-machine CA in `<data-dir>/ca/` (`ca.key` mode 0600, `ca.pem`), ECDSA P-256, 1 year leaf / 5 year CA, created on first `kit net enable`. Never shipped.
- **Name constraints** (critical): permitted DNS subtrees only for the domains of installed packages (`wikipedia.org`, `wikimedia.org`, ...). The domain list lives in the catalog manifests (new optional field `mirror_domains`), not in code. If the list changes, the CA is **not** re-issued (its constraint is the union of all known catalog domains at creation); leaf certs are re-issued.
- Leaf: one cert with SANs for every mirrored host, signed by the CA, regenerated by reconcile when the domain set changes.

## P4-2. Trust stores (root, each step explained and reversible)
- System store: `trust anchor` (p11-kit, Arch/Fedora) or `/usr/local/share/ca-certificates` + `update-ca-certificates` (Debian/Ubuntu). Needs root once.
- Firefox and Chromium NSS DBs (`~/.mozilla/firefox/*.default*`, `~/.pki/nssdb`) via `certutil`, unprivileged, nickname `kit local CA`. Removed by `disable`.
- Flatpak/Snap browsers: not handled, printed as a known limitation by `kit net status`.

## P4-3. Caddy
- Caddy is a system package like kiwix-tools (checked up front, clear install hint). Config generated from installed state into `<data-dir>/net/Caddyfile`, run as user unit `kit-caddy.service` with `--config <file> --adapter caddyfile`, auto-HTTPS disabled (we supply the cert), admin API off.
- Listens on `127.0.0.1:8443` (unprivileged). One vhost per mirrored domain; upstream `127.0.0.1:6589` (kiwix-serve).
- Rewrites: `/wiki/<Title>` -> `/content/<book>/A/<Title>` (redirect-aware), `/w/index.php?search=` -> kiwix search, `upload.wikimedia.org` and other asset hosts -> the book's embedded media paths. A catch-all returns a friendly "not available offline" page (HTTP 404, no connection error) for everything else. Real request inventory taken from a live article before writing rewrites (roadmap risk).

## P4-4. Port 443 via nftables redirect (owner's choice)
- Root step: a dedicated table `inet kit_net` with a NAT output rule redirecting `127.0.0.1:443` -> `8443`, loaded with `nft -f`. Marked by the table name, so `disable` is `nft delete table inet kit_net`; nothing else is touched. Not persisted across reboot by default; `kit net enable --persist` installs a small system unit that reloads it (root, explained).
- Only loopback destinations are redirected, so no outside traffic is affected.
- Trade-off noted: without persistence the redirect is gone after reboot and `kit net status` says so.

## P4-5. Name resolution
- Managed block in `/etc/hosts` between `# BEGIN kit (emergency-kit)` and `# END kit`, mapping every mirrored domain to `127.0.0.1`. Written atomically, root via `sudo`/`pkexec` for that one file only, idempotent, removed by `disable`.
- dnsmasq config generation is part of Phase 9 (hotspot); not activated here.

## P4-6. CLI
`kit net enable [--persist] | disable | status`. `enable` is a reconcile: CA, leaf, Caddyfile, unit, then the privileged steps (trust, hosts, nft) listed with `--dry-run` first. `disable` reverses every step and `--remove-ca` also deletes the CA and its key. Uninstalling the last mirrored package triggers `disable`.

## P4-7. Link checker
`kit net check [--samples 50]`: picks N article links from the served pages (starting at the main page), requests them through the real vhost with a client whose DNS is pinned locally, and asserts HTTP 200, no redirect off-machine, and no absolute URL pointing to a non-mirrored host in the body. Reports every offender.

## P4-8. Testing
- Unit: CA generation (name constraints present and enforced, verified by validating a leaf for an allowed and a forbidden name with a test verifier), Caddyfile golden test, hosts-block edit (idempotent, preserves foreign lines), nft ruleset golden test, NSS command lines with `FakeRunner`.
- No test touches `/etc`, nft, certutil on real profiles, or the system trust store. Everything is via `Ctx` (paths and `CommandRunner`).
- Opt-in e2e (`--ignored`, needs caddy + kiwix-tools): Caddy on a random port with the generated cert, curl with `--resolve` and `--cacert`, a fixture ZIM, checking rewrite, "not available offline" page and cert validity.
- Manual acceptance on this machine (`test/accept_net.sh` checklist): `kit net enable`, unplug network, load Berlin in Firefox, follow 50 links via `kit net check`, then `kit net disable` and verify hosts, nft, NSS and trust store are clean.

## P4-10. Amendment (2026-11-03): the real-domain mirror is gone
`kit.local` replaced it, so `mirror_domains`, the per-book vhosts with `/wiki/<Title>` and `/w/index.php` rewrites, and the hosts entries for `*.wikipedia.org` and friends were removed. `kit net` now serves only `kit.local` and the service hosts (`<service>.kit.local`); real internet names are never touched. The CA's name constraint is the portal host plus all catalog service hosts. Existing setups: re-run `kit net enable` (the hosts block and Caddyfile are rewritten; the old CA stays valid as its constraint is a superset). Points 1 and 2 of "P4 decisions" below describe the removed mirror and are kept as history.

## P4-9. Open question
Chromium is not installed on this machine, so only Firefox (and `~/.pki/nssdb`) can be verified for real; Chromium coverage rests on the shared NSS path.

## P4 decisions and deviations (found while implementing)
1. **Name constraint = exact hosts, not registrable domains.** `mirror_domains` lists exact hosts (`de.wikipedia.org`, `de.m.wikipedia.org`); the CA is constrained to the union of all hosts in the shipped catalog, so a stolen key cannot sign `wikipedia.org` itself or any other host. Adding a package with a new host later needs `kit net enable --renew-ca` (new CA, old one is untrusted, new one trusted again).
2. **Only Wikimedia projects have `mirror_domains` so far** (Wikipedia en/de, Wiktionary, Wikibooks, Wikisource, Wikivoyage en). ArchWiki (`/title/X`), Stack Exchange and Gutenberg need their own URL rewrites; hijacking `stackoverflow.com` without them would only break the site while online. `upload.wikimedia.org` is not mirrored: the ZIMs embed their images.
3. **nftables table is `ip kit_net`, not `inet`.** Only IPv4 loopback is redirected and the hosts block is IPv4-only, so an `ip` table is enough and works on older kernels. The ruleset declares and deletes the table before creating it, so reloading never stacks a second redirect.
4. **The redirect is not persisted across reboot** (`--persist` from the proposal is not implemented). `kit net status` reports a missing redirect and `kit net enable` restores it; a boot unit comes with the live image (Phase 11).
5. **`kit uninstall` does not touch the network layer.** Changing hosts/nft needs root, so it prints a note to run `kit net enable` or `disable` instead of the proposal's automatic disable.
6. **Root is explicit and local:** `sudo` is called per command (`cp` onto `/etc/hosts`, `nft -f`, `trust anchor` / `update-ca-certificates`), the plan with the reason for each `[root]` step is printed first, and `--dry-run` stops there. The unprivileged runner never receives these commands (tested).
7. **Caddy config is validated before use:** `kit net enable` runs `caddy validate` and aborts without starting or changing anything privileged if it is rejected. The Caddyfile syntax could not be validated while writing it (caddy was not installed here); `tests/e2e_net.rs` (opt-in) runs the real thing.
8. **Rewrites:** `/wiki/<Title>` is proxied (not redirected) to `/content/<book>/<Title>` so relative links inside the ZIM keep working; `/` redirects to the book; `/w/index.php?search=` and `/wiki/Special:Search` become kiwix `/search?content=<book>&pattern=`; everything unknown answers 404 with a "Not available offline" page. The book id is the ZIM file name without `.zim` (verified against kiwix-serve 3.8: `/content/archlinux_en_all_maxi_2026-10/Main_page`, with `A/...` redirecting).
9. **`kit net check`** resolves only mirrored hosts (to Caddy) and trusts only the local CA, so a request that would leave the machine fails by construction. External `<a>` links are counted, not followed; embedded resources (img/script/css) from other hosts are reported as leaks.
10. **Test hygiene:** spawning `openssl` from a library test could leave a forked child holding another test's `flock` for a moment (spurious "another kit process" failures). The X.509 check therefore lives in its own test binary, `tests/net_ca.rs`.

---

# Phase 5 - Portal and unified search (implemented; decided without a separate proposal round)

Started on the owner's instruction ("continue, I will test the app later") while the Phase 4 browser acceptance was still pending.

## P5-1. The portal lives on `kit.local`, behind the Phase 4 front
`https://kit.local` (and `http://kit.local`, which redirects with 308) is a normal Caddy site next to the mirrored hosts: `kit.local` is in the hosts block, the leaf certificate and the CA's name constraint. It serves `<data>/portal/kit.html` from disk and proxies `/content`, `/search`, `/skin`, `/suggest`, `/catalog` to kiwix-serve, so the page, the search and the books share one origin (relative links). `.local` is the mDNS suffix, but `files` comes before `mdns` in nsswitch on mainstream distros; a different name is a one-line constant (`PORTAL_HOST`).
An existing CA made in Phase 4 does not permit `kit.local`: `kit net enable --renew-ca` once.

## P5-2. Unified search is kiwix-serve's own cross-library search
`GET /search?pattern=...` without `content=` searches every book in the library (verified against kiwix-serve 3.8, 1,064 hits for "pacman" with one book). No search code of our own: the portal's form submits `pattern` to it and the results link into the books. `tests/e2e_net.rs` checks it through the front.

## P5-3. Emergency shortcuts are saved searches
A fixed list (`EMERGENCY_SEARCHES`: first aid, resuscitation, drinking water, bleeding, hypothermia, food preservation, English and German terms) links to `/search?pattern=...`, so they work with whatever is installed and need no curated content yet. Emergency numbers are printed as plain text. Contacts and documents come with the personal emergency folder (Phase 10).

## P5-4. Two page variants
`index.html` (open as a file, absolute links to `localhost:<port>`) and `kit.html` (served by the front, relative links), both generated by `install::portal` from the installed state on every install/uninstall.

## P5-5. Mirror address 127.77.0.1 instead of 127.0.0.1 (change to Phase 4)
Adding a redirect for port 80 would have hijacked any local web server on `localhost:80`. The hosts block now maps the names to `127.77.0.1` (all of 127.0.0.0/8 is loopback) and the nftables table redirects 80 and 443 for that address only. Caddy still listens on `127.0.0.1:8443/8444`.

---

# Phase 6 - Service packages (first slice: framework, Navidrome, one music pack)

Started on the owner's instruction while the Phase 4/5 browser acceptance was still pending. Owner's choices: Podman Quadlet, one host name per service under `kit.local`, first slice = framework + Navidrome + free music; Audiobookshelf, Jellyfin and Kavita/Calibre-Web follow as further manifests on the same framework.

## P6-1. Manifest: `service` and `content`
- `type: container` needs `source.image` pinned by digest (`repo[:tag]@sha256:...`; validation rejects anything else, the digest is the checksum, rule 7) and a `service` block: `host` (under `kit.local`), `port` (same inside and on 127.0.0.1), `health_path`, `env`, `mounts` (`data` = the service's own directory, `media` = `<data>/media/<kind>` shared with content packs, optionally read-only), `provision`.
- `type: files` with a `content` block (`media` kind, `extract: zip|none`) is a content pack: it lands in `<data>/media/<kind>/<id>/`. Kinds: music, audiobooks, video, books, games.

## P6-2. Quadlet, rootless, loopback only
`kit install` writes `~/.config/containers/systemd/kit-<id>.container` (first line = marker; a file without it is never touched), `daemon-reload`, `systemctl --user start kit-<id>.service`, waits for the health path. `PublishPort=127.0.0.1:<port>:<port>`, so nothing is reachable from outside until Phase 9 decides it. `podman pull` by digest is the only step that needs the internet; after that the image lives in Podman's local storage.
Tests: golden/behaviour tests with fakes, plus `tests/quadlet.rs` which feeds the shipped manifests to Podman's real generator (`quadlet -dryrun`, nothing installed).

## P6-3. Provisioning without a wizard
`provision: navidrome` creates the first admin through Navidrome's own `POST /auth/createAdmin` (verified against 0.64.2: 200 the first time, 403 "Cannot create another first admin" afterwards), with a random 24-hex password written to `<data>/services/<id>/credentials.json` (mode 0600) only after the call succeeded. `kit status` prints the login; `--purge` deletes it. If an admin already exists kit says so and does not invent credentials. Trade-off: the password is readable by the user who owns the data directory, which is the same trust boundary as the service data itself.

## P6-4. Front and portal
Each service gets its own Caddy site (`https://navidrome.kit.local`, apps do not like sub-paths) proxying to its loopback port; hosts, leaf certificate and the CA's name constraint include it. The portal lists services as tiles. Existing CAs from before this phase do not permit the new host names: `kit net enable --renew-ca`.

## P6-5. Content packs are unpacked, not copied twice
Zips are unpacked into a temporary directory and renamed into place (no half-unpacked pack after a crash), `enclosed_name` rejects zip-slip, a 200 GB cap bounds zip bombs. A pack record is kept in the same state file; `library_files()` now only returns ZIMs, so packs never end up in the Kiwix library.

## P6 decisions found while implementing
1. **`kit install` installs packages without a download** (containers). Tools are checked per package type up front (`podman` for containers, the Kiwix tools only if ZIMs are involved or installed).
2. **Updating an image** is a manifest change (new digest): the next `kit install` sees the recorded image differs, pulls, rewrites the Quadlet file and restarts the unit. The old image is not pruned automatically.
3. **No linger hint yet** for container services (the Kiwix hint covers the same `loginctl enable-linger` need).
4. **Legal status of content:** each pack states its licence; items hosted on archive.org carry an uploader-claimed licence, so only a source with a trustworthy provenance is shipped (Musopen, see progress notes). The roadmap's "legal check of each entertainment source" remains open for every further pack.

## P6-6. Audiobookshelf provisioning scans explicitly
Audiobookshelf has no environment-based first login; kit calls `/init`, `/login`, `POST /api/libraries` and then `POST /api/libraries/<id>/scan`. Found on the real image 2.37.2: without the scan the library stays empty, because books unpacked before the library exists are not seen by the folder watcher. Later packs are picked up by the watcher.

---

# Collections: the whole Standard Ebooks catalog (CONFIRMED by the project owner, 2026-11-03)

## C1. New package type `collection`, trust on first download
`catalog/books/standardebooks_all.yaml` (`type: collection`, `source.collection.provider: standardebooks`, `content: {media: books, extract: none}`, an estimated `size_bytes`). Standard Ebooks publishes no checksums; its bulk zips and OPDS feeds are for Patrons Circle members (a login, which kit does not automate). kit therefore lists the books from the public sitemap (a book is a page that has a `/text` page below it), fetches each EPUB from its public per-book download URL, and records the SHA-256 of every file at the first download for `kit verify --deep`.

**Deviation from rule 7 (verify checksums before installing):** there is nothing to verify against beforehand. What is checked instead: HTTPS, the file must be a real EPUB (a zip whose `mimetype` entry is `application/epub+zip`; a login page or error page is refused), and later changes are detected. A tampered file at the very first download would not be noticed. The deviation is limited to this package type; manifests with `source.url` still need `sha256`.

## C2. Being a good guest
One request every 500 ms, three tries per book with growing pauses, and the run stops when three books in a row fail (blocked or offline). The user agent is `emergency-kit/<version>`. The site's `robots.txt` bans some AI-assistant agents from the download paths and its pages warn about a 24 h IP ban for bots; kit is a user-run tool that sends one polite request at a time, but a user who is a Patron should prefer the official bulk zips.

## C3. Resumable, recorded at the end
Each book is written as `<file>.epub.part`, checked, and renamed into `<data-dir>/media/books/<id>/`; an interrupted run keeps the finished books and the next run fetches only the missing ones (leftover `.part` files and non-EPUBs are removed first). The package is recorded in `installed.json` (one `FileRecord` per book) only when all books are there, so `kit status` shows it as installed only then. A plain re-run does not pick up books that were added to the site later (that is an update, Phase 10).

---

# Catalog repositories (CONFIRMED, implemented)

## R1. The catalog is its own repository
`catalog/` moved out of the kit repository (`../katalog`: manifests, `profiles/`, `catalog.yaml`, CI that runs `kit catalog check .`, a release workflow that publishes `catalog.tar.gz` and `catalog.tar.gz.sha256`). The kit keeps only a small fixture catalog for tests.

## R2. Transport: HTTPS tarball + SHA-256, not git
No `git` needed on the user's machine, and the checksum rule (rule 7) applies to the catalog too. A source is `{name, url, sha256?, enabled}` in `<config>/kit/catalogs.json`; `<url>.sha256` is read on each update unless the hash was pinned with `--sha256`. A hash served next to the archive protects against corruption and half-finished uploads, not against a hostile host; pinning does, at the price of a new pin per update. Signed catalogs are Phase 12. Plain `http://` is accepted only for the local machine.

## R3. Several catalogs, first one wins
Priority is the list order (`kit catalog move`). An id (package or profile) that an earlier catalog defines is skipped with a warning; duplicates inside one catalog stay an error. References are checked on the merged set, so a catalog may depend on packages of another. The origin is shown in `kit list` (when more than one catalog contributes) and `kit show`.

## R4. Safe replacement
Download to a work folder, verify, unpack (regular files only, any `..`/absolute path or link is refused, size capped), load the result as a catalog, then swap the directory. Anything failing leaves the previous copy in place. One wrapper directory (GitHub-style archives) is looked through.

## R5. Where things live and the window
Catalogs sit in `<data>/catalogs/<name>/` (moved along by `kit data-dir set --move`). The location of catalogs follows `KIT_DATA_DIR` or the saved place, not another command's `--data-dir`. The window has a **Catalogs…** dialog (sidebar) instead of a tab; it runs `kit catalog ...` as child processes.
