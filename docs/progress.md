# Progress

## Phase 0 - Bootstrap (done)
- Decisions confirmed and recorded in `docs/decisions.md` (Rust, YAML manifests, Debian/Ubuntu target).
- Cargo project: library `emergency_kit` + binary `kit`; placeholder modules for later phases; `catalog/`, `docs/`, `test/`, `tests/`.
- `CLAUDE.md` with the project rules, `Makefile`, GitHub Actions CI (lint, test, build), `rust-toolchain.toml`.
- Acceptance: `make test` and `make build` are green. (The `test/` directory for VM/container integration tests is reserved for Phase 3+; nothing needs it yet.)

## Phase 1 - Catalog and manifest (done)
- **Manifest schema** (`src/catalog/manifest.rs`): `id, name, description, category, type, source, sha256, size_bytes, license, languages, depends_on, post_install`, strict validation (license required, checksums required for direct downloads, Kiwix content must not hardcode size/URL/checksum, https only).
- **Catalog loader** (`src/catalog/mod.rs`): walks `catalog/`, reports all errors with file paths, checks unique ids, dependencies, cycles, profile references, file name = id.
- **Kiwix fetcher** (`src/catalog/kiwix.rs`, `http.rs`): OPDS feed plus `.meta4` metalink for exact size, SHA-256 and mirrors; 24 h disk cache, stale-cache fallback with warning, `--offline`.
- **CLI**: `kit list`, `kit show <id>`, `kit profiles`, `kit plan --profile <id> | --select a,b` (`--json`, `--data-dir`, `--offline`).
- **Profiles**: `mini`, `standard`, `full` over 15 real Kiwix packages (Wikipedia en/de, Wiktionary, Wikibooks, Wikivoyage, Wikisource, ArchWiki, Stack Exchange, Gutenberg).
- **Acceptance**: `kit plan` prints a size table for a profile and exits with code 3 when free space is insufficient. Verified in tests (fake disk space, real binary) and against the live Kiwix catalog on 2026-10-30.

### Findings and caveats
- Kiwix names differ from the roadmap's examples: `wikipedia_de_all` + `flavour: nopic` + `lang: deu`; the server ignores a `flavour` filter, so it is applied client-side. OPDS has no checksum; the `.meta4` file does.
- *(Superseded in Phase 2: the largest-package temp reserve was dropped, so Standard needs about 210 GB.)* Originally required space = packages + the largest package + 5 %, which gave 343 GB for Standard.
- Installed packages are not yet subtracted from the plan (nothing is installable before Phase 3).
- The shipped `full` profile is about 536 GB, short of the nominal 1 TB, because maps, media and AI models arrive in Phases 6-7.
- Legal check pending for non-Wikimedia sources before they go into the catalog: Project Gutenberg is public domain in the US with a trademark policy (`LicenseRef-Public-Domain-US`), and Stack Exchange content is CC BY-SA with mixed versions for older posts. Check these before any release.
- Non-Kiwix manifest types (`files`, `container`, `apt`, `model`) are validated and tested with fixtures, but the shipped catalog has none yet, since no verified URLs and checksums exist.

## Phase 2 - Download engine (done)
- **Engine** (`src/download/`): parallel range downloads with N worker threads into a pre-allocated `.part` file; per-chunk state file written atomically after the data is `fsync`ed; resume only when name, size, checksum and file length all match; per-chunk retry with exponential backoff and jitter (honors `Retry-After`); mirror fallback (two strikes, then next source; sources that 404, ignore `Range` or report a different size are dropped); shared bandwidth limiter; exclusive `flock` per target; SHA-256 verification and atomic rename; idempotent re-runs.
- **CLI**: `kit download --profile <id> | --select a,b [--data-dir] [--jobs] [--limit-rate 5M] [--chunk-size] [--retries]`. Disk preflight credits what is already on disk and refuses with exit code 3 before fetching anything.
- **Tests**: 41 unit tests (state, rate limiter, engine with an in-memory fake), 11 integration tests against a real local HTTP server with injectable faults (resets, 503, corruption, no Range support, wrong size, dead/unreachable mirror, bandwidth), and 5 tests on the real binary (`kill -9` then resume with exactly the missing chunks requested, repeated kills, outage during a run, second process refused, disk refusal).
- **Acceptance**: `test/accept_5gb.sh` (manual) passed on 2026-10-30: 5.4 GB (5,368,709,120 bytes) from a throttled local server (about 28 MB/s) that also cut every 7th request mid-body and went down for 4 s every 40 s. Three `kill -9`s landed at 12 %, 24 % and 32 % of the chunks; the final run resumed after 1.7 GB, finished and the SHA-256 matched. The script asserts that each kill really interrupts a download. Smoke test against a real Kiwix mirror: ArchWiki (38 MB) downloaded through the `lb.download.kiwix.org` redirect in 1.5 s, checksum identical to the catalog, second run a no-op.

### Changes to Phase 1 behavior
- `kit plan` no longer adds a temp-space reserve (see `docs/decisions.md`, P2 deviations 2). Plain `http://` is allowed for loopback only (deviation 3).

