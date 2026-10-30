#!/bin/sh
# Manual Phase 4 acceptance on a real machine (not CI). Needs: caddy, nftables, sudo, certutil,
# kiwix-tools, Firefox. Everything it changes is undone by `kit net disable` (run at the end).
#
#   test/accept_net.sh [package-id]      default: wikivoyage_en_nopic (smallest mirrored wiki); use wikipedia_de_nopic for the roadmap case
set -eu
KIT="${KIT:-cargo run --release --quiet --}"
PKG="${1:-wikivoyage_en_nopic}"

step() { printf '\n== %s\n' "$*"; }

step "1. install $PKG (downloads if needed)"
$KIT install --select "$PKG"

step "2. dry run: what will change and where root is needed"
$KIT net enable --dry-run

step "3. enable (sudo asks for your password for the [root] steps)"
$KIT net enable
$KIT net status

step "4. link check through the real vhost (50 random pages, no request may leave the machine)"
$KIT net check --samples 50

cat <<MSG

== 5. Manual browser check (needs you)
 - Unplug the network cable / turn Wi-Fi off.
 - Open in Firefox (and Chromium if installed):  https://de.wikipedia.org/wiki/Berlin
   (for wikivoyage_en_nopic: https://en.wikivoyage.org/wiki/Berlin)
 - Expected: page loads, padlock shown, no certificate warning; 50 clicked links work.
MSG
printf '\nPress Enter when done to undo everything (kit net disable) ... '
read -r _

step "6. disable and confirm nothing is left"
$KIT net disable
$KIT net status || true
! grep -q 'BEGIN kit (emergency-kit)' /etc/hosts && echo "hosts: clean"
sudo nft list table ip kit_net 2>/dev/null && echo "nft: STILL PRESENT" || echo "nft: clean"
[ ! -e "$HOME/.config/systemd/user/kit-caddy.service" ] && echo "unit: clean"
