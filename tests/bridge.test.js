'use strict';
// Loads public/js/bridge.js in a sandbox with the app's globals stubbed and a fake
// core that behaves like src-tauri/src/bridge.rs (long poll, push, command queue).
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const SRC = fs.readFileSync(path.join(__dirname, '../public/js/bridge.js'), 'utf8');
const wait = ms => new Promise(r => setTimeout(r, ms));
async function until(fn, ms = 2000) {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (fn()) return true; await wait(10); }
  return false;
}
const okResp = body => ({ ok: true, status: 200, json: () => Promise.resolve(body) });

function fakeCore() {
  const core = {
    watch: false, cmds: [], pushes: [], waiters: [], calls: [], fail: false, dead: false,
    reply() { return { ok: true, watch: core.watch, cmds: core.cmds.splice(0) }; },
    wake() { core.waiters.splice(0).forEach(f => f()); },
    queue(cmd, value = 0) { core.cmds.push({ cmd, value }); core.wake(); },
    setWatch(v) { core.watch = v; core.wake(); },
    fetch(url, init = {}) {
      if (core.dead) return new Promise(() => {});
      core.calls.push({ url, init });
      if (core.fail) return Promise.resolve({ ok: false, status: 500, json: () => Promise.resolve({}) });
      if (url.startsWith('/api/bridge/push')) { core.pushes.push(JSON.parse(init.body)); return Promise.resolve(okResp(core.reply())); }
      if (url.startsWith('/api/bridge/wait')) {
        const known = /[?&]w=1/.test(url);
        return new Promise(res => {
          const check = () => {
            if (core.cmds.length || core.watch !== known) res(okResp(core.reply()));
            else core.waiters.push(check);
          };
          check();
        });
      }
      return Promise.resolve(okResp({}));
    }
  };
  return core;
}

