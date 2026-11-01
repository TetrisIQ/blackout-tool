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

## Phase 5 - Portal and unified search (implemented; acceptance together with Phase 4 pending)
- **Portal:** `https://kit.local` with a search box over all installed libraries (kiwix-serve's cross-book `/search`), emergency quick searches (EN/DE), emergency numbers and a tile per installed package; `http://kit.local` redirects to HTTPS. Pages are generated from the state into `<data>/portal/` (`index.html` for files, `kit.html` for the front).
- **Change to Phase 4:** mirrored names now resolve to `127.77.0.1`, and the nft table redirects ports 80 and 443 for that address only, so no local dev server is hijacked. Existing Phase 4 setups: `kit net disable`, then `kit net enable --renew-ca`.
- **Tests:** portal rendering (relative links, escaping, URL encoding), Caddyfile and nft goldens, enable/disable with the portal host; the opt-in e2e test passes with real Caddy 2.11.7 and kiwix-serve: portal page from disk, search across books through `kit.local`, http -> https redirect.
- **Acceptance (manual, needs sudo and a browser):** `test/accept_net.sh`, then open `http://kit.local`, search a term that exists in two installed ZIMs and expect hits from both; the "one search box finds results from all installed ZIMs" criterion was only verified with a single ZIM so far.

## Phase 6 - Service packages, first slice (implemented; real-host acceptance pending)
- **Framework:** manifest blocks `service` (host, port, health path, env, mounts, provisioner) and `content` (media kind, zip/none); `container` installer (digest-pinned image, Quadlet file `kit-<id>.container`, loopback-only port, health wait, first-run provisioning), `files` media installer (atomic zip unpack, zip-slip and size guards); `kit install` now installs packages that need no download; `kit status` shows services and their login; Caddy vhost and portal tile per service; the CA/hosts/leaf include service hosts.
- **Packages:** `navidrome` (GPL-3.0, image `deluan/navidrome:0.64.2` by digest, mounts `/music` read-only from `<data>/media/music`) and `music_musopen_dvd` (Musopen DVD, 2.4 GB zip, 145 MP3 files, Public Domain Mark; sha256 computed from a full download and the SHA-1 matches archive.org's metadata).
- **Verified for real, without touching the host's config:** Navidrome 0.64.2 run with `podman run` and the manifest's environment (`/ping` 200, `createAdmin` 200 then 403, scanner schedule applied); Podman's own `quadlet -dryrun` accepts the generated file (`tests/quadlet.rs`); the real Musopen zip unpacks through `kit install --select music_musopen_dvd` in a temp data dir in 15 s with the checksum verified. The test image was removed again.
- **Tests:** 14 new unit tests for the installers, one CLI round trip (install, status with login, no-op re-run, uninstall), manifest validation, Caddy golden for services.
- **Acceptance (manual, `test/accept_services.sh`):** install both packages for real, `kit net enable --renew-ca`, open `https://navidrome.kit.local`, log in, play a track offline. Not done: this writes `kit-navidrome.service` into the user's systemd.

### Caveats
- The Musopen zip contains macOS `__MACOSX/` and `.DS_Store` files; Navidrome should ignore them but this is unverified.
- The licence of the pack is the Public Domain Mark claimed on archive.org for Musopen's own release; a human legal review is still open (roadmap risk).
- Audiobookshelf, Jellyfin and Kavita/Calibre-Web, further content packs (LibriVox, Blender Open Movies, Standard Ebooks, games) and the linger hint are the next slices.

## Phase 6 - second slice: Audiobookshelf + LibriVox (implemented; real-host acceptance pending)
- **Packages:** `audiobookshelf` (GPL-3.0-only, `ghcr.io/advplyr/audiobookshelf:2.37.2` by digest, port 13378, `https://audiobookshelf.kit.local`, books mounted read-only from `<data>/media/audiobooks`) and `librivox_alice` (LibriVox recording of *Alice in Wonderland*, 12 MP3 chapters, 86 MB, sha256 computed from a full download, SHA-1 matches archive.org).
- **Provisioner:** `POST /init` creates the root user (`admin`, random password in `credentials.json`, shown by `kit status`), then `/login`, `POST /api/libraries` per media mount, then a library scan. The scan is needed: the folder watcher only sees changes made after the library exists, so books unpacked earlier were not listed (found on the real image, item count 0 until a scan ran).
- **Framework change:** `Http::post_json_bearer` (needed for the authenticated calls).
- **Verified for real, without touching the host's config:** the pinned image via `podman run` with the manifest's environment (`/healthcheck` 200, `/init` 200, library create 200, scan lists "Alice in Wonderland"); Quadlet dry run (`tests/quadlet.rs`); the real zip installs through `kit install` into a temp data dir. Image and container were removed again.
- **Tests:** 225 unit tests (new: login, library and scan sequence with a fake HTTP client), `shipped_catalog_is_valid` covers both manifests.
- **Acceptance (manual):** `test/accept_services.sh` now installs both; open `https://audiobookshelf.kit.local` and play a chapter offline.

### Caveats
- The public-domain status of LibriVox recordings (and of the source texts in every jurisdiction) needs a human legal review before shipping more titles.
- `size_bytes` of the image (160 MB) is an estimate for the disk-space plan.

## Phase 6 - third slice: Jellyfin + Kavita (implemented; real-host acceptance pending)
- **Packages:** `jellyfin` (GPL-2.0-only, `jellyfin/jellyfin:12.4.20261026-211555` by digest, port 8096) with `blender_big_buck_bunny` (480p, CC-BY-3.0, 249 MB, sha256 from a full download); `kavita` (GPL-3.0-only, `kareadita/kavita:0.9.1` by digest, port 5000) with `standardebooks_alice` (Standard Ebooks EPUB, CC0, 18 MB, sha256 from a full download).
- **Provisioners:** Jellyfin: the setup wizard (`/Startup/*`), login with a `MediaBrowser` authorization header, `POST /Library/VirtualFolders` with internet metadata providers off and `refreshLibrary=true`. Kavita: `/api/Account/register` (open while no user exists, returns the token), `POST /api/Library/create` (type 2 = book; the scan starts on its own).
- **Framework change:** `Http::post_json_auth` takes a whole `Authorization` value (replaces `post_json_bearer`); Jellyfin rejects `Bearer`.
- **Verified for real (podman, temp dirs, no host config):** both images with the exact API calls the provisioners make: Jellyfin lists "Big Buck Bunny" about 10 s after library creation; Kavita lists the Alice series. `kit install --select standardebooks_alice` unpacks to `media/books/`. Images and containers were removed again.
- **Findings from the real images:** Kavita rejects a library without `fileGroupTypes`, `excludePatterns` and `metadataProvider`, and ignores files unless `enableMetadata` is true ("metadata turned off", then "Unable to parse"). Jellyfin ignores `Authorization: Bearer`.
- **Tests:** 227 unit tests (new: both provisioners), `shipped_catalog_is_valid`, Quadlet dry run.

### Caveats
- Kavita names the series after the Standard Ebooks collection ("The Guardian's 100 Greatest Novels"), not the book title; the book is searchable by title.
- The Standard Ebooks download URL is unversioned: a new release of the book changes the file and the checksum check fails until the manifest is updated.
- Image digests are those of the tags as pulled on amd64; `size_bytes` of the images are estimates.
- Blender's films are CC-BY 3.0; the attribution is in the manifest. The legal review of all content sources is still open.
- Not verified: HTTPS front (`kit net enable --renew-ca`) with Jellyfin's and Kavita's websockets, playback in a browser.

## Test profile
`catalog/profiles/test.yaml` (`kit plan --profile test`, `kit install --profile test`): all four services plus one small pack each, two small ZIMs (Arch Wiki, Wikivoyage) for the cross-library search. 4.3 GB in total, of which Musopen is 2.4 GB (the only free music pack so far).
