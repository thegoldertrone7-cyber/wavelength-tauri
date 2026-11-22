'use strict';
const test = require('node:test');
const assert = require('node:assert');
const A = require('../public/js/artists.js');

const T = (artist, seconds = 200, title = 'Song') => ({ artist, seconds, plays: 1, title, key: artist + title + seconds });

test('a co-credited singer is their own artist and the franchise stays one group', () => {
  const r = A.rank([
    T('Hazbin Hotel, Blake Roman', 300, 'Stayed Gone'),
    T('Blake Roman', 100, 'Solo'),
    T('Hazbin Hotel Cast', 200, 'Opening'),
    T('Erika Henningsen, Hazbin Hotel', 150, 'Respectless'),
  ]);
  const names = r.map(x => x.name);
  assert.deepStrictEqual(names.filter(n => /Hazbin/.test(n)), ['Hazbin Hotel']);
  assert.ok(!names.some(n => n.includes(',')), 'no combined "A, B" row: ' + names);
  const h = r.find(x => x.name === 'Hazbin Hotel'), b = r.find(x => x.name === 'Blake Roman');
  assert.strictEqual(h.seconds, 650); assert.strictEqual(h.songs, 3);
  assert.strictEqual(b.seconds, 400);
});

test('twenty franchise songs with different singers are one group', () => {
  const tracks = Array.from({ length: 20 }, (_, i) => T(`Hazbin Hotel, Singer ${i}`, 100, 'Track ' + i));
  const r = A.rank(tracks);
  assert.strictEqual(r[0].name, 'Hazbin Hotel');
  assert.strictEqual(r[0].songs, 20);
  assert.strictEqual(r[0].seconds, 2000);
});

test('soft separators only split with evidence', () => {
  const alone = A.rank([T('Simon & Garfunkel', 100)]);
  assert.deepStrictEqual(alone.map(x => x.name), ['Simon & Garfunkel']);
  const unknown = A.rank([T('Foo & Bar', 100)]);
  assert.deepStrictEqual(unknown.map(x => x.name), ['Foo & Bar']);
  const proven = A.rank([T('Foo & Bar', 100), T('Foo', 50), T('Bar', 50)]);
  assert.deepStrictEqual(proven.map(x => x.name).sort(), ['Bar', 'Foo']);
});

test('protected names with commas survive', () => {
  const r = A.rank([T('Tyler, The Creator', 100), T('Earth, Wind & Fire', 100)]);
  assert.deepStrictEqual(r.map(x => x.name).sort(), ['Earth, Wind & Fire', 'Tyler, The Creator']);
});

test('feat. and parentheses, Topic and accents fold together', () => {
  const r = A.rank([T('Sia - Topic', 100), T('Sia', 100), T('Beyoncé', 100), T('Beyonce', 100), T('Drake (feat. Sia)', 60)]);
  const sia = r.find(x => x.name === 'Sia');
  assert.strictEqual(sia.seconds, 260);
  assert.strictEqual(r.filter(x => /beyonc/i.test(x.name)).length, 1);
});

test('"from" titles join a group only when it already exists', () => {
  const r = A.rank([
    T('Hazbin Hotel', 100, 'Intro'),
    T('Some Singer', 100, 'Stayed Gone (from "Hazbin Hotel")'),
    T('Other Singer', 100, 'Thing (from "Unknown Show")'),
  ]);
  assert.strictEqual(r.find(x => x.name === 'Hazbin Hotel').songs, 2);
  assert.ok(!r.some(x => /Unknown Show/.test(x.name)));
});

test('empty and junk input is fine', () => {
  assert.deepStrictEqual(A.rank([]), []);
  assert.deepStrictEqual(A.rank(null), []);
  assert.deepStrictEqual(A.rank([{ artist: '', seconds: 0 }]), []);
});
