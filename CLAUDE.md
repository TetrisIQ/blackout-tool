# Emergency Offline Kit (`kit`)

Linux tool that downloads and installs offline knowledge and entertainment so a laptop keeps working as "the internet, read-only". Read `ROADMAP.md` for the phases and `docs/decisions.md` for the confirmed technical choices. Work phase by phase; do not start a phase before the previous one's acceptance criteria pass.

## Commands
- `make test` - unit and integration tests (no network, no host changes)
- `make lint` - `cargo fmt --check` + `cargo clippy -D warnings`
- `make build` - release binary at `target/release/kit`
- `make build-gui` - also builds the native download manager `target/release/kit-gui` (feature `gui`; run it next to `kit`)
- `test/accept_5gb.sh` - manual Phase 2 acceptance (5 GB, kill -9, outages; needs 5+ GB free in `$TMPDIR`, which should not be a tmpfs)
- `KIT_E2E_ZIM=/path/small.zim cargo test --test e2e_kiwix -- --ignored` - opt-in end-to-end test with real kiwix-tools (no systemd involved)
- `test/accept_net.sh` - manual Phase 4 acceptance (needs caddy, nftables, sudo; changes hosts/nft/trust and undoes them)
- `KIT_E2E_ZIM=/path/small.zim cargo test --test e2e_net -- --ignored` - opt-in end-to-end test with real caddy and kiwix-serve (no host changes)
- `cargo run -- catalog add main <github-repo-url or url-of-catalog.tar.gz>` then `cargo run -- plan --profile mini` - try the CLI (needs network or a warm cache; `--offline` for cache only). Without `catalog add` the default catalog (`catalog::remote::DEFAULT_URL`) is fetched on first use. `--catalog <dir>` / `KIT_CATALOG` uses one local catalog directory alone (e.g. a checkout of the catalog repository, or `tests/fixtures/catalog_shipped`)

## Rules
1. Every system change (hosts, DNS, certs, services) must be **reversible** and marked; provide `disable` and `uninstall`.
2. Run unprivileged by default; request root only for specific steps (hosts/DNS/cert trust/hotspot) and explain why.
3. Never modify the host system in tests; use containers/VMs/temp dirs (`tempfile`).
4. All installers are **idempotent**; re-running must be safe.
5. No hardcoded sizes or URLs for Kiwix content; read from the catalog (manifest validation rejects them).
6. Every content package declares a license; reject packages without one. No copyrighted media.
7. Verify checksums before installing anything.
8. Every phase ends with tests plus a short entry in `docs/progress.md`.
9. Prefer small, reviewable commits per task.
10. **CLI and GUI have the same feature set.** Every user-facing feature must be available in both `kit` and `kit-gui`. Add it to both in the same task (the GUI runs the `kit` child process, so the CLI stays the one implementation); a feature is not done until both exist, or the missing side is recorded as a gap in `docs/progress.md`.

