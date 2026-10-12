#!/usr/bin/env bash
# Build the browser (`worker:`) build of the vgi-zarr worker:
# vgi-zarr compiled to wasm32-unknown-emscripten (staticlib), linked by emcc into a
# MODULARIZE'd Web Worker module that Haybarn-wasm reaches over its SAB ring.
#
#   VGI_WORKER_RUNTIME=<dir with vgi_worker_{lib,pre}.js, vgi-worker-boot.js> ./wasm/build.sh
#
# Needs emcc on PATH and a nightly toolchain with rust-src: the pthread build
# recompiles std with atomics (-Z build-std). Pattern from Query-farm/vgi-fixedformat.
set -euo pipefail
cd "$(dirname "$0")/.."

: "${VGI_WORKER_RUNTIME:?set VGI_WORKER_RUNTIME to @query-farm/vgi-worker-runtime/wasm}"
TARGET=wasm32-unknown-emscripten
OUT=wasm/dist
mkdir -p "$OUT"

# The extension crate's .cargo/config.toml sets CFLAGS -fPIC for side modules;
# this is a main module, which is fine either way.
RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-args=-pthread -C link-arg=--no-entry" \
  cargo +nightly build --lib \
  --target "$TARGET" \
  -Z build-std=std,panic_abort \
  --release

LIB="target/$TARGET/release/libvgi_zarr.a"

# vgi_worker_pre.js starts with "(function(){"; emscripten 6's minified prelude
# ends with `globalThis.name=="em-pthread"` and no semicolon, so without a
# leading ";" the two parse as a call: `"em-pthread"(function(){...})`.
PRE="$(mktemp --suffix=.js)"
{ echo ';'; cat "$VGI_WORKER_RUNTIME/vgi_worker_pre.js"; } > "$PRE"

# PTHREAD_POOL_SIZE covers the 4 serve slots plus the rayon pool that fetches
# chunks concurrently (each sync XHR blocks only its own pthread).
POOL=${PTHREAD_POOL_SIZE:-12}
emcc wasm/main.c "$LIB" \
  --js-library wasm/zarr_xhr_lib.js \
  --js-library "$VGI_WORKER_RUNTIME/vgi_worker_lib.js" \
  --pre-js "$PRE" \
  -sMODULARIZE=1 -sEXPORT_NAME=VgiWorker \
  -pthread -sPTHREAD_POOL_SIZE=$POOL -sSHARED_MEMORY=1 \
  -fwasm-exceptions \
  -sENVIRONMENT=web,worker \
  -sEXPORTED_FUNCTIONS=_main,_vgi_worker_init,_vgi_worker_serve_sab_slot,_vgi_worker_serve_pool,_malloc,_free \
  -sEXPORTED_RUNTIME_METHODS=HEAPU8,PThread,stringToNewUTF8 \
  -sEXIT_RUNTIME=0 -sALLOW_MEMORY_GROWTH=1 -sMAXIMUM_MEMORY=${MAXIMUM_MEMORY:-2GB} \
  -sMALLOC=dlmalloc \
  -sSTACK_SIZE=2MB -sDEFAULT_PTHREAD_STACK_SIZE=2MB -sINITIAL_MEMORY=64MB \
  -O3 \
  -o "$OUT/vgi_worker.js"

# Emscripten 6 dropped Module.mainScriptUrlOrBlob: pthreads are spawned from
# _scriptName, which inside the boot Worker is vgi-worker-boot.js, so the pool
# never comes up. Restore the override vgi-worker-boot.js passes.
sed -i 's/var pthreadMainJs=_scriptName;/var pthreadMainJs=Module["mainScriptUrlOrBlob"]||_scriptName;/' "$OUT/vgi_worker.js"
grep -q 'Module\["mainScriptUrlOrBlob"\]' "$OUT/vgi_worker.js" || { echo "mainScriptUrlOrBlob patch did not apply" >&2; exit 1; }

cp "$VGI_WORKER_RUNTIME/vgi-worker-boot.js" "$OUT/vgi-worker-boot.js"
ls -la "$OUT"
