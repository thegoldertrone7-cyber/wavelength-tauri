'use strict';
const test = require('node:test');
const assert = require('node:assert');
const { parseSongTable } = require('../public/js/songlist.js');

test('headered CSV with quoted commas and multi-artist fields', () => {
  const r = parseSongTable('Track name,Artist name(s),Album,Duration (ms)\n"Hello, World","A;B",Alb,215000\nSecond,C,,3:05\n');
  assert.deepStrictEqual(r.map(x => [x.title, x.artist, x.duration]), [['Hello, World', 'A, B', 215], ['Second', 'C', 185]]);
});
test('semicolon and tab separated', () => {
  assert.strictEqual(parseSongTable('title;artist\nOne;X\nTwo;Y').length, 2);
  assert.strictEqual(parseSongTable('Title\tArtist\nOne\tX').length, 1);
});
test('a quoted field can hold a line break and doubled quotes', () => {
  const r = parseSongTable('title,artist\n"Say ""Hi""\nagain",Z');
  assert.strictEqual(r[0].title, 'Say "Hi"\nagain');
});
test('duplicates collapse and 700 rows read fine', () => {
  let csv = 'Title,Artist\n';
  for (let i = 0; i < 700; i++) csv += `Song ${i},Artist ${i % 40}\n`;
  csv += 'Song 1,Artist 1\n';
  assert.strictEqual(parseSongTable(csv).length, 700);
});
test('JSON arrays', () => {
  const r = parseSongTable(JSON.stringify([{ name: 'A', artists: [{ name: 'X' }, { name: 'Y' }], duration_ms: 1000 }]));
  assert.deepStrictEqual([r[0].title, r[0].artist, r[0].duration], ['A', 'X, Y', 1]);
});
test('plain lines are left for the line reader', () => {
  assert.strictEqual(parseSongTable('Helluva Boss - Hot Goss\nJT Music - After Hours'), null);
  assert.strictEqual(parseSongTable('Artist - Song, Remix\nOther - Thing, Live'), null);
  assert.strictEqual(parseSongTable(''), null);
});
