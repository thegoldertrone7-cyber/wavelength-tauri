#!/usr/bin/env node
'use strict';
// node scripts/bump-version.js 2.9.1   — keeps every version in step.
const fs = require('fs'), path = require('path');
const root = path.join(__dirname, '..');
const v = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(v || '')) { console.error('Usage: node scripts/bump-version.js X.Y.Z'); process.exit(1); }
const edit = (file, fn) => {
  const p = path.join(root, file), a = fs.readFileSync(p, 'utf8'), b = fn(a);
  if (a === b && !a.includes(v)) { console.error('No version found in ' + file); process.exit(1); }
  fs.writeFileSync(p, b); console.log('updated ' + file);
};
const json = (file) => edit(file, s => s.replace(/("version"\s*:\s*")[^"]+(")/, `$1${v}$2`));
json('package.json'); json('src-tauri/tauri.conf.json');
edit('src-tauri/Cargo.toml', s => s.replace(/^version = "[^"]+"/m, `version = "${v}"`));
edit('src-tauri/src/api.rs', s => s.replace(/pub const VERSION: &str = "[^"]+-tauri";/, `pub const VERSION: &str = "${v}-tauri";`));
edit('src-tauri/Cargo.lock', s => s.replace(/(name = "wavelength"\nversion = ")[^"]+(")/, `$1${v}$2`));
console.log('\nNext: add a "## ' + v + '" section to CHANGELOG.md, commit, then: git tag v' + v + ' && git push --tags');
