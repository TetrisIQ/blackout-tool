# Emergency Offline Kit — Roadmap

Working name: `emergency-kit` (rename freely).
Audience: Claude Code, working in an empty Linux repo. Work phase by phase; do not start a phase before the previous phase's acceptance criteria pass.

## 1. Goal

A Linux tool that downloads and installs a curated set of offline knowledge and entertainment, so a laptop keeps feeling like "the internet, read-only" when the real internet is gone.

- Overview screen with selectable packages and live size/disk estimates ("mostly an installer").
- Packages: Wikipedia (selected languages), other Kiwix ZIM content, maps, small local LLM (evaluate `anonnerdfrenzy/survival-llm`), Jellyfin, Audiobookshelf, Navidrome, Kavita/Calibre-Web, offline games, personal emergency folder.
- Real domains (e.g. `de.wikipedia.org`) resolve locally, so links between articles work unchanged, over HTTPS, without browser warnings.
- Optional: laptop acts as Wi-Fi hotspot so phones/tablets can use the same domains.
- Optional later: bootable live image with everything preconfigured.
- Entertainment content must be public domain or freely licensed (CC, etc.). Every content pack carries license metadata.

## 2. Key technical decisions (confirm in Phase 0, record in `docs/decisions.md`)

| Topic | Recommendation | Why |
|---|---|---|
| Language | Go (single static binary), alternative: Python + Textual | Easy distribution, good for downloader and daemons |
| UI | CLI first, then local web UI (embedded) and/or TUI | Web UI also usable from phones in hotspot mode |
| Package format | YAML manifests in `catalog/` | Community-extendable, diffable |
| Service runtime | Podman (rootless) for Jellyfin/Audiobookshelf/etc.; native `kiwix-serve` | Isolation, reproducible |
| Reverse proxy | Caddy | Simple config, automatic TLS with own CA |
| DNS | dnsmasq (also serves hotspot clients); `/etc/hosts` only as fallback | `/etc/hosts` does not cover other devices |
| Wikipedia mapping | Proxy rewrites `https://<lang>.wikipedia.org/wiki/<Title>` to Kiwix path | Kiwix serves under `/wikipedia_de/A/...` |
| Sizes | Fetched live from Kiwix catalog (OPDS), never hardcoded | Catalog changes |

## 3. Phases