### Caveats
- A real `kill -9` or power loss can lose at most the chunks in flight. A power loss on a filesystem that reorders writes is covered by `fsync` before each state update, but not tested on real hardware.
- While writing the acceptance script, a first version killed a shell-function wrapper instead of `kit`, leaving orphans that finished the download, so it "passed" without testing anything. The script now runs `kit` directly and fails if the final file exists after a kill.
- The 5 GB test uses a local fake server because real network loss is not reproducible; the real-mirror smoke test covers redirects and TLS.
- Several simultaneous `kit download` runs for *different* files are fine; for the same file the second is refused.

## Phase 3 - Package installers, ZIM first (done on ArchWiki; Wikipedia DE pending)
- **Installer interface** (`src/install/`): `Installer { plan, install, verify, uninstall }` looked up by package type; `zim` implemented, other types fail early with "cannot be installed yet". Host access (commands, units, tool lookup, HTTP probe) is injected.
- **State and reconcile**: `<data>/state/installed.json` (locked, atomic, never silently reset when corrupt) is the source of truth. `library.xml` is rebuilt from scratch with `kiwix-manage`, the systemd user unit `kit-kiwix.service` (`127.0.0.1:6589`, `--monitorLibrary` when supported, else a restart) is written only if changed, and a static landing page is generated in `<data>/portal/`. Removing the last package stops, disables and deletes the unit, the library file and the portal.
- **CLI**: `kit install --profile|--select [--port] [--no-download] [--dry-run]`, `kit uninstall <id>... [--purge]`, `kit verify [<id>...] [--deep]` (file, size, optional SHA-256, library entry, unit, service active, and the server's own book count), `kit status`.
- **Tests**: 50 new unit and CLI tests with fakes (the library suite grew from 117 to 167); one opt-in end-to-end test for real kiwix-tools. No test touches systemd, `~/.config` or the network (checked: no unit and no `~/.local/share/kit` exist after the suite).
- **Real-host acceptance (2026-10-30, kiwix-tools 3.8.2, Arch):** `kit install --select archwiki_en` downloaded, verified, installed and started the unit; `kit verify --deep` passes 5/5, the book is served at `http://localhost:6589/`, a re-run is a no-op. The guessed `kiwix-manage ... add` order, the `kiwix-serve` flags and the `/viewer#<book>` link were all correct; the opt-in e2e test passes. One bug found: the unpaginated `/catalog/v2/entries` has no `totalResults`, so the probe now uses `?count=1`. Still open: `wikipedia_de_nopic` (18.6 GB) and old Debian kiwix-tools.

### Caveats
- Starting at boot or after logout needs `loginctl enable-linger` (root). `kit install` prints this hint when lingering is off; it is never done silently.
- Debian's `kiwix-tools` may be older than Arch's; if it lacks `--monitorLibrary`, kit restarts the service instead (implemented and tested with a fake, not with the real old version).

## Phase 4 - Local domains over HTTPS (implemented; real-browser acceptance pending)
- **Manifests:** new `mirror_domains` (exact hosts, validated); set for the Wikimedia ZIMs.
- **CA** (`src/netcfg/ca.rs`, `rcgen`): per-machine, key mode 0600 in `<data>/net/ca/`, X.509 name constraints for the catalog's mirrored hosts, leaf re-issued when hosts change or expiry is near. Verified with a real verifier: a certificate for `example.com` or a sibling host signed with the CA's own key is rejected (`tests/net_ca.rs`).
- **Pieces:** marked `/etc/hosts` block (exact removal), nftables redirect 443 -> 8443 in its own table, Caddyfile + `kit-caddy.service` user unit, system store (`trust anchor` / `update-ca-certificates`) and NSS (`certutil`) trust, all behind `CommandRunner` with separate unprivileged and root runners.
- **CLI:** `kit net enable [--dry-run] [--renew-ca] | disable [--remove-ca] | status | check [--samples N]`. State of what was done is in `<data>/net/state.json`, so `disable` undoes exactly that.
- **Tests:** 30+ new unit tests (hosts, nft, Caddyfile golden, trust commands, enable/disable/status idempotency, privilege separation, link checker), one OpenSSL test, one opt-in end-to-end test with real Caddy and kiwix-serve. No test touches `/etc`, nftables, systemd, certificate stores or real browser profiles.
- **Still to do for acceptance (needs sudo and Firefox):** run `test/accept_net.sh` (enable, `kit net check`, Firefox with the cable unplugged on `https://de.wikipedia.org/wiki/Berlin`, disable and confirm hosts/nft/NSS/trust are clean).

### Caveats
- The Caddyfile was validated against Caddy 2.11.7 via `tests/e2e_net.rs` (first run fixed the search rewrite: `uri query >a b` is not valid, kit now sets `pattern` from `{query.search}` and drops `search`).
- Chromium is not installed here; only Firefox/`~/.pki/nssdb` can be verified. Flatpak/Snap browsers keep their own store and are not covered.
- The 443 redirect is gone after a reboot (`kit net enable` restores it).
