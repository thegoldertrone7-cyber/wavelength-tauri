'use strict';
/* ═════════ Wavelength artist grouping ═════════
   YouTube gives every song one free-text "artist" string: "Hazbin Hotel, Blake Roman",
   "Hazbin Hotel Cast", "Blake Roman", "Sia - Topic". Counting those strings as they come
   puts the same people in the recap two or three times. This turns the strings into
   people and groups:

     1. Each string is split into its individual artists ("A, B feat. C" -> A, B, C).
        Commas, feat./ft./with/x/vs always split. "&", "and" and "+" only split when
        every piece also shows up as an artist on its own somewhere in the listening
        data, so "Simon & Garfunkel" or "Florence + The Machine" stay whole.
     2. Variants of one name are folded together ("Hazbin Hotel Cast", "Hazbin Hotel
        (Original Soundtrack)", "Hazbin Hotel - Topic" -> "Hazbin Hotel"; accents,
        case and punctuation are ignored).
     3. Every song credits every artist on it with the full listening time, so a song by
        "Hazbin Hotel, Blake Roman" counts for Hazbin Hotel AND for Blake Roman, and 20
        Hazbin Hotel songs sung by 20 different people are still one Hazbin Hotel.
     4. A title like "Stayed Gone (from "Hazbin Hotel")" counts for Hazbin Hotel too, but
        only when Hazbin Hotel is already an artist elsewhere in the data.

   Pure functions, no DOM, so the same code runs in the page and in the tests. */
(function (root) {
  const PROTECTED = [
    'Tyler, The Creator', 'Earth, Wind & Fire', 'Crosby, Stills & Nash', 'Crosby, Stills, Nash & Young',
    'Emerson, Lake & Palmer', 'Peter, Paul and Mary', 'Mumford & Sons', 'Simon & Garfunkel',
    'Hall & Oates', 'Florence + The Machine', 'Marina and the Diamonds', 'Bob Marley & The Wailers',
    'Huey Lewis and the News', 'Tom Petty and the Heartbreakers', 'Kool & The Gang', 'Sly & The Family Stone',
    'Brooks & Dunn', 'Years & Years', 'Nico & Vinz', 'Of Monsters and Men', 'Echo & the Bunnymen',
  ];
  const SOFT = /\s+(?:&|\+|and)\s+/i;
  const HARD = /\s*[,;]\s*|\s+(?:x|×|vs\.?|feat\.?|ft\.?|featuring|with)\s+/i;
  const FEAT_PAREN = /[([]\s*(?:feat\.?|ft\.?|featuring|with)\s+([^)\]]*)[)\]]/gi;
  const DESCRIPTORS = new Set(['cast', 'soundtrack', 'ost', 'official', 'original', 'vevo', 'topic', 'music', 'records', 'recordings']);

  const fold = s => String(s || '').normalize('NFD').replace(/[\u0300-\u036f]/g, '').toLowerCase()
    .replace(/&/g, ' and ').replace(/[^a-z0-9\u00c0-\uffff]+/g, ' ').trim().replace(/\s+/g, ' ');

  /* The name without YouTube decoration: "- Topic", VEVO, "(Original Soundtrack)", "Cast". */
  function clean(name) {
    let s = String(name || '').replace(/\s*-\s*topic\s*$/i, '').replace(/vevo\s*$/i, '')
      .replace(/[([][^)\]]*[)\]]/g, ' ').replace(/\s+/g, ' ').trim();
    const words = s.split(' ');
    while (words.length > 1 && DESCRIPTORS.has(fold(words[words.length - 1]))) words.pop();
    return words.join(' ').trim();
  }
  const keyOf = name => fold(clean(name));

  function splitHard(raw) {
    let s = String(raw || '').replace(FEAT_PAREN, (_, list) => ', ' + list);
    const held = [];
    for (const p of PROTECTED) {
      const re = new RegExp(p.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'ig');
      s = s.replace(re, m => { held.push(m); return `\u0001${held.length - 1}\u0001`; });
    }
    return s.split(HARD).map(x => x && x.trim()).filter(Boolean)
      .map(x => x.replace(/\u0001(\d+)\u0001/g, (_, i) => held[+i]));
  }

  /* Plain "artist" strings (no soft separator): the evidence that "A & B" is two people. */
  function knownSolo(strings) {
    const known = new Set();
    for (const raw of strings) for (const seg of splitHard(raw)) {
      if (!SOFT.test(seg) || PROTECTED.some(p => fold(p) === fold(seg))) known.add(keyOf(seg));
    }
    return known;
  }

  function namesFor(raw, known) {
    const out = [];
    for (const seg of splitHard(raw)) {
      if (SOFT.test(seg) && !PROTECTED.some(p => fold(p) === fold(seg))) {
        const bits = seg.split(SOFT).map(x => x.trim()).filter(Boolean);
        if (bits.length > 1 && bits.every(b => known.has(keyOf(b)))) { out.push(...bits); continue; }
      }
      out.push(seg);
    }
    return out.filter(n => keyOf(n));
  }

  const FROM_TITLE = /[([]\s*(?:from|ost|soundtrack)[^)\]"“]*["“']?([^)\]"”]{3,60}?)["”']?\s*[)\]]/i;

  /* tracks: [{ key?, title?, artist, seconds, plays }]  ->  ranked artists. */
  function rank(tracks, opt = {}) {
    const list = (tracks || []).filter(t => t && (+t.seconds > 0 || +t.plays > 0));
    const known = knownSolo(list.map(t => t.artist));
    const groups = new Map();
    const known_keys = new Set();
    const credited = list.map(t => {
      const names = namesFor(t.artist || '', known);
      names.forEach(n => known_keys.add(keyOf(n)));
      return names;
    });
    list.forEach((t, idx) => {
      const names = credited[idx].slice();
      const m = FROM_TITLE.exec(String(t.title || ''));
      if (m) { const k = keyOf(m[1]); if (k && known_keys.has(k)) names.push(m[1]); }
      const seen = new Set(), here = [];
      for (const n of names) {
        const k = keyOf(n);
        if (!k || seen.has(k)) continue;
        seen.add(k); here.push({ n, k });
      }
      for (const { n, k } of here) {
        const g = groups.get(k) || { key: k, seconds: 0, plays: 0, songs: new Set(), variants: new Map(), together: new Map() };
        g.seconds += +t.seconds || 0; g.plays += +t.plays || 0;
        g.songs.add(t.key || `${fold(t.title)}|${fold(t.artist)}`);
        const c = clean(n) || n;
        g.variants.set(c, (g.variants.get(c) || 0) + (+t.seconds || 1));
        for (const o of here) if (o.k !== k) g.together.set(o.k, (g.together.get(o.k) || 0) + (+t.seconds || 1));
        groups.set(k, g);
      }
    });
    const display = new Map();
    for (const [k, g] of groups) display.set(k, [...g.variants.entries()].sort((a, b) => b[1] - a[1])[0][0]);
    const out = [...groups.values()].map(g => ({
      name: display.get(g.key),
      key: g.key,
      seconds: g.seconds,
      plays: g.plays,
      songs: g.songs.size,
      with: [...g.together.entries()].sort((a, b) => b[1] - a[1]).slice(0, 3).map(([k]) => display.get(k)).filter(Boolean),
    })).sort((a, b) => b.seconds - a.seconds || b.plays - a.plays);
    return opt.limit ? out.slice(0, opt.limit) : out;
  }

  const api = { rank, splitHard, namesFor, clean, keyOf, fold };
  root.WLArtists = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof window !== 'undefined' ? window : globalThis);
