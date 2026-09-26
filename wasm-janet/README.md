# Pared-down Janet VM for the dirge wasm bundle

`dist/janet.js` + `dist/janet.wasm` are a standalone Janet interpreter compiled
to WebAssembly with Emscripten. It is the Janet language core only: `init` +
`eval` on a persistent environment. There is no dirge FFI, no plugin tool
bridge, no threads, no dynamic module loading — those stay in the native
`plugin` feature (the `janetrs`/`evil_janet` path, which needs `bindgen` and a
host C toolchain and is not wasm-portable).

## Rebuild

```sh
./wasm-janet/build.sh          # needs docker + the emscripten/emsdk image
```

The script clones the pinned Janet release (default `v1.42.1`), generates the
amalgamated `build/c/janet.c` natively, then compiles `janet_wasm.c` + `janet.c`
to `dist/janet.js` + `dist/janet.wasm` inside the `emscripten/emsdk` container.

## API

`dist/janet.js` is an Emscripten `MODULARIZE` build exporting a factory named
`createJanetModule`. Two C functions are exposed:

- `janet_wasm_init()` — initialize the VM once.
- `janet_wasm_eval(code) -> char*` — evaluate a Janet expression and return a
  malloc'd string (pretty-printed value, or `"ERROR: ..."`). The caller frees
  it with `Module._free`.

In Node: `const createJanetModule = require('./dist/janet.js')`.
In the browser: load it with a plain `<script>` tag, then
`await window.createJanetModule()`.
