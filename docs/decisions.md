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

# Phase 3 - Package installers, ZIM first (PROPOSED, awaiting confirmation)

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

## P3 open questions
1. kiwix-tools as a system package (you run `sudo pacman -S kiwix-tools` for the acceptance test), per P3-5: OK?
2. Hard link into `library/` with move fallback (P3-3): OK?
3. May I create the `kit-kiwix.service` user unit on this machine for the manual acceptance test (P3-6)? Removal is `kit uninstall`.
4. ~~Bind to 127.0.0.1:8080~~ **Decided:** 127.0.0.1:6589 (supersedes the roadmap's `localhost:8080` acceptance URL).