## Conventions
- Rust, edition 2024, library crate `emergency_kit` plus thin binary `kit` (`src/main.rs`). Logic lives in the library so it is testable.
- Network and disk access go through traits (`catalog::http::Http`, `diskspace::FreeSpace`); tests inject fakes. Kiwix fixtures in `tests/fixtures/kiwix/` are recorded from the live service. Re-record them if the catalog format changes.
- Catalogs live in their own repository (`../katalog`, sibling folder of this one). Manifests: `<category>/<id>.yaml`, file name = id, unknown keys rejected. Profiles: `profiles/<id>.yaml`. `catalog.yaml` at the top is the catalog's metadata (name, format), not a manifest. `Catalog::load` reports all problems of one directory at once; `Catalog::load_all` merges several in priority order (first wins, hidden ids become warnings, `depends_on`/profiles are checked after the merge). `tests/fixtures/catalog_shipped` is a small stand-in for the real catalog (`fixture_catalog_is_valid`); the real one is checked by `kit catalog check .` in its own CI.
- Catalog sources (`catalog::remote`, `cli_catalog`, `kit catalog add|remove|update|list|enable|disable|move|check`): the list is `<config>/kit/catalogs.json` (order = priority); each catalog is a GitHub repository URL (`https://github.com/<owner>/<repo>`, resolved by `remote::archive_url` to `releases/latest/download/catalog.tar.gz`) or a direct `.tar.gz` URL, plus `<archive-url>.sha256` (or a pin from `--sha256`), unpacked to `<data>/catalogs/<name>/` only after the checksum and a test load pass (the old copy stays on any failure; plain files only, no paths outside the folder). `kit` never updates silently, only `kit catalog update` (a missing catalog is fetched once when online). Fetching uses `Env.probe` (uncached `Http::get_file`). In `kit-gui` the sidebar entry **Catalogs…** runs the same commands.
- Exit codes of `kit`: 0 ok, 1 error, 3 `kit plan` / `kit download` refused for insufficient disk space.
- Downloads: the engine (`download::download`) only talks to the `RangeSource` and `Progress` traits; unit tests use an in-memory fake, integration tests use `tests/common/server.rs` (a real localhost HTTP server with fault injection, also used by `examples/fake_server.rs`).
- Sizes are decimal (1 GB = 10^9 bytes).
- Installers: all host access goes through `install::Ctx` (command runner, unit dir, tool search path, HTTP probe). Tests build a `Ctx` from temp dirs and `FakeRunner`; never call `systemctl` or write to `~/.config` in tests. State is in `<data-dir>/state/installed.json`; library XML, unit and portal are derived from it by `install::reconcile`.
- Network layer (`netcfg`): host access goes through `NetCtx` (an unprivileged and a root `CommandRunner`, hosts path, unit dir, home); tests use temp dirs and `FakeRunner`. Only the root runner may receive `cp /etc/hosts`, `nft`, `trust`. Don't spawn processes in library unit tests (inherited `flock`s); use `tests/`.
- Flatpak games (`install::flatpak`): always `--user`; tests use `FakeRunner`. A manual check against the real Flathub must set `FLATPAK_USER_DIR` to a temp dir.
- Native download manager (`src/ui`, binary `kit-gui`, egui): `ui::model` (selection, diff, sizes) and `ui::backend` (catalog, installed state, `Runner`) are toolkit-free and tested; only `ui::app` / `ui::theme` need the `gui` feature. Installs and removals are child `kit` processes (pause = kill, the engine resumes from the `.part` state); progress is read from `downloads/<id>/`. Tests use a fake `kit` shell script (`tests/ui_runner.rs`), never a real install. `KIT_DATA_DIR` points the window at a scratch data dir, `KIT_CATALOG` at one catalog directory; `KIT_GUI_SHOT=file.ppm` (+ `KIT_GUI_TAB=downloads|installed|updates|services|wizard|location|archive|catalogs`) saves a screenshot and quits.
- Website archives (`archive`, `kit archive`): `zimit` in rootless Podman crawls a URL prefix into a ZIM, installed through `ZimInstaller`; a license is required. In the window it is a queue job (`Runner::enqueue_archive`) that runs the same `kit archive`.
- Data location (`datadir`, `install::relocate`): order is `--data-dir` / `KIT_DATA_DIR`, then `<config>/kit/config.json` (written by `kit data-dir set`), then `$XDG_DATA_HOME/kit`. `kit data-dir set <path> --move` stops our units, renames (same filesystem) or copies (hard links kept, old tree removed last), rewrites the old path in kit-marked units, Quadlet files and the Caddyfile plus `FileRecord.source`, saves the new place and runs `reconcile`. The target must be an empty or new absolute path without spaces or special characters (the rewrite is plain text; use a symlink for such disks). Tests use temp dirs; `Env.config_dir` keeps the real `~/.config/kit` out of tests.
- Maps (`install::portal` viewer, `mapsearch`, `zimfile`): maps are ordinary `zim` packages of the category `maps` (Kiwix `maps_en_<country>`); the viewer under `/maps/` reads tiles and style from `/content/<book>/`. `kit install` builds the street/place search index of each map into `<data>/maps/<book>.search/` (`kit maps index` repeats it); `mapsearch::norm` is mirrored in `assets/maps/viewer.html`, change both together. `tests/fixtures/maps/mini.zim` is a real 5-tile extract.
- Keep the placeholder module `services` empty until its phase.