const hexToRgb = c => { const m = /^#([0-9a-f]{6})$/i.exec(c); return m ? `rgb(${parseInt(m[1].slice(0, 2), 16)}, ${parseInt(m[1].slice(2, 4), 16)}, ${parseInt(m[1].slice(4), 16)})` : c; };

function boot({ native = true, android = false, host = '127.0.0.1' } = {}) {
  const core = fakeCore();
  const timers = [];
  const tokens = {
    '--bg': '#101010', '--card': 'rgb(30, 30, 30)', '--card2': 'color-mix(in srgb, red 50%, blue)', '--line': '#333333',
    '--text': '#ffffff', '--text2': '#cccccc', '--text3': '#999999', '--dot': '#555555', '--invert': '#ffffff',
    '--on-invert': '#000000', '--accent': '#d71921', '--on-accent': '#ffffff'
  };
  const doc = {
    readyState: 'complete',
    documentElement: { dataset: { mode: 'dark' }, style: { getPropertyValue: () => '' } },
    body: { appendChild() {} },
    createElement: tag => tag === 'canvas'
      ? { width: 0, height: 0, getContext: () => ({ clearRect() {}, fillRect() {}, fillStyle: '', getImageData: () => ({ data: [12, 34, 56, 255] }) }) }
      : { style: { cssText: '', color: '' } },
    addEventListener() {}
  };
  const calls = [];
  const sb = {
    console, URL, JSON, Math, Date, Promise, isFinite, Object, Array, String, Number, Error,
    setTimeout: (fn, ms) => setTimeout(fn, ms <= 800 ? Math.min(ms || 0, 8) : ms),
    setInterval: (fn, ms) => { const id = setInterval(fn, Math.min(ms || 0, 20)); timers.push(id); return id; },
    document: doc,
    location: { hostname: host, port: '47823', href: 'http://127.0.0.1:47823/' },
    addEventListener() {},
    getComputedStyle: el => el === doc.documentElement
      ? { getPropertyValue: k => tokens[k] || '' }
      : { color: el.style.color.startsWith('color-mix') ? 'color(srgb 0.5 0 0.5)' : hexToRgb(el.style.color) },
    fetch: core.fetch,
    NATIVE: android,
    S: { settings: { theme: 'mocha', style: 'glass', accent: null, speed: 1, volume: 0.6 } },
    R: { cur: { title: 'Song', artist: 'Band', art: '/covers/a.jpg', ytId: 'abc', duration: 200 }, playing: true, muted: false, shuffle: false, repeat: 1 },
    audio: { duration: 180, currentTime: 12.5, paused: false },
    uid: t => 'yt:' + t.ytId, liked: () => false, motionReduced: () => false,
    togglePlay: () => calls.push(['toggle']), next: () => calls.push(['next']), prev: () => calls.push(['prev']),
    seekPreview: (f, final) => calls.push(['seek', f, final]), toggleLike: t => calls.push(['like', t.ytId]),
    setVol: f => calls.push(['volume', f])
  };
  sb.window = sb;
  if (native) sb.__WL_NATIVE_AUDIO__ = true;
  vm.createContext(sb);
  vm.runInContext(SRC, sb);
  return { sb, core, calls, api: sb.__WL_BRIDGE__, stop() { core.dead = true; timers.forEach(clearInterval); } };
}

async function scenario(opts, fn) {
  const t = boot(opts);
  try { await fn(t); } finally { t.stop(); }
}
const polling = t => until(() => t.core.calls.some(c => c.url.startsWith('/api/bridge/wait')));

test('stays out of the way off the native build, on Android and off loopback', async () => {
  for (const opts of [{ native: false }, { android: true }, { host: '192.168.0.4' }]) {
    await scenario(opts, async t => { await wait(60); assert.equal(t.core.calls.length, 0); });
  }
});

test('snapshot carries what the core cannot know, and no levels', () => scenario({}, t => {
  const s = t.api.snapshot();
  assert.equal(s.track.id, 'yt:abc');
  assert.equal(s.track.dur, 180);
  assert.equal(s.track.art, 'http://127.0.0.1:47823/covers/a.jpg');
  assert.equal(s.volume, 0.6); assert.equal(s.repeat, 1);
  assert.equal(s.theme.id, 'mocha'); assert.equal(s.theme.style, 'glass');
  assert.equal(s.theme.accent, 'rgb(215, 25, 33)');
  assert.equal(s.theme.card2, 'rgb(12, 34, 56)', 'color(srgb ...) is flattened through a canvas');
  assert.ok(!('bands' in s));
}));

test('signature ignores position but notices real changes', () => scenario({}, t => {
  const a = t.api.signature(t.api.snapshot());
  t.sb.audio.currentTime = 99;
  assert.equal(t.api.signature(t.api.snapshot()), a);
  t.sb.R.playing = false;
  assert.notEqual(t.api.signature(t.api.snapshot()), a);
}));

test('idle game: one parked long poll, no pushes', () => scenario({}, async t => {
  await polling(t); await wait(80);
  assert.equal(t.core.pushes.length, 0);
  const waits = t.core.calls.filter(c => c.url.startsWith('/api/bridge/wait'));
  assert.equal(waits.length, 1);
  assert.match(waits[0].url, /w=0/);
}));

test('mod starts reading: page pushes everything with the marker header', () => scenario({}, async t => {
  await polling(t);
  t.core.setWatch(true);
  assert.ok(await until(() => t.core.pushes.length > 0));
  assert.equal(t.core.pushes[0].track.title, 'Song');
  const push = t.core.calls.find(c => c.url === '/api/bridge/push');
  assert.equal(push.init.method, 'POST');
  assert.equal(push.init.headers['X-Wavelength'], '1');
  assert.ok(await until(() => t.core.calls.some(c => /wait.*w=1/.test(c.url))));
}));

test('commands arrive through the long poll and run in the app', () => scenario({}, async t => {
  await polling(t);
  t.core.queue('next');
  assert.ok(await until(() => t.calls.some(c => c[0] === 'next')));
  for (const [c, v] of [['volume', 0.3], ['seek', 0.5], ['like', 0], ['toggle', 0], ['prev', 0]]) t.core.queue(c, v);
  assert.ok(await until(() => t.calls.length >= 6));
  assert.deepEqual(t.calls.find(c => c[0] === 'seek'), ['seek', 0.5, true]);
  assert.deepEqual(t.calls.find(c => c[0] === 'volume'), ['volume', 0.3]);
  assert.deepEqual(t.calls.find(c => c[0] === 'like'), ['like', 'abc']);
}));

test('play/pause only act when they would change something; volume is clamped', () => scenario({}, async t => {
  await polling(t);
  t.core.queue('play'); await wait(60);
  assert.equal(t.calls.filter(c => c[0] === 'toggle').length, 0);
  t.core.queue('pause');
  assert.ok(await until(() => t.calls.some(c => c[0] === 'toggle')));
  t.core.queue('volume', 7);
  assert.ok(await until(() => t.calls.some(c => c[0] === 'volume')));
  assert.deepEqual(t.calls.find(c => c[0] === 'volume'), ['volume', 1]);
}));

test('a failing core backs off instead of hammering it', async () => {
  const t = boot();
  t.core.fail = true;
  try {
    await wait(300);
    const n = t.core.calls.filter(c => c.url.startsWith('/api/bridge/wait')).length;
    assert.ok(n >= 1 && n <= 2, `saw ${n} requests`);
  } finally { t.stop(); }
});
