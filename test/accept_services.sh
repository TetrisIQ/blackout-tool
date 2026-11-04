#!/bin/sh
# Manual Phase 6 acceptance (first slice): Navidrome with a free music pack, Audiobookshelf with a LibriVox book, Jellyfin with Big Buck Bunny, Kavita with an ebook. Needs podman, systemd
# user services, and the Phase 4 front (caddy, sudo) for the HTTPS names. The pack is ~2.4 GB.
# Undo: kit uninstall navidrome audiobookshelf jellyfin  kavita --purge ; kit net enable (refresh) or disable.
set -eu
KIT="${KIT:-cargo run --release --quiet --}"

step() { printf '\n== %s\n' "$*"; }

step "1. dry run"
$KIT install --select navidrome,audiobookshelf,jellyfin,kavita --dry-run

step "2. install (pulls the image by digest, downloads and unpacks the pack, creates the login)"
$KIT install --select navidrome,audiobookshelf,jellyfin,kavita
$KIT status
$KIT verify

step "3. HTTPS name (needs --renew-ca once, because navidrome.kit.local is new to the CA)"
$KIT net enable --renew-ca
$KIT net status

cat <<MSG

== 4. Manual check (needs you)
 - Open https://navidrome.kit.local and log in with the login printed by 'kit status'.
 - Expected: the Musopen albums appear within about a minute (scan runs every minute) and play,
   with the network cable unplugged.
 - Open https://audiobookshelf.kit.local, log in, "Alice in Wonderland" is in the Audiobooks library
   and plays.
 - Open https://jellyfin.kit.local, log in, "Big Buck Bunny" is in the Video library and plays.
 - Open https://kavita.kit.local, log in, the Alice ebook is in the Books library and opens
   (the series is named after the Standard Ebooks collection, "The Guardian's 100 Greatest Novels").
 - Open https://kit.local: all four services are listed as tiles.
MSG
