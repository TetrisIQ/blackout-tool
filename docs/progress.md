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

## Fix: Jellyfin and Kavita first-run setup on a real host
Found by the first real install of the test profile: no login, no library.
- **Cause 1:** the health wait was 3 s for containers; Jellyfin and Kavita need 10-30 s on first start (migrations). The install failed after the state was written, and a re-run saw the package as current and never provisioned. Now: 180 s wait for containers (`START_WAIT`), and a service with a provisioner but no `credentials.json` counts as not current, so the next `kit install` retries.
- **Cause 2 (Jellyfin):** `/health` answers 200 while the start-up page is still up; the wizard POSTs go to that page and are dropped. Now kit waits until `/System/Info/Public` reports `StartupWizardCompleted`, and leaves a wizard that is already completed alone. The wizard also needs a `GET /Startup/User` before the `POST` (404 otherwise).
- **Verified for real** (podman, temp dirs): fresh Jellyfin image, the full call sequence incl. library and scan lists the film.
- **Not covered by the first slice's tests:** timing against real images. Unit tests use a fake HTTP client with no waits.
- **Recovering an install that was set up by hand:** `kit uninstall jellyfin kavita --purge`, then `kit install --profile test`.

## Fix: Jellyfin library stayed empty after provisioning
- **Cause (from the Jellyfin log of a real install):** `Startup/Complete` makes Jellyfin validate its still empty library ("Validating media library"). The scan that `refreshLibrary=true` starts while the library is created runs at the same moment and is dropped, so the film never appeared until a manual scan. A fresh container in four parallel runs could not reproduce it; it depends on timing.
- **Fix:** after creating the library, kit asks for a scan (`POST /Library/Refresh`) and checks `/Items` until a film is listed (up to 10 tries, 3 s apart; only when a video pack is installed), and says so if it never shows. `Http::get_auth` was added for the check.

