#!/usr/bin/env node
'use strict';
// node scripts/release-notes.js 2.9.0 [CHANGELOG.md] — prints that version's section (used by the release workflow).
const fs = require('fs'), path = require('path');
const v = (process.argv[2] || '').replace(/^v/, '');
const file = process.argv[3] || path.join(__dirname, '..', 'CHANGELOG.md');
const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
const start = lines.findIndex(l => new RegExp('^##\\s+\\[?v?' + v.replace(/\./g, '\\.') + '\\]?(\\s|$)').test(l));
if (start < 0) { console.error('No section for ' + v + ' in ' + file); process.exit(1); }
let end = lines.findIndex((l, i) => i > start && /^##\s/.test(l));
if (end < 0) end = lines.length;
console.log(lines.slice(start + 1, end).join('\n').trim());
