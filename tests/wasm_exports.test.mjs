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
const { token_count, chat, agent_chat, AgentHandle } = require('../pkg/dirge_agent.js');

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

test('AgentHandle is a constructor', () => {
  assert.equal(typeof AgentHandle, 'function');
});

test('AgentHandle registers a sync JS tool and calls it', async () => {
  const h = new AgentHandle('dummy-key');
  h.add_js_tool(
    'double',
    'Double a number',
    JSON.stringify({ type: 'object', properties: { n: { type: 'number' } }, required: ['n'] }),
    (argsJson) => {
      const { n } = JSON.parse(argsJson);
      return `double: ${n * 2}`;
    }
  );
  assert.equal(await h.call_tool('double', JSON.stringify({ n: 21 })), 'double: 42');
});

test('AgentHandle supports async JS tools (Promise result)', async () => {
  const h = new AgentHandle('dummy-key');
  h.add_js_tool(
    'upper',
    'Uppercase a string',
    JSON.stringify({ type: 'object', properties: { s: { type: 'string' } }, required: ['s'] }),
    async (argsJson) => {
      const { s } = JSON.parse(argsJson);
      return s.toUpperCase();
    }
  );
  assert.equal(await h.call_tool('upper', JSON.stringify({ s: 'abc' })), 'ABC');
});

test('AgentHandle rejects an unknown tool', async () => {
  const h = new AgentHandle('dummy-key');
  let msg = '';
  try {
    await h.call_tool('nope', '{}');
  } catch (err) {
    msg = String(err);
  }
  assert.match(msg, /unknown tool/);
});

test('AgentHandle.run returns a Promise', () => {
  const h = new AgentHandle('dummy-key');
  const p = h.run('hi');
  assert.ok(p instanceof Promise, 'run() should return a Promise');
  p.catch(() => {}); // swallow the eventual rejection so it never becomes unhandled
});

test('AgentHandle.run round-trips with a real key', { skip: !process.env.DEEPSEEK_API_KEY }, async () => {
  const h = new AgentHandle(process.env.DEEPSEEK_API_KEY);
  const text = await h.run('Say exactly: hello from the agent handle');
  assert.equal(typeof text, 'string');
  assert.ok(text.length > 0, 'agent reply should be non-empty');
});
