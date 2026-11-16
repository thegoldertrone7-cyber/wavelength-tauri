'use strict';
// The lyrics helpers in public/js/lyrics.js: LRC parsing (including [offset:]),
// title cleaning for upload-style names, and the loose match used to accept a
// result. These are shared by the UI and the Rust core's matching rules.
const test = require('node:test');
const assert = require('node:assert/strict');
const LY = require('../public/js/lyrics.js');

const { parseLrc, parseLrcMeta, guessTrack, cleanArtist, formatOffset, similar } = LY;

test('parses timestamps with two and three digit fractions', () => {
  const lines = parseLrc('[00:12.34] first\n[01:02.5] second\n[00:05] third');
  assert.deepEqual(lines.map(l => l.text), ['third', 'first', 'second']);
  assert.equal(lines[1].t, 12.34);
  assert.equal(lines[2].t, 62.5);
});

test('keeps a line that repeats its timestamp and drops empty duplicates', () => {
  const lines = parseLrc('[00:01.00] hello\n[00:01.00]\n[00:02.00] translation\n[00:02.00] translation');
  assert.deepEqual(lines.map(l => l.text), ['hello', 'translation']);
});

test('reads the [offset:] tag in seconds and ignores metadata lines', () => {
  const meta = parseLrcMeta('[ar:Someone]\n[offset:+250]\n[00:01.00] hello\n[00:02.00] world');
  assert.equal(meta.offset, 0.25);
  assert.deepEqual(meta.lines.map(l => l.text), ['hello', 'world']);
  assert.equal(parseLrcMeta('[offset:-1000]').offset, -1);
  assert.equal(parseLrcMeta('no tags here').offset, 0);
  assert.deepEqual(parseLrcMeta('').lines, []);
});

test('treats plain text as nothing to sync', () => {
  assert.deepEqual(parseLrc('just words\nand more words'), []);
});

test('drops word timestamps from enhanced LRC', () => {
  const lines = parseLrc('[00:31.32]<00:31.32>You <00:31.66>wouldn\'t <00:32.19>get <00:32.46>this');
  assert.deepEqual(lines, [{ t: 31.32, text: "You wouldn't get this" }]);
});

test('cleans artist names from video sites', () => {
  assert.equal(cleanArtist('Adele - Topic'), 'Adele');
  assert.equal(cleanArtist('AdeleVEVO'), 'Adele');
  assert.equal(cleanArtist('Simon & Garfunkel'), 'Simon, Garfunkel');
  assert.equal(cleanArtist('  '), '');
});

test('finds the song in an upload title', () => {
  assert.deepEqual(guessTrack('Daft Punk - Get Lucky (Official Video) [HD]', ''), {
    track: 'Get Lucky', artist: 'Daft Punk'
  });
  assert.deepEqual(guessTrack('Get Lucky (feat. Pharrell Williams)', 'Daft Punk'), {
    track: 'Get Lucky', artist: 'Daft Punk'
  });
  assert.deepEqual(guessTrack('Get Lucky - Radio Edit', 'Daft Punk'), {
    track: 'Get Lucky', artist: 'Daft Punk'
  });
});

test('matches even when the title and artist are reversed', () => {
  assert.deepEqual(guessTrack('Daft Punk - Get Lucky', 'Get Lucky'), {
    track: 'Daft Punk', artist: 'Get Lucky'
  });
});

test('falls back to the raw title when nothing can be cleaned', () => {
  assert.deepEqual(guessTrack('', 'Artist'), { track: '', artist: 'Artist' });
  assert.equal(guessTrack('(Official Video)', '').track, '(Official Video)');
});

test('formats the manual offset for people', () => {
  assert.equal(formatOffset(0), '0.00s');
  assert.equal(formatOffset(0.25), '+0.25s');
  assert.equal(formatOffset(-1), '−1.00s');
  assert.equal(formatOffset(NaN), '0.00s');
});

test('is loose about punctuation and strict about different songs', () => {
  assert.ok(similar('Don’t Stop Me Now', 'Dont Stop Me Now!'));
  assert.ok(similar('Get Lucky', 'Get Lucky (feat. Pharrell)'));
  assert.ok(!similar('Get Lucky', 'Lose Yourself'));
  assert.ok(!similar('', 'Anything'));
});
