/* Wavelength lyrics helpers.
 *
 * Parsing and title cleaning live here instead of app.js so the same code can
 * be unit-tested with Node. The UI (app.js) and the local core (lyrics.rs) both
 * rely on this shape, so keep the returned objects stable.
 */
(function (root, factory) {
  'use strict';
  var api = factory();
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  if (root) root.WLLyrics = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  const LRC_TAG = /\[(\d{1,3}):(\d{2})(?:[.:](\d{1,3}))?\]/g;
  /* Enhanced LRC adds a timestamp before each word. The line still carries one
     real timestamp; drop the word tags so the words read as a sentence. */
  const WORD_TAG = /<\d{1,3}:\d{2}(?:[.:]\d{1,3})?>/g;
  const OFFSET_TAG = /\[offset:\s*([+-]?\d+(?:\.\d+)?)\s*\]/i;

  /* Bracketed noise that YouTube titles carry but lyrics databases do not. */
  const NOISE =
    /[\(\[]\s*(?:official(?:\s+(?:music\s+)?video)?|officiel|music\s+video|video|audio|lyrics?|lyric\s+video|visuali[sz]er|remaster(?:ed)?(?:\s*\d{4})?|hd|hq|4k|8k|mv|m\/v|clip|live(?:\s+at\s+[^)\]]*)?|explicit|clean|color\s*coded|sped\s*up|speed\s*up|slowed(?:\s*\+\s*reverb)?|nightcore|reverb|tiktok|from\s+[^)\]]*|full\s+album|album\s+version|radio\s+edit|single\s+version|bonus\s+track|remix|prod\.?\s+[^)\]]*|instrumental)\s*[\)\]]/gi;
  const TRAILING =
    /\s*[-–—]\s*(?:remaster(?:ed)?(?:\s*\d{4})?|live(?:\s+at\s+.*)?|radio\s+edit|single\s+version|album\s+version|from\s+.*|official(?:\s+(?:music\s+)?video)?|lyrics?|audio|visuali[sz]er)\s*$/i;

  function parseLrcMeta(text) {
    const src = String(text || '');
    const om = OFFSET_TAG.exec(src);
    // LRC says "+ shifts time up", so a positive offset makes the lyrics
    // appear earlier. Keep the raw seconds here and apply the sign in the UI.
    const offset = om ? (+om[1]) / 1000 : 0;
    const out = [];
    for (const raw of src.split(/\r?\n/)) {
      const tags = [...raw.matchAll(LRC_TAG)];
      if (!tags.length) continue;
      const body = raw.replace(LRC_TAG, '').replace(WORD_TAG, '').trim();
      for (const m of tags) {
        const frac = m[3] ? (+m[3]) / Math.pow(10, m[3].length) : 0;
        out.push({ t: (+m[1]) * 60 + (+m[2]) + frac, text: body });
      }
    }
    out.sort((a, b) => a.t - b.t);
    // A repeated timestamp is a translation or a credit. Drop an identical or
    // empty repeat, but keep a genuinely different line (bilingual lyrics).
    const clean = [];
    for (const line of out) {
      const prev = clean[clean.length - 1];
      if (prev && Math.abs(prev.t - line.t) < 0.001) {
        if (!line.text || line.text === prev.text) continue;
        clean.push(line);
        continue;
      }
      clean.push(line);
    }
    return { lines: clean, offset };
  }

  const parseLrc = text => parseLrcMeta(text).lines;

  function cleanArtist(artist) {
    return String(artist || '')
      .replace(/\s*[-–—]\s*topic$/i, '')
      .replace(/[\s_-]*(?:vevo|official)$/i, '')
      .replace(/\s+(?:music|records?)$/i, '')
      .replace(/\s*&\s*/g, ', ')
      .replace(/\s{2,}/g, ' ')
      .trim();
  }

  const norm = s =>
    String(s || '')
      .toLowerCase()
      .replace(/[’']/g, '')
      .replace(/[^\p{L}\p{N}]+/gu, ' ')
      .replace(/\s+/g, ' ')
      .trim();

  function similar(a, b) {
    const x = norm(a), y = norm(b);
    if (!x || !y) return false;
    if (x === y) return true;
    const short = x.length <= y.length ? x : y;
    const long = x.length <= y.length ? y : x;
    if (short.length >= 3 && long.includes(short)) return true;
    const ax = new Set(x.split(' ')), by = new Set(y.split(' '));
    let hit = 0;
    for (const w of ax) if (by.has(w)) hit++;
    return hit >= Math.max(1, Math.ceil(Math.min(ax.size, by.size) * 0.6));
  }

  /* "Artist - Song (Official Video) [HD]" -> { track: "Song", artist: "Artist" } */
  function guessTrack(title, artist) {
    let t = String(title || '').trim();
    let a = cleanArtist(artist);
    t = t
      .replace(NOISE, ' ')
      .replace(/[\(\[]\s*(?:ft\.?|feat\.?|featuring)\s+[^)\]]*[\)\]]/gi, ' ')
      .replace(/\s+(?:ft\.?|feat\.?|featuring)\s+.*$/i, '')
      .replace(/\s*\[[^\]]*\]\s*$/g, ' ')
      .replace(/\s{2,}/g, ' ')
      .trim();
    const m = /^(.+?)\s+[-–—]\s+(.+)$/.exec(t);
    if (m) {
      const left = cleanArtist(m[1]), right = m[2].trim();
      if (!a || similar(a, left)) {
        t = right;
        if (!a) a = left;
      } else if (similar(a, right)) {
        t = left;
      }
    }
    t = t.replace(TRAILING, '').replace(/^["'“”]+|["'“”]+$/g, '').replace(/\s{2,}/g, ' ').trim();
    return { track: t || String(title || ''), artist: a.split(',')[0].trim() };
  }

  /* Seconds -> "0.00s", "+0.25s", "−0.50s". */
  function formatOffset(sec) {
    const v = Math.round((+sec || 0) * 100) / 100;
    if (!v) return '0.00s';
    return (v > 0 ? '+' : '−') + Math.abs(v).toFixed(2) + 's';
  }

  return { LRC_TAG, parseLrc, parseLrcMeta, cleanArtist, cleanTitle: title => guessTrack(title, '').track, guessTrack, formatOffset, similar };
});
