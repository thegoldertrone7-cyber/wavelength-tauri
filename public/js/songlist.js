'use strict';
/* Reads song lists exported from other services (Amazon Music through a converter, Exportify, TuneMyMusic, Soundiiz, a
   hand-made spreadsheet): CSV, semicolon or tab separated, with a header row, or a JSON array. Pure functions so the
   page and the tests share them. Returns null when the text does not look like a table with a recognisable title
   column, so the caller can fall back to the plain one-song-per-line reader. */
(function (root) {
  const COLS = {
    title: ['track name', 'track title', 'song name', 'track', 'title', 'song', 'name'],
    artist: ['artist name(s)', 'artist names', 'artist name', 'artists', 'artist', 'performer', 'album artist name(s)', 'album artist'],
    album: ['album name', 'album title', 'album'],
    dur: ['duration (ms)', 'duration_ms', 'track duration (ms)', 'duration', 'length', 'time', 'track duration'],
  };

  /* RFC 4180: quotes may hold the separator, doubled quotes and line breaks. */
  function parseDelimited(text, d) {
    const rows = []; let row = [], cur = '', q = false;
    for (let i = 0; i < text.length; i++) {
      const ch = text[i];
      if (q) {
        if (ch === '"') { if (text[i + 1] === '"') { cur += '"'; i++; } else q = false; }
        else cur += ch;
      } else if (ch === '"' && cur === '') q = true;
      else if (ch === d) { row.push(cur); cur = ''; }
      else if (ch === '\n' || ch === '\r') {
        if (ch === '\r' && text[i + 1] === '\n') i++;
        row.push(cur); cur = '';
        if (row.some(c => c.trim())) rows.push(row);
        row = [];
      } else cur += ch;
    }
    row.push(cur);
    if (row.some(c => c.trim())) rows.push(row);
    return rows;
  }

  const seconds = (v, header) => {
    const s = String(v || '').trim(); if (!s) return 0;
    const clock = s.match(/^(?:(\d+):)?(\d{1,2}):(\d{2})$/);
    if (clock) return (+clock[1] || 0) * 3600 + (+clock[2]) * 60 + (+clock[3]);
    const n = parseFloat(s); if (!isFinite(n)) return 0;
    return Math.round(/ms|milli/.test(header) || n > 20000 ? n / 1000 : n);
  };

  function fromRows(header, body) {
    const h = header.map(x => String(x || '').trim().toLowerCase());
    const find = list => { for (const name of list) { const i = h.indexOf(name); if (i >= 0) return i; } return -1; };
    const ti = find(COLS.title);
    if (ti < 0) return null;
    const ai = find(COLS.artist), li = find(COLS.album), di = find(COLS.dur);
    const out = [], seen = new Set();
    for (const r of body) {
      const title = String(r[ti] || '').trim();
      if (!title) continue;
      const artist = ai >= 0 ? String(r[ai] || '').trim().replace(/\s*;\s*/g, ', ') : '';
      const key = (artist + '\u0000' + title).toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({ title, artist, album: li >= 0 ? String(r[li] || '').trim() : '', duration: di >= 0 ? seconds(r[di], h[di]) : 0 });
    }
    return out;
  }

  function parseSongTable(input) {
    const text = String(input || '').replace(/^\uFEFF/, '').trim();
    if (!text) return null;
    if (text[0] === '[' || text[0] === '{') {
      try {
        let j = JSON.parse(text);
        if (!Array.isArray(j)) j = j.tracks || j.items || j.songs || [];
        const rows = j.map(x => x && (x.track && typeof x.track === 'object' ? x.track : x)).filter(Boolean);
        if (!rows.length) return null;
        const pick = (o, names) => { for (const n of names) if (o[n] != null && o[n] !== '') return o[n]; return ''; };
        const asName = v => Array.isArray(v) ? v.map(a => (a && a.name) || a).join(', ') : (v && v.name) || v || '';
        const out = [], seen = new Set();
        for (const o of rows) {
          const title = String(pick(o, ['title', 'name', 'trackName', 'track_name', 'song']) || '').trim();
          const artist = String(asName(pick(o, ['artist', 'artists', 'artistName', 'artist_name']))).trim();
          if (!title) continue;
          const key = (artist + '\u0000' + title).toLowerCase();
          if (seen.has(key)) continue;
          seen.add(key);
          out.push({ title, artist, album: String(asName(pick(o, ['album', 'albumName', 'album_name'])) || ''), duration: o.duration_ms != null || o.durationMs != null ? seconds(o.duration_ms != null ? o.duration_ms : o.durationMs, 'ms') : seconds(o.duration, 'duration') });
        }
        return out.length ? out : null;
      } catch { return null; }
    }
    const first = text.split(/\r?\n/, 1)[0];
    const best = [',', ';', '\t'].map(d => [d, first.split(d).length - 1]).sort((a, b) => b[1] - a[1])[0];
    if (!best || best[1] < 1) return null;
    const rows = parseDelimited(text, best[0]);
    if (rows.length < 2) return null;
    const got = fromRows(rows[0], rows.slice(1));
    return got && got.length ? got : null;
  }

  const api = { parseSongTable, parseDelimited };
  root.WLSongList = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof window !== 'undefined' ? window : globalThis);
