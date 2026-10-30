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