## Phase 6 - fourth slice: more content, linger hint, test profile
- **Linger hint:** container services print `sudo loginctl enable-linger <user>` when linger is off (shared helper with the Kiwix service).
- **New packs:** `blender_tears_of_steel` (CC-BY-3.0, 372 MB) and `blender_sintel` (CC-BY-3.0, 674 MB) for Jellyfin; `music_macleod_local_forecast` and `music_macleod_sneaky_snitch` (Kevin MacLeod, CC-BY-4.0, 5-8 MB each, from the artist's own site) for Navidrome. All checksums from full downloads.
- **Test profile** now uses the two MacLeod tracks instead of the 2.4 GB Musopen pack: 1.9 GB in total.
- **Download file names** are percent-decoded (`%20` -> space); a decoded `/` is still refused.
- **Not done, on purpose:** Internet Archive films and Jamendo/FMA. IA items carry licence claims set by the uploaders (the "free" Kevin MacLeod items there say BY-ND or nothing), and the one Chaplin short checked had no video file. Jamendo needs an API key and FMA has no bulk download. These need a human legal review and a vetted list before they ship.
- **Phase 6b (games)** is now its own phase in `ROADMAP.md`.

## Phase 6b - Games as Flatpaks, grouped on the portal
- **New package type `flatpak`** (`source.flatpak: <app id>`, remote fixed to Flathub, `group:` required, `size_bytes` estimated). Groups (fixed list, `APP_GROUPS`): arcade, strategy, puzzle, sandbox, racing, board. No `url`/`sha256`: Flatpak verifies Flathub's signed commits itself.
- **13 games in `catalog/games/`**, all FOSS with the licence from the Flathub appstream data: Luanti (sandbox); OpenTTD, Freeciv, 0 A.D., Battle for Wesnoth (strategy); SuperTux, Frozen Bubble, Quadrapassel (arcade); SuperTuxKart (racing); GNOME Sudoku, Mines (puzzle); GNOME Chess, Aisleriot (board). `size_bytes` is the app's installed size without runtimes.
- **Installer** (`install::flatpak`): adds the `flathub` remote to the *user* installation only if it is missing there, `flatpak install --user --noninteractive`, records `app` (id, group, `by_kit`) in `installed.json`. Uninstall removes only what kit installed (`--purge` also deletes saved games); runtimes stay, with the command to clean them. Idempotent; a game that was there before is left alone.
- **Portal:** section "Games and programs" with one list per group, licence and the command `flatpak run <id>`. A browser cannot start programs, so the desktop menu entry (exported by Flatpak) is the launcher; the portal names the command.
- **Profiles:** `test` gets Luanti and Mines, `standard` the small games (no 0 A.D./SuperTuxKart/Wesnoth), `full` all of them.
- **Verified for real** (isolated `FLATPAK_USER_DIR`, temp data dir; nothing on the host touched): `kit install --select gnome_mines` adds the remote, installs the app and its runtime, exports `org.gnome.Mines.desktop`; `verify` passes; the portal lists it; `uninstall` removes it and the portal. **Found there:** `flatpak remotes` without `--user` also lists a system-wide `flathub`, which cannot serve `--user` installs ("No remote refs found"); kit now looks at the user installation only.
- **Tests:** 239 unit tests (new: manifest rules, installer against `FakeRunner`, portal groups); `shipped_catalog_is_valid`.

### Caveats
- The first install needs the internet (app plus runtime). Runtimes are shared and not in `size_bytes`: expect about 0.6 GB (freedesktop) and 0.8 GB (GNOME) once. For an offline-only machine, install while online first.
- Freeciv on Flathub still uses the end-of-life 22.08 runtime. 0 A.D. is 3.7 GB installed; SuperTuxKart 0.8 GB.
- That OpenTTD's Flathub build bundles OpenGFX/OpenSFX/OpenMSX comes from Flathub's description and was not started here. Not tried: actually starting a game (no display in this session).
- Not pinned to a commit: a game updates when Flathub does and the user runs `flatpak update`. Distro packages are not supported, only Flatpak.
- Licence strings are taken from Flathub's appstream data; the legal review of content sources stays open.

## Phase 7 - first slice: tech documentation (DevDocs ZIMs)
- **New packs** in `catalog/reference/`: `devdocs_man`, `_bash`, `_git`, `_python`, `_rust`, `_html`, `_css`, `_javascript`, `_c` (Kiwix `devdocs_en_*`, plain `zim` packages, sizes from the live catalog; 51 MB together). Added to `standard` and `full`.
- **Checked:** `kit plan --profile standard` resolves all nine against the live catalog; 239 unit tests and `shipped_catalog_is_valid` pass.
- **Caveats:** the DevDocs ZIMs are tagged `_ftindex:no`, so the unified search (Phase 5) finds them by title only. Licences are those of the upstream documentation (man pages are a mix, `Linux-man-pages-copyleft` is the project's umbrella); the DevDocs ZIMs were not opened to confirm per-page notices.
- **Still open in Phase 7:** maps, local LLM (`survival-llm` evaluation, hardware check), Whisper/translation packs, optional apt/pip mirror subset. The "done when" criterion is not met yet.

## Phase 7 - second slice: offline maps (PMTiles + browser viewer)
- **New package type `map`** (`source.map: {bbox: [w, s, e, n], maxzoom}`, estimated `size_bytes`, no `url`/`sha256`). Protomaps publishes only dated ~115 GB planet files, so kit cuts the region out with `pmtiles extract` from the newest build (`build-metadata.protomaps.dev/builds.json`, range requests: only the region is transferred), runs `pmtiles verify`, and records its own SHA-256 (`kit verify --deep` compares it). **Documented exception to rule 7:** there is no upstream checksum for a derived region.
- **Installer** `install::map`: needs `pmtiles` on PATH (go-pmtiles; the missing-tool error says where to get it), writes `<data-dir>/maps/<id>.pmtiles` via a `.part` file, idempotent, a failed extract/verify leaves no file and no record. Uninstall removes the file. Updating to a newer build is Phase 10.
- **Viewer:** Leaflet 1.9.4 + protomaps-leaflet 5.1.0 vendored in `assets/maps/` (BSD, no CDN, vector tiles drawn on canvas with system fonts, so no glyph/sprite files). `portal::write` puts it under `<portal>/maps/` with `regions.json` and a symlink per region; Caddy's `file_server` already serves the portal directory with range requests, so the Caddyfile did not change. The portal lists a "Maps" section; opened as a file it says to run `kit net enable`, because a `file://` page cannot do range requests.
- **Catalog:** `map_germany` (zoom 13) in `standard` and `full`, `map_berlin` (zoom 14) in `test`.
- **Tests:** 269 pass (new: manifest rules, installer against `FakeRunner`/`FakeHttp` incl. failed verify, offline, missing tool, verify/uninstall; portal section and `regions.json`).

### Caveats
- **Sizes measured** (go-pmtiles 1.31.2, build 20261102, `extract --dry-run`): Berlin zoom 14 is 31 MB (was guessed 200 MB), Germany zoom 13 is 1.8 GB (was guessed 2.5 GB); manifests corrected. Sizes grow with newer builds, so they stay estimates.
- **Checked for real:** `pmtiles extract` of Berlin against the live build plus `pmtiles verify`; `kit install --select map_berlin` with temp data/XDG dirs; the viewer in headless Firefox served by `caddy file-server` (206 Partial Content, symlink, roads, water, labels and attribution render). Not checked: Chromium, the full path through `kit net enable` (HTTPS), the Germany extract, geolocation.
- **The viewer shows no routing**, only panning, zoom, region switch, the place search (next section) and the browser's geolocation.
- Still open in Phase 7: local LLM (`survival-llm` is a desktop app around Ollama with its own library, not a model; plan is Ollama/llama.cpp as a service plus Kiwix search), hardware check, Whisper/translation, apt/pip mirror subset.

## Phase 7 - third slice: first local model (Granite 4.2 3B through Ollama)
- **Scope on purpose small:** one model, no automatic choice yet. `granite4.2:3b` is the smallest candidate that is not a toy: 2.2 GB, 128K context, tool use, Apache-2.0 (licence text read from the registry's licence layer).
- **New package type use `model`** (`source.ollama: {model, tag}`, no `url`/`sha256`/`size_bytes`): `LiveResolver` reads `registry.ollama.ai/v2/library/<model>/manifests/<tag>` for the exact size (config + layers); the version shown is the first 12 hex of the weights layer. Same idea as the Kiwix catalog (rule 5).
- **Installer** `install::model` uses a standard Ollama and does not install or run it: needs `ollama` on PATH and a server on `127.0.0.1:11434` (clear errors with the start commands otherwise), then `ollama pull <name>`; Ollama checks every layer's SHA-256 against the registry manifest (rule 7, like Flatpak). Records `ModelRecord {name, size, by_kit}`; a model that was already there is never removed (`by_kit: false`); uninstall runs `ollama rm` only for kit's own pulls. Idempotent.
- **Catalog:** `llm_granite_3b` in `test`, `standard` and `full`. `kit plan --profile test` resolves it live (2.2 GB).
- **Tests:** 275 pass, lint clean (new: manifest rules, registry resolver, installer against a fake Ollama server that reflects pulls/removals).
- **Try it:** start Ollama, `kit install --select llm_granite_3b`, then `ollama run granite4.2:3b`.

### Caveats
- **Real run (Ollama 0.40.13, 8 GB GPU, temp data dir):** `kit install --select llm_granite_3b` pulled the model in 24 s, `kit verify` passed, `ollama list` shows it. Not yet run for real: `kit uninstall` (`ollama rm`).
- `ollama pull` output is captured, so a 2.2 GB pull shows no progress bar in `kit install`.
- **Safety:** Survival LLM's own evaluation found Granite 4.2 3B gave harmful first-aid answers (5 of 22). It is a speed-first starting point, not a recommendation; the evaluation gate and grounding in the Kiwix library (plan slices 5 and 6) come before it ships in a profile as the default. Meanwhile it is in `standard` and `full` so it can be tested.
- Not done yet from the plan: engine tarball fallback (see the fifth slice for the rest).

### First real answers (granite4.2:3b, 8 GB GPU, 100 % on GPU, 4096 context)
- Speed: 88 tok/s; a 165-token answer took 3.2 s (1.1 s of it loading), a 1,061-token answer 12.2 s.
- Quality (2 questions, not an evaluation): the burn answer was short and mostly right but advises antibiotic ointment, which first-aid guidance usually leaves out for fresh burns. "How far from a downed power line?" gave no distance at all and a long hedging answer. Both are the kind of gap that grounding in the library and the evaluation gate have to catch.
- The `ID` in `ollama list` (`40577dc168a3`) is not the version kit shows (`20e436143017`, the weights layer's digest prefix); they are different digests.

## Phase 7 - fourth slice: grounded assistant page (`/ai/`)
- **Why:** the first real answers (see above) showed that the 3B model must not answer from memory. The page answers only from library passages and shows which ones.
- **Page** (`assets/ai/chat.html` + `rag.js`, no dependencies, written by `portal::write` to `<portal>/ai/` only while a model is installed, with `config.json` naming the model): the model first turns the question into English search keywords (works for German questions over English ZIMs), Kiwix `search?format=xml` finds articles, `dropVariants` removes translated copies of a page, the best paragraphs of the top 4 are cut out, passages that mention less than half of the keywords are dropped. **No relevant passage means no model call:** the page says nothing was found. Otherwise the model gets the numbered passages with an "answer only from these, cite [n], never invent numbers, say so if they do not answer" prompt (`think: false`, `num_ctx` 8192, 700 tokens max). The sources shown under an answer are the passages that were really retrieved, not what the model claims.
- **Front:** `kit net enable` adds (when a model is installed) a Caddy rule proxying exactly `/ai/api/chat` and `/ai/api/tags` to Ollama on 127.0.0.1:11434 (path becomes `/api/...`, `Origin` dropped, unbuffered streaming); `/ai/api/*` otherwise answers 403, so a phone on the hotspot cannot pull or delete models.
- **Tests:** `node test/ai_rag.test.mjs` (pure logic, no network); Rust unit tests for the Caddy rule and the portal files; opt-in `KIT_E2E_ZIM=... cargo test --test e2e_ai -- --ignored` with real Caddy + kiwix-serve + Ollama (page, search XML, tags 200, pull/delete 403, chat 200). It found a real bug (`strip_prefix` had cut `/api`). 277 unit/integration tests pass.
- **Real runs** (granite4.2:3b, ArchWiki ZIM, 8 GB GPU): grounded and good for "update the mirror list" (reflector command from the article, 6.6 s). German "Wie installiere ich ein Paket mit pacman?": correct keywords, but the answer was in English and invented `pacman -S --all`. The first version produced empty answers because the model's hidden thinking used the whole token budget (fixed with `think: false`).

### Caveats
- **Not run in a browser.** The JS logic is tested in node and the plumbing through real Caddy with curl; the page itself (rendering, streaming display) has not been opened.
- **A 3B model still invents details even with passages**, and ignores the "language of the question" rule at times. The page cannot fix that; a larger model (e.g. `qwen3.5:4b` / `9b`) and the evaluation gate are the next steps. Relevance filtering is by term overlap only.
- Library content is whatever ZIMs are installed (the test used ArchWiki only), so first-aid questions return "nothing found" there; the Standard profile has the medical/survival sources.
- Needs `kit net enable` (root steps) to be reachable from a browser; the `file://` portal only names `ollama run`.

## Phase 7 - fifth slice: `kit hw`, automatic model choice, survival ZIMs, answer timing
- **`kit hw [--json]`** (`src/hw.rs`): reads `/proc/meminfo`, `/proc/cpuinfo`, `nvidia-smi` and AMD cards from `/sys/class/drm` (integrated graphics are not counted); parsers are pure and tested with recorded text. On the dev machine: Ryzen 5 5600, 33.6 GB RAM, **AMD GPU with 8.6 GB** (not NVIDIA).
- **Automatic choice** (`src/ai.rs`, `llm_auto` in `--select` and in the `test`/`standard`/`full` profiles, replaces `llm_granite_3b`): the largest model whose working set (weights x 1.25 + 0.5 GB, sizes live from the registry) fits 95 % of the GPU's memory; without a usable GPU only models up to 3.5 GB (quick on a CPU) that fit 60 % of the free RAM; a bigger one is chosen only as "slow but possible"; otherwise an error naming `--select`. The choice and the reason are printed by `plan`, `download` and `install`. Here: `llm_auto -> llm_qwen35_4b`. Manifests of type `model` now need `model: {context_k, thinking}`; `context_k` below 32 is rejected (a few library passages would not fit).
- **New models:** `llm_qwen35_4b` (3.3 GB) and `llm_qwen35_9b` (6.6 GB), both 256K context, can reason, Apache-2.0 (licence layer read from the registry).
- **Survival ZIMs** (`catalog/survival/`, in `standard` and `full`): WikiMed (no pictures, 1.05 GB), Ready.gov (2.06 GB), NHS medicines (15 MB, OGL-UK-3.0), USDA canning (20 MB), CDC Travelers' Health (178 MB); 3.3 GB, downloaded and verified into a temp dir and served by a scratch kiwix-serve. Left out on purpose: military medicine (reprints copyrighted volumes), MedlinePlus (partly third-party licensed).
- **Page:** shows the time of every answer (total, search, first word, tokens/s; also in the console), has a "Think harder" switch for models that can reason (`config.json` carries `thinking`), searches again with fewer terms when the full keyword list finds nothing (the search needs all terms), and asks for exactly 3-4 English keywords.
- **Found by measuring:** the keyword call used `num_ctx` 2048 and the answer call 8192, so Ollama reloaded the model between them (about 6 s per question). One context size for both removed it.

### Results (survival ZIMs, 8.6 GB AMD GPU, all on the GPU unless noted; same page logic replayed in node)
| | granite4.2:3b | qwen3.5:4b | qwen3.5:9b |
|---|---|---|---|
| speed | 84 tok/s | 28 tok/s | 17 tok/s (17 % on CPU) |
| typical answer time | 7-12 s | 4.6-8 s (first question +6 s model load) | 6-9.5 s |
| answer quality | leaks its reasoning into the answer, rambles | clean, concise, cites passages, German question answered in German | same as 4B, slower |
- Prompt processing of the ~1,300-token context takes 2.5 s on the 4B model and is now the largest fixed cost; fewer or shorter passages would cut it.
- Facts checked against the passages: burn (cool running water 10-20 min was in one run; other runs quoted WikiMed on friction burns, which is the wrong burn), downed power line (all models say the passages give no distance, none invent one), fridge during an outage (four hours, correct), boiling water (1 min, 3 min above 2,000 m, correct).
- **Reasoning mode ("Think harder")** took 45 s and 99 s (1,000 to 2,400 thinking tokens) with no better answer; it stays off by default.

### Caveats
- **Retrieval is the weak point now:** Kiwix search needs every term, and the model's keywords vary. "How do I make water safe to drink in an emergency?" found nothing although Ready.gov has a Water page ("boil water" finds it). The minor-burn question retrieves friction-burn pages instead of the general burn first aid. A better ranking (title match boost, trying alternative keyword sets) is the next step.
- The assistant page itself (rendering, streaming, timing line, "Think harder") has not been opened in a browser; the logic is tested in node and the plumbing with Caddy + curl.
- No evaluation set yet: the questions above are a smoke test of five questions, not a safety evaluation.
- The ZIMs for this test sit in a scratch directory; no installation on the host was changed (`ollama serve` runs as the user, the models live in `~/.ollama`).

## Phase 7 - fourth slice: place search in the map viewer
- **What it finds:** named places (cities, towns, villages, suburbs, neighbourhoods), not streets or house numbers. Protomaps tiles have no search data, but their `places` layer lists every named place as a point. `geocode` reads it from the installed `.pmtiles` (PMTiles v3 reader and a small vector-tile decoder, no tool needed, new dependency `flate2` for gzip) at zoom 12, merges repeats from neighbouring tiles and writes `<data-dir>/maps/<id>.places.json` (`[name, lon, lat, kind, population]`, biggest first). German and English forms are separate entries, so "Munich" finds München.
- **When:** built by `kit install` after the map; a map installed before this slice gets its index on the next `kit install --select <id>` (a map without index is not "current"; that run needs no `pmtiles`). A failed index is only a note, the map keeps working. Uninstall removes it.
- **Viewer:** search box next to the region switch, accent-insensitive, prefix matches first, click or Enter flies to the place. The portal links the index as `/maps/data/<id>.places.json` and lists it in `regions.json`.
- **Measured on real data:** Berlin 1,378 places, 71 KB; Germany 335,900 places, 16 MB, built in 3 s. Searched in the browser with headless Firefox ("kreuzb" finds Kreuzberg and moves the map).
- **Tests:** 270 unit tests (new: Hilbert tile ids from the spec, varints, dedupe, the reader against a 400 KB real extract in `tests/fixtures/maps/mitte.pmtiles`, installer builds/links/removes the index).
- **Caveats:** no street or address search (would need the `roads` layer with names, which is far bigger, or a Nominatim/Photon service as a heavy package); the 16 MB Germany list is parsed in the browser on first use; only checked in Firefox.

## Phase 7 - sixth slice: assistant page checked in a browser, better retrieval
- **Browser check:** the `/ai/` page was driven in headless Firefox (WebDriver BiDi, scratch Caddy in front of the real kiwix-serve and Ollama, qwen3.5:4b, survival ZIMs): it renders, streams the answer, shows the source links and the timing line, and the "Think harder" box appears for a model that can reason. (Only Firefox; the "Think harder" switch itself was not exercised.)
- **Problem found:** Ready.gov holds every page once per language under the same title, so 8 of 8 hits were copies of one page, and the big Wikipedia extract crowded out everything else. "How do I treat a minor burn?" was answered from First aid kit / Friction burn.
- **Fix** (`assets/ai/rag.js`, new `retrieve`, the page just calls it): one hit per title (the original, not the `/ar/`, `/fr/` copy); every book is searched on its own (6 hits each) and at most 2 pages per book are used; all strings of the keyword ladder are searched (full, every list with one keyword left out for up to 4 keywords, first 3, first 2) and everything is ranked against the full keyword string by title and snippet match, the engine's rank breaks ties; the passage filter (at least half of the keywords) is unchanged.
- **Measured (same 4B model):** the burn question now retrieves "Preventing and Treating Burns" (Ready.gov) and answers "hold under cool running water 10 to 15 minutes"; the German water question retrieves Ready.gov Water and CDC Water Disinfection. Search time 3.8 s -> 0.5 s (searches run in parallel, less ladder waiting).
- **Tests:** `node test/ai_rag.test.mjs` (ladder, dedupe, ranking, `retrieve` against a fake library); 270 Rust tests and lint unchanged and green.

### Caveats
- Still weak: questions whose pages use other words (downed power line, earthquake: nothing relevant is found or the passages are off-topic) and an unrelated passage can still pass the half-the-keywords filter (a ketoconazole side-effects page was shown under the burn answer). The next step is the evaluation set (questions with expected facts, and ones that must answer "not found") so these can be measured instead of spot-checked.

## Phase 7 - seventh slice: evaluation set for the assistant, keyword and ranking changes
- **`node test/ai_eval.mjs`** (opt-in; needs the running kiwix-serve and Ollama of a real install, changes nothing): runs the page's own pipeline (keywords, `Rag.retrieve`, answer; prompts and Ollama options are shared through `Rag.OPTIONS`) on the 25 questions in `test/ai_eval/questions.json` and checks three things per question: do the retrieved passages contain the expected facts (regular expressions), does the answer state them without forbidden advice (negated sentences such as "do not apply butter" are skipped), and for 4 questions the library cannot answer, is nothing found or does the answer say so. Options: `--model`, `--only id,id`, `--retrieval-only`, `--show`, `--json file`. Exit code 1 on any failure except cases with a `known` reason (library gaps, listed but not counted). The facts were checked against the installed ZIMs (the retrieval check is literally "the text is in the passage").
- **Retrieval changes made with it:** the keyword prompt asks for the subject named like an article title, without generic words (emergency, treatment, first aid ...), and the same words are dropped from the model's output; the question's own content words are searched as a second source; the first keyword alone is a last search for 3+ keywords; shorter titles rank slightly higher; one failing search (Kiwix answers 500 for odd strings) no longer loses the others.
- **Baseline (survival ZIMs, one run each, answers vary by a question or two between runs):**

| model | all checks pass | retrieval ok | answers ok |
|---|---|---|---|
| qwen3.5:4b | 14 of 25 | 16 | 15 |
| qwen3.5:9b | 16 of 25 | 20 | 16 |

  (granite4.2:3b was started too but most of its run failed with errors, not a quality result; not repeated.)
- **What the failures show:** (1) Retrieval is the main limit: CPR, wound bleeding (answered with *menstrual* bleeding), shock, the German burn/water questions retrieve the wrong pages because Kiwix needs every term and the model's keywords vary. (2) **Wrong-source answers are the real safety problem:** CPR was answered with "AutoPulse runs at 80 compressions per minute", bleeding with tranexamic acid; the passage filter (half of the keywords) does not catch a page about the wrong thing. (3) "How high is Mount Everest" (German) was answered with 8,848 m although no passage says so: the "answer only from the passages" instruction is not airtight with a small model. (4) When the library lacks the fact, both models mostly say so (freezer 48 hours, downed power line distance): that is the wanted behaviour.
- **Library gaps found:** the USDA canning ZIM has no searchable text (home page only), so it adds nothing to the assistant; "diarrhoea" does not match the CDC's "diarrhea" (no spelling normalisation).
- **Caveat on the numbers:** the retrieval changes were tuned while looking at these 25 questions, so they are optimistic for new questions; the set needs to grow (and a held-out part be kept) before the numbers are used as a release gate. Nothing here is a clinical evaluation.
