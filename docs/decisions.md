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