### Phase 0 — Bootstrap
- Repo layout: `cmd/`, `internal/{catalog,download,install,netcfg,services,ui}`, `catalog/`, `docs/`, `test/`.
- `CLAUDE.md` with project rules (see section 4).
- CI: lint, unit tests, build. Integration tests in a throwaway VM/container (never touch the developer's real `/etc` in tests).
- **Done when:** `make test` and `make build` run green on an empty skeleton.

### Phase 1 — Catalog and manifest
- Define manifest schema: `id`, `name`, `description`, `category`, `type` (zim | files | container | apt | model), `source` (URL, mirrors, torrent/metalink if available), `sha256`, `size_bytes`, `license`, `languages`, `depends_on`, `post_install` hooks.
- Catalog loader with schema validation and a fetcher that reads current sizes/URLs/checksums from the Kiwix OPDS catalog.
- CLI: `kit list`, `kit show <id>`, `kit plan --profile standard` (total size, disk-space check, warnings).
- Profiles: Mini (~50 GB), Standard (~250 GB), Full (~1 TB), custom.
- **Done when:** `kit plan` prints a correct size table for a profile and refuses if disk space is insufficient.

### Phase 2 — Download engine
- HTTP range requests with resume, parallel chunks, retry with backoff, mirror fallback, optional bandwidth limit.
- SHA-256 verification; atomic move from `.part` to final file; persistent state file so interrupted downloads continue after reboot.
- Disk preflight (including temp space), configurable data directory (internal disk or external drive).
- Optional: metalink/torrent support for large ZIMs.
- **Done when:** a 5 GB download survives `kill -9` and network loss and finishes with a verified checksum; unit tests cover resume and corruption.

### Phase 3 — Package installers (ZIM first)
- Installer interface: `Plan() → Install() → Verify() → Uninstall()`; all steps idempotent.
- ZIM type: place file in library dir, maintain Kiwix library XML, run `kiwix-serve` as systemd (user) service.
- Landing page stub listing installed packages.
- **Done when:** `kit install wikipedia_de_nopic` yields a working local Wikipedia at `http://localhost:8080`.

### Phase 4 — Local domains over HTTPS (highest risk, do early)
- Generate a **per-machine local CA** (never shipped in a repo or image shared between users). Use X.509 **name constraints** so the CA can only issue certs for the mirrored domains.
- Trust-store integration: system store, Firefox and Chromium NSS databases (`certutil`), Flatpak/Snap browsers noted as known limitation.
- Caddy config generated from installed packages: one vhost per mirrored domain (`de.wikipedia.org`, `en.wikipedia.org`, `upload.wikimedia.org`, `*.wikimedia.org` assets, etc.).
- URL rewriting from real Wikipedia paths to Kiwix paths; handle redirects, search, images, special pages that do not exist offline (friendly "not available offline" page instead of a connection error).
- Name resolution: managed block in `/etc/hosts` (marked, reversible) plus dnsmasq for network-wide use.
- Link checker: crawl N sample articles, assert internal links resolve locally, no request leaves the machine.
- `kit net enable | disable | status` with full rollback.
- **Done when:** with the network cable unplugged, `https://de.wikipedia.org/wiki/Berlin` loads in Firefox and Chromium without warnings, and following 50 random links works.

### Phase 5 — Portal and unified search
- Start page at a friendly local name (e.g. `http://kit.local`) with tiles for all installed services.
- Unified search across Kiwix libraries (use Kiwix search API), plus global shortcuts to emergency content (first aid, water, contacts).
- **Done when:** one search box finds results from all installed ZIMs.

### Phase 6 — Service packages (containers)
- Rootless Podman compose-style definitions for Jellyfin, Audiobookshelf, Navidrome, Kavita/Calibre-Web.
- Content packs with license manifests: Blender Open Movies, Internet Archive public-domain films, LibriVox, Jamendo/Free Music Archive (CC), Standard Ebooks, free games (Luanti, OpenTTD, FreeCiv, 0 A.D.).
- Auto-provision libraries and first-run setup (no wizard needed offline).
- **Done when:** after install, each service shows its sample content without further manual setup.

### Phase 7 — Maps, AI, tools
- Maps: Organic Maps/OsmAnd data for region selection; optional PMTiles viewer; optional routing and geocoding as "heavy" packages.
- LLM: evaluate `survival-llm`; run via Ollama or llama.cpp; hardware check (RAM/GPU) recommends model size; Whisper and Argos/LibreTranslate as optional packs.
- Tech content: ArchWiki, man pages, DevDocs, selected Stack Exchange ZIMs, optional apt/pip mirror subset.
- **Done when:** a fresh Standard install answers a survival question via the local LLM and shows a map of the user's region offline.

### Phase 8 — UI overview
- Web UI (and/or TUI): package grid by category, size per package, running total vs free disk, profile presets, install/pause/resume, service status, "verify" button.
- **Done when:** a non-technical user can pick the Standard profile and install it without touching the terminal.

### Phase 9 — Hotspot mode
- `hostapd` + dnsmasq setup so phones/tablets resolve the same domains; captive-portal-style redirect to the portal.
- Trust of the local CA on mobile devices (guided install page with QR code).
- Clients are read-only by design; no inbound internet exposure.
- **Done when:** a phone joins the hotspot and opens the offline Wikipedia and Jellyfin.

### Phase 10 — Reliability, updates, offline self-test
- `kit doctor`: checks services, certs, DNS, disk, battery state, library integrity.
- "Airplane test": simulates no internet (network namespace/firewall), crawls portal and sample pages, reports missing links.
- Update mode: when online, diff catalog vs installed versions, download updates, keep previous version until verified.
- Uninstall/rollback for every package and the network layer.
- Personal emergency folder: encrypted store (e.g. gocryptfs/age) for IDs, medication list, contacts; surfaced on the portal.
- **Done when:** `kit doctor` and the airplane test pass on a clean Standard install and fail loudly when something is broken.

### Phase 11 — Live image (optional but valuable)
- Build pipeline with Debian `live-build` (or `archiso`) embedding the tool, services and a browser preconfigured for the local CA.
- Separate data partition (ext4/exFAT) so content updates do not require a new image; encrypted persistence partition for personal data.
- Test on real hardware: Secure Boot, Wi-Fi firmware, GPU/CPU-only LLM, low RAM.
- **Done when:** the stick boots on two different laptops, services start automatically, Wikipedia works over HTTPS without manual steps.

### Phase 12 — Release
- Documentation (English + German): quick start, profiles, hardware guide, power-saving tips.
- Packaging: `.deb`, AppImage or static tarball; reproducible release builds; signed catalog.
- Contribution guide for new packs (manifest template, license checklist).

## 4. Rules for Claude Code (put into `CLAUDE.md`)

1. Every system change (hosts, DNS, certs, services) must be **reversible** and marked; provide `disable` and `uninstall`.
2. Run unprivileged by default; request root only for specific steps (hosts/DNS/cert trust/hotspot) and explain why.
3. Never modify the host system in tests; use containers/VMs/temp dirs.
4. All installers are **idempotent**; re-running must be safe.
5. No hardcoded sizes or URLs for Kiwix content; read from the catalog.
6. Every content package declares a license; reject packages without one. No copyrighted media.
7. Verify checksums before installing anything.
8. Every phase ends with tests plus a short entry in `docs/progress.md`.
9. Prefer small, reviewable commits per task.

## 5. Risks and open questions

- **Local CA is security-sensitive.** Per-machine CA, name-constrained, key stored with strict permissions, clear removal command. Document the trade-off honestly.
- **HSTS/certificate pinning** in some apps (mobile apps, Firefox with strict settings) may still fail; browser-only support is the realistic target.
- **Wikipedia assets** come from several domains (`upload.wikimedia.org`, etc.); inspect real page requests early in Phase 4.
- **Live image size vs. content size.** Decide target stick/SSD sizes per profile.
- **LLM quality vs. hardware.** Need a minimal config for old laptops (CPU-only, 8 GB RAM).
- **Legal check** of each entertainment source before it is added to the catalog.
- Which Linux base for the first release (Debian/Ubuntu/Arch)? Decide in Phase 0.

## 6. Suggested first prompt for Claude Code

> Read ROADMAP.md. Do Phase 0 and Phase 1 only. Propose the tech-stack decisions in `docs/decisions.md` first and wait for my confirmation before writing code. Then implement the manifest schema, catalog loader and `kit plan` with tests.
