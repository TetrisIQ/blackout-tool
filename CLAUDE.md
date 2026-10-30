# Emergency Offline Kit (`kit`)

Linux tool that downloads and installs offline knowledge and entertainment so a laptop keeps working as "the internet, read-only". Read `ROADMAP.md` for the phases and `docs/decisions.md` for the confirmed technical choices. Work phase by phase; do not start a phase before the previous one's acceptance criteria pass.

## Commands
- `make test` - unit and integration tests (no network, no host changes)
- `make lint` - `cargo fmt --check` + `cargo clippy -D warnings`
- `make build` - release binary at `target/release/kit`
- `test/accept_5gb.sh` - manual Phase 2 acceptance (5 GB, kill -9, outages; needs 5+ GB free in `$TMPDIR`, which should not be a tmpfs)
- `cargo run -- plan --profile mini` - try the CLI (needs network or a warm cache; `--offline` for cache only)

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

## Conventions
- Rust, edition 2024, library crate `emergency_kit` plus thin binary `kit` (`src/main.rs`). Logic lives in the library so it is testable.
- Network and disk access go through traits (`catalog::http::Http`, `diskspace::FreeSpace`); tests inject fakes. Kiwix fixtures in `tests/fixtures/kiwix/` are recorded from the live service. Re-record them if the catalog format changes.
- Manifests: `catalog/<category>/<id>.yaml`, file name = id, unknown keys rejected. Profiles: `catalog/profiles/<id>.yaml`. `Catalog::load` reports all problems at once; `shipped_catalog_is_valid` keeps the shipped catalog honest.
- Exit codes of `kit`: 0 ok, 1 error, 3 `kit plan` / `kit download` refused for insufficient disk space.
- Downloads: the engine (`download::download`) only talks to the `RangeSource` and `Progress` traits; unit tests use an in-memory fake, integration tests use `tests/common/server.rs` (a real localhost HTTP server with fault injection, also used by `examples/fake_server.rs`).
- Sizes are decimal (1 GB = 10^9 bytes).
- Keep placeholder modules (`install`, `netcfg`, `services`, `ui`) empty until their phase.
