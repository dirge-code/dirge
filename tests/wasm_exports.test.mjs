// Node.js test suite for the dirge wasm surface (nodejs target, `pkg/`).
//
// Build the artifact first, then run:
//   wasm-pack build --target nodejs --dev --no-opt --out-dir pkg -- --no-default-features --features wasm
//   node --test tests/wasm_exports.test.mjs
//
// The pure `token_count` tests always run; the live `chat` round-trip only runs
// when DEEPSEEK_API_KEY is present (it makes a real call to the DeepSeek API).

import test from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { token_count, chat, agent_chat } = require('../pkg/dirge_agent.js');

test('token_count is a function', () => {
  assert.equal(typeof token_count, 'function');
});

test('token_count returns 0 for empty text', () => {
  assert.equal(token_count(''), 0);
});

test('token_count returns a non-negative integer', () => {
  for (const s of ['a', 'hello world', '你好世界', 'emoji 🚀 test']) {
    const n = token_count(s);
    assert.ok(Number.isInteger(n), `${JSON.stringify(s)} -> ${n} is not an integer`);
    assert.ok(n >= 0, `${JSON.stringify(s)} -> ${n} is negative`);
  }
});

test('token_count golden values (llmtrim Anthropic estimator)', () => {
  const cases = [
    ['hello', 1],
    ['hello world', 3],
    ['hello, world', 4],
    ['a b c d e', 4],
    ['你好世界', 1],
    ['emoji 🚀 test', 3],
    ['the quick brown fox jumps over the lazy dog', 9],
  ];
  for (const [text, expected] of cases) {
    assert.equal(token_count(text), expected, `token_count(${JSON.stringify(text)})`);
  }
});

test('chat is a function returning a Promise', () => {
  assert.equal(typeof chat, 'function');
  const p = chat('dummy-key', 'hi');
  assert.ok(p instanceof Promise, 'chat() should return a Promise');
  p.catch(() => {}); // swallow the eventual rejection so it never becomes unhandled
});

test('chat round-trips with a real key', { skip: !process.env.DEEPSEEK_API_KEY }, async () => {
  const text = await chat(process.env.DEEPSEEK_API_KEY, 'Reply with exactly: ok');
  assert.equal(typeof text, 'string');
  assert.ok(text.length > 0, 'assistant reply should be non-empty');
});

test('agent_chat is a function returning a Promise', () => {
  assert.equal(typeof agent_chat, 'function');
  const p = agent_chat('dummy-key', 'hi');
  assert.ok(p instanceof Promise, 'agent_chat() should return a Promise');
  p.catch(() => {}); // swallow the eventual rejection so it never becomes unhandled
});

test('agent_chat round-trips with a real key', { skip: !process.env.DEEPSEEK_API_KEY }, async () => {
  const text = await agent_chat(process.env.DEEPSEEK_API_KEY, 'Say exactly: hello from the agent');
  assert.equal(typeof text, 'string');
  assert.ok(text.length > 0, 'agent reply should be non-empty');
});
