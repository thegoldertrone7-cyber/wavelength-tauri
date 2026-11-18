'use strict';
// The 2.9.3 port dropped the closing `*/` from the router banner, which commented out
// the `routeTok` declaration and made every page open fail in WebKit. Guard it here.
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const SRC = fs.readFileSync(path.join(__dirname, '../public/js/app.js'), 'utf8');
const DECL = "let curRoute = 'home', curArg = '', routeTok = 0, drawn = false;";

test('router banner is closed before the route state declaration', () => {
  const banner = SRC.split('\n').find(l => l.startsWith('/* ═════════ router'));
  assert.ok(banner, 'router banner exists');
  assert.match(banner, /\*\/\s*$/, 'router banner must close on its own line');
});

test('route state declaration follows the closed router banner', () => {
  const lines = SRC.split('\n');
  const i = lines.findIndex(l => l.startsWith('/* ═════════ router'));
  assert.notEqual(i, -1, 'router banner exists');
  assert.equal(lines[i + 1], DECL, 'declaration is the very next line, outside the banner');
});
