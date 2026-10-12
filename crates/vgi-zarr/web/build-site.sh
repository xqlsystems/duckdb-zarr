#!/usr/bin/env bash
# Assemble the browser demo as a static site (e.g. for wx.xql.systems):
#
#   HAYBARN=<@haybarn/haybarn-wasm>/dist ./web/build-site.sh [out-dir]
#
# Needs wasm/dist from wasm/build.sh. The host must send the headers in
# _headers (COOP/COEP; Netlify and Cloudflare Pages read that file) over
# HTTPS: SharedArrayBuffer needs a cross-origin-isolated, secure context.
# Mind per-file limits: duckdb-coi.wasm is ~44 MB (Cloudflare Pages caps
# files at 25 MiB; serve that one from R2/a bucket with CORP, or use Netlify).
set -euo pipefail
cd "$(dirname "$0")"
: "${HAYBARN:?set HAYBARN to the @haybarn/haybarn-wasm dist directory}"
OUT=${1:-site}
WORKER=../wasm/dist

rm -rf "$OUT"
mkdir -p "$OUT/haybarn" "$OUT/vgi-zarr" "$OUT/vendor"
cp index.html app.js "$OUT/"
cp vendor/*.mjs "$OUT/vendor/"
for f in duckdb-browser.mjs duckdb-browser-vgi.mjs duckdb-browser-coi.worker.js \
         duckdb-browser-coi.pthread.worker.js duckdb-coi.wasm; do
  cp "$HAYBARN/$f" "$OUT/haybarn/"
done
cp "$WORKER/vgi_worker.js" "$WORKER/vgi_worker.wasm" "$WORKER/vgi-worker-boot.js" "$OUT/vgi-zarr/"

cat > "$OUT/_headers" <<'EOF'
/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Resource-Policy: same-origin
EOF
du -sh "$OUT"
