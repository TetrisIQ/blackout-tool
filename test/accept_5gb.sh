#!/usr/bin/env bash
# Phase 2 acceptance test (manual): a 5 GB download must survive kill -9 and network loss and
# finish with a verified checksum. Uses a local generated server; never touches the host system.
#
#   test/accept_5gb.sh [size-in-bytes]      (default 5 GiB; needs that much free space in $TMPDIR)
set -euo pipefail

SIZE="${1:-5368709120}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/kit-accept.XXXXXX")"
SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

cd "$ROOT"
cargo build --release --locked --bin kit --example fake_server
KIT=target/release/kit
SRV=target/release/examples/fake_server

echo "== generating checksum for $SIZE bytes"
# Throttled to roughly 40 MB/s in total (4 connections x 64 KiB per 6 ms), so no run can finish
# inside its kill window. Also: every 7th request is cut mid-body, and a 4 s outage every 40 s.
"$SRV" "$SIZE" --seed 5 --print-sha256 --piece-delay-ms 6 --reset-every 7 --outage 4:40 >"$WORK/server.log" 2>"$WORK/server.err" &
SERVER_PID=$!
for _ in $(seq 1 600); do grep -q '^PORT' "$WORK/server.log" 2>/dev/null && break; sleep 1; done
SHA="$(awk '/^SHA256/{print $2}' "$WORK/server.log")"
PORT="$(awk '/^PORT/{print $2}' "$WORK/server.log")"
echo "   sha256 $SHA, port $PORT"

mkdir -p "$WORK/catalog"
cat >"$WORK/catalog/big.yaml" <<YAML
id: big
name: Big test file
description: generated
category: test
type: files
source: {url: 'http://127.0.0.1:$PORT/data.bin'}
sha256: $SHA
size_bytes: $SIZE
license: {spdx: CC0-1.0}
YAML

KIT_ARGS=(--catalog "$WORK/catalog" --offline download --select big --data-dir "$WORK/data" --jobs 4)

DIR="$WORK/data/downloads/big"
progress() { # chunks done / total, from the state file
  python3 - "$DIR/data.bin.part.state" <<'PY' 2>/dev/null || echo "?"
import json, sys
s = json.load(open(sys.argv[1]))
done = bin(int(s["done"], 16)).count("1")
total = -(-s["size"] // s["chunk_size"])
print(f"{done}/{total} chunks ({100*done//total}%)")
PY
}

for i in 1 2 3; do
  echo "== run $i: starting, will be killed with SIGKILL after 20 s"
  "$KIT" "${KIT_ARGS[@]}" >/dev/null 2>&1 &   # kit itself, not a function, so $! is its PID
  PID=$!
  sleep 20
  kill -9 "$PID" 2>/dev/null || { echo "FAIL: kit exited before the kill (download too fast?)"; exit 1; }
  wait "$PID" 2>/dev/null || true
  [ ! -e "$DIR/data.bin" ] || { echo "FAIL: final file exists after kill $i; the kill did not interrupt anything"; exit 1; }
  echo "   killed; progress on disk: $(progress)"
done

echo "== final run (resets and outages continue in the background)"
"$KIT" "${KIT_ARGS[@]}"

echo "== verifying"
ACTUAL="$(sha256sum "$WORK/data/downloads/big/data.bin" | cut -d' ' -f1)"
if [ "$ACTUAL" = "$SHA" ]; then echo "PASS: checksum matches"; else echo "FAIL: $ACTUAL != $SHA"; exit 1; fi
ls "$WORK/data/downloads/big"
