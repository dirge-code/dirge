#!/usr/bin/env bash
# Build the pared-down Janet VM (janet.js + janet.wasm) for the dirge wasm bundle.
#
# Prereqs: docker (emscripten/emsdk image), plus make + a C compiler on the
# host for the amalgamation step.
#
# Regenerates wasm-janet/dist/janet.js and wasm-janet/dist/janet.wasm from the
# pinned Janet release. The produced janet.js is a MODULARIZE (CommonJS/UMD)
# module: `require()` it in Node, or load it with a plain <script> tag in the
# browser and call `window.createJanetModule()`.
set -euo pipefail

JANET_REF="${JANET_REF:-v1.42.1}"
EMSDK_IMAGE="${EMSDK_IMAGE:-emscripten/emsdk:latest}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "==> fetching janet ${JANET_REF}"
git clone --quiet --depth 1 --branch "${JANET_REF}" https://github.com/janet-lang/janet "$WORK/janet"

echo "==> generating amalgamated janet.c"
( cd "$WORK/janet" && make build/c/janet.c )

echo "==> compiling to wasm with emscripten (${EMSDK_IMAGE})"
mkdir -p "$ROOT/dist"
docker run --rm -u "$(id -u):$(id -g)" \
  -v "$WORK/janet:/janet" -v "$ROOT:/src" \
  "$EMSDK_IMAGE" emcc -O2 \
    -s WASM=1 -s MODULARIZE=1 -s EXPORT_NAME=createJanetModule \
    -s EXPORTED_FUNCTIONS='["_janet_wasm_init","_janet_wasm_eval","_free"]' \
    -s EXPORTED_RUNTIME_METHODS='["cwrap","ccall","UTF8ToString","stringToUTF8","lengthBytesUTF8"]' \
    -s ALLOW_MEMORY_GROWTH=1 \
    -I /janet/src/include -I /janet/src/conf \
    -o /src/dist/janet.js \
    /src/janet_wasm.c /janet/build/c/janet.c

echo "==> done:"
ls -la "$ROOT/dist"
