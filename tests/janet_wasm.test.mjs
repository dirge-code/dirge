// Node.js test suite for the pared-down Janet VM in the wasm bundle.
//
// The artifact is committed (wasm-janet/dist/), so this test runs without a
// build step:
//   node --test tests/janet_wasm.test.mjs
//
// Rebuild the artifact with `./wasm-janet/build.sh` (see wasm-janet/README.md).

import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const createJanetModule = require('../wasm-janet/dist/janet.js');

// Emscripten MODULARIZE factories resolve with the Module instance.
const Module = await createJanetModule();
Module.ccall('janet_wasm_init', null, [], []);

const evalFn = Module.cwrap('janet_wasm_eval', 'number', ['string']);
const evalJanet = (code) => {
  const p = evalFn(code);
  const s = Module.UTF8ToString(p);
  Module._free(p);
  return s;
};

test('evalJanet evaluates arithmetic', () => {
  assert.equal(evalJanet('(+ 1 2)'), '3');
  assert.equal(evalJanet('(math/pow 2 10)'), '1024');
});

test('evalJanet pretty-prints collections like the REPL', () => {
  assert.equal(evalJanet('(map inc [1 2 3])'), '@[2 3 4]');
  assert.equal(evalJanet('(sort @[3 1 2])'), '@[1 2 3]');
});

test('evalJanet quotes string results', () => {
  assert.equal(evalJanet('(string "hello " "world")'), '"hello world"');
});

test('evalJanet prefixes errors with ERROR:', () => {
  const out = evalJanet('(this-symbol-is-undefined)');
  assert.ok(out.startsWith('ERROR:'), `expected ERROR: prefix, got ${JSON.stringify(out)}`);
});

test('evalJanet keeps a persistent environment across calls', () => {
  evalJanet('(var acc 41)');
  assert.equal(evalJanet('(+ acc 1)'), '42');
});

test('evalJanet defines and calls a function', () => {
  evalJanet('(defn square [x] (* x x))');
  assert.equal(evalJanet('(square 9)'), '81');
});
