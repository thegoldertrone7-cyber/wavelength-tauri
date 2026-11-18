'use strict';
// A view transition that never calls back (what WebKitGTK can do) must not stop a page from drawing.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const SRC = fs.readFileSync(path.join(__dirname, '../public/js/motion.js'), 'utf8');
const wait = ms => new Promise(r => setTimeout(r, ms));

function load({ ua = 'Mozilla/5.0 Chrome/120', vt }) {
  const store = {};
  const classes = new Set();
  const el = () => ({ style: {}, classList: { add() {}, remove() {}, toggle() {}, contains: () => false }, querySelector: () => null, querySelectorAll: () => [], addEventListener() {} });
  const doc = Object.assign(el(), {
    hidden: false, startViewTransition: vt,
    documentElement: { classList: { add: c => classes.add(c), remove: (...c) => c.forEach(x => classes.delete(x)), contains: c => classes.has(c) }, style: {} },
    querySelector: () => null, querySelectorAll: () => [], addEventListener() {}
  });
  const sb = { console, performance, setTimeout, clearTimeout, requestAnimationFrame: f => setTimeout(f, 0), cancelAnimationFrame() {},
    document: doc, navigator: { userAgent: ua }, localStorage: { getItem: k => store[k] ?? null, setItem: (k, v) => { store[k] = v; } },
    matchMedia: () => ({ matches: false, addEventListener() {} }), addEventListener() {}, getComputedStyle: () => ({}), ResizeObserver: class { observe() {} disconnect() {} } };
  sb.window = sb; vm.createContext(sb); vm.runInContext(SRC, sb);
  return { M: sb.WLMotion, store, classes };
}

test('a transition that never calls back still draws the page, then turns itself off', async () => {
  const never = () => ({ ready: new Promise(() => {}), finished: new Promise(() => {}), skipTransition() {} });
  const { M, store } = load({ vt: never });
  let swaps = 0;
  M.page(() => { swaps++; });
  await wait(550);
  assert.equal(swaps, 1, 'the page was drawn exactly once');
  assert.equal(store['wl-vt-off'], '1');
  assert.equal(M.canVT(), false);
  M.page(() => { swaps++; });
  assert.equal(swaps, 2, 'later changes draw immediately');
});

test('a working transition draws once and stays on', async () => {
  const good = cb => { cb(); return { ready: Promise.resolve(), finished: Promise.resolve(), skipTransition() {} }; };
  const { M, store } = load({ vt: good });
  let swaps = 0;
  await M.page(() => { swaps++; });
  await wait(450);
  assert.equal(swaps, 1);
  assert.equal(store['wl-vt-off'], undefined);
  assert.equal(M.canVT(), true);
});

test('WebKitGTK on Linux starts with transitions off', () => {
  const ua = 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15';
  const { M } = load({ ua, vt: () => { throw new Error('must not be used'); } });
  let swaps = 0;
  M.page(() => { swaps++; });
  assert.equal(swaps, 1);
  assert.equal(M.canVT(), false);
});

test('Chrome on Linux is not treated as WebKitGTK', () => {
  const { M } = load({ ua: 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120 Safari/537.36', vt: cb => { cb(); return { ready: Promise.resolve(), finished: Promise.resolve(), skipTransition() {} }; } });
  assert.equal(M.canVT(), true);
});
