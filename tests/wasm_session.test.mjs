// Node.js test suite for the dirge wasm session backend (nodejs target, `pkg/`).
//
// Build the artifact first, then run:
//   wasm-pack build --target nodejs --dev --no-opt --out-dir pkg -- --no-default-features --features wasm
//   node --test tests/wasm_session.test.mjs
//
// `SessionStore` is the pared-down, JSON-backed mirror of the terminal session
// persistence (create / append / load / list / delete) exposed to JS.

import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { SessionStore } = require('../pkg/dirge_agent.js');

test('SessionStore is a constructor', () => {
  assert.equal(typeof SessionStore, 'function');
});

test('create returns a unique id with empty messages', () => {
  const store = new SessionStore();
  const a = store.create('first');
  const b = store.create('second');
  assert.equal(typeof a, 'string');
  assert.notEqual(a, b);
  const json = JSON.parse(store.get(a));
  assert.equal(json.id, a);
  assert.equal(json.name, 'first');
  assert.deepEqual(json.messages, []);
});

test('append + get round-trips messages in order', () => {
  const store = new SessionStore();
  const id = store.create('session');
  store.append_message(id, 'user', 'hello');
  store.append_message(id, 'assistant', 'hi there');
  const json = JSON.parse(store.get(id));
  assert.equal(json.messages.length, 2);
  assert.deepEqual(json.messages[0], { role: 'user', content: 'hello' });
  assert.deepEqual(json.messages[1], { role: 'assistant', content: 'hi there' });
});

test('get on unknown id throws', () => {
  const store = new SessionStore();
  assert.throws(() => store.get('nope'));
});

test('append on unknown id throws', () => {
  const store = new SessionStore();
  assert.throws(() => store.append_message('nope', 'user', 'hi'));
});

test('list returns sessions newest first', () => {
  const store = new SessionStore();
  const older = store.create('older');
  const newer = store.create('newer');
  store.append_message(older, 'user', 'touch older'); // bumps older above newer
  const list = JSON.parse(store.list());
  assert.equal(list.length, 2);
  assert.equal(list[0].id, older);
  assert.equal(list[1].id, newer);
});

test('delete removes a session and is idempotent', () => {
  const store = new SessionStore();
  const id = store.create('doomed');
  store.delete(id);
  assert.throws(() => store.get(id));
  store.delete(id); // no throw
});
