#!/usr/bin/env node
'use strict';
// node scripts/setup-github.js OWNER/REPO [path/to/key.pub]
// Points the updater at your GitHub repo and (optionally) installs the public signing key.
//   Generate the key pair once:  npx tauri signer generate -w ~/.tauri/wavelength.key
const fs = require('fs'), path = require('path');
const [repo, keyFile] = process.argv.slice(2);
if (!/^[\w.-]+\/[\w.-]+$/.test(repo || '')) { console.error('Usage: node scripts/setup-github.js OWNER/REPO [key.pub]'); process.exit(1); }
const p = path.join(__dirname, '..', 'src-tauri', 'tauri.conf.json');
const conf = JSON.parse(fs.readFileSync(p, 'utf8'));
const u = (conf.plugins = conf.plugins || {}).updater = conf.plugins.updater || {};
u.endpoints = [`https://github.com/${repo}/releases/latest/download/latest.json`];
if (keyFile) {
  const key = fs.readFileSync(keyFile, 'utf8').trim();
  if (key.includes('PRIVATE') || key.length < 40) { console.error('That does not look like a PUBLIC key (.pub).'); process.exit(1); }
  u.pubkey = key;
}
fs.writeFileSync(p, JSON.stringify(conf, null, 2) + '\n');
console.log('endpoint: ' + u.endpoints[0]);
console.log('pubkey:   ' + (u.pubkey ? 'set' : 'NOT SET — releases will be refused until you set it'));
