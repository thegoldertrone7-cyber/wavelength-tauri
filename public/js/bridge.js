'use strict';
/* ═════════ Game link (page side) ═════════
   Lets the Wavelength Link Minecraft mod mirror this player and control it.

   The Rust core (src-tauri/src/bridge.rs) answers the mod and already knows
   the playback position, play state, duration and live audio levels, because
   it owns the audio engine. This page supplies only what the core cannot
   know: the track's title, artist, artwork and liked state, shuffle and
   repeat, and the live theme tokens. It also runs the commands the mod sends
   (play, pause, next, previous, seek, like, volume), because the queue and
   shuffle logic live here.

   Quiet by design: while the mod is not reading, the page only keeps one
   long poll open (the core answers it the moment something happens). While
   the mod is reading, the page pushes a snapshot whenever something
   changed, and at least every few seconds.

   Loaded after app.js and uses its globals (R, S, audio, uid, ...). */
(function () {
  if (!window.__WL_NATIVE_AUDIO__) return;                              // only the native build has a core to talk to
  if (typeof NATIVE !== 'undefined' && NATIVE) return;                  // Android has no local core
  if (!/^(127\.0\.0\.1|localhost)$/.test(location.hostname)) return;    // only this computer's own page

  const root = document.documentElement;
  const TOKENS = {
    bg: '--bg', card: '--card', card2: '--card2', line: '--line', text: '--text', text2: '--text2',
    text3: '--text3', dot: '--dot', invert: '--invert', onInvert: '--on-invert', accent: '--accent', onAccent: '--on-accent'
  };
  const HEADERS = { 'Content-Type': 'application/json', 'X-Wavelength': '1' };
  const HEARTBEAT = 3000;      // push at least this often while the mod is reading
  const CHECK = 400;           // how often to look for changes while the mod is reading

  let probe = null, canvas = null, ctx2d = null;
  let themeSig = '', themeVal = null;
  let watching = false, pushing = false, lastPush = 0, lastSig = '', failures = 0;

  const clamp01 = v => Math.min(1, Math.max(0, +v || 0));
  const sleep = ms => new Promise(r => setTimeout(r, ms));

  /* Resolve any CSS colour to "rgb(r, g, b)" / "rgba(r, g, b, a)".
     getComputedStyle already does this for plain colours. Newer engines return
     color(srgb ...) or oklab(...) for color-mix(), which the core would
     reject, so those are flattened through a 1x1 canvas instead. */
  function resolve(value) {
    const v = String(value || '').trim();
    if (!v) return '';
    if (!probe) {
      probe = document.createElement('i');
      probe.style.cssText = 'position:fixed;left:-9px;top:-9px;width:1px;height:1px;visibility:hidden;pointer-events:none';
      document.body.appendChild(probe);
    }
    probe.style.color = '';
    probe.style.color = v;
    const c = getComputedStyle(probe).color;
    if (/^rgba?\(/i.test(c)) return c;
    try {
      if (!ctx2d) {
        canvas = document.createElement('canvas');
        canvas.width = canvas.height = 1;
        ctx2d = canvas.getContext('2d', { willReadFrequently: true });
      }
      ctx2d.clearRect(0, 0, 1, 1);
      ctx2d.fillStyle = '#000'; ctx2d.fillStyle = c || v;
      ctx2d.fillRect(0, 0, 1, 1);
      const d = ctx2d.getImageData(0, 0, 1, 1).data;
      return d[3] === 255 ? `rgb(${d[0]}, ${d[1]}, ${d[2]})` : `rgba(${d[0]}, ${d[1]}, ${d[2]}, ${(d[3] / 255).toFixed(3)})`;
    } catch { return ''; }
  }

  function theme() {
    const sig = [S.settings.theme, S.settings.style, S.settings.accent, root.dataset.mode, root.style.getPropertyValue('--accent')].join('|');
    if (sig === themeSig && themeVal) return themeVal;
    const cs = getComputedStyle(root), t = {
      id: String(S.settings.theme || 'dark'), style: String(S.settings.style || 'nothing'),
      mode: root.dataset.mode === 'light' ? 'light' : 'dark'
    };
    for (const k in TOKENS) t[k] = resolve(cs.getPropertyValue(TOKENS[k]).trim());
    themeSig = sig; themeVal = t;
    return t;
  }

  function snapshot() {
    const t = R.cur, d = isFinite(audio.duration) && audio.duration > 0 ? audio.duration : (t && t.duration) || 0;
    let art = '';
    if (t && t.art) { try { art = new URL(t.art, location.href).href; } catch { /* leave it out */ } }
    return {
      v: 1,
      track: t ? { id: uid(t), title: t.title || 'Untitled', artist: t.artist || '', album: t.album || '', art, liked: !!liked(t), dur: d, local: !!t.localId } : null,
      playing: !!R.playing, pos: audio.currentTime || 0, speed: +S.settings.speed || 1,
      volume: R.muted ? 0 : clamp01(S.settings.volume), shuffle: !!R.shuffle, repeat: R.repeat | 0,
      theme: Object.assign({ reduced: !!motionReduced() }, theme())
    };
  }

  /* What a change in the mod's view would look like, without the position
     (which the core reads from the engine and which changes every frame). */
  function signature(s) {
    const t = s.track;
    return [t ? t.id + '|' + t.title + '|' + t.artist + '|' + t.art + '|' + t.liked : '', s.playing, s.speed, s.volume, s.shuffle, s.repeat,
      s.theme.id, s.theme.style, s.theme.mode, s.theme.reduced, s.theme.accent, s.theme.bg].join('~');
  }

  function run(c) {
    try {
      switch (c.cmd) {
        case 'toggle': togglePlay(); break;
        case 'play': if (audio.paused) togglePlay(); break;
        case 'pause': if (!audio.paused) togglePlay(); break;
        case 'next': next(); break;
        case 'prev': prev(); break;
        case 'seek': if (R.cur) seekPreview(clamp01(c.value), true); break;
        case 'like': if (R.cur) toggleLike(R.cur); break;
        case 'volume': setVol(clamp01(c.value)); break;
      }
    } catch (e) { console.warn('[game link]', c && c.cmd, e); }
  }

  function accept(r) {
    if (!r || typeof r !== 'object') return;
    watching = !!r.watch;
    if (Array.isArray(r.cmds) && r.cmds.length) {
      r.cmds.forEach(run);
      lastSig = '';                                   // a command usually changes what the mod shows
      setTimeout(() => { if (watching) push(); }, 150);
    }
  }

  async function call(path, init) {
    const r = await fetch(path, Object.assign({ cache: 'no-store' }, init));
    if (!r.ok) throw new Error(path + ' ' + r.status);
    return r.json();
  }

  async function push() {
    if (pushing) return;
    pushing = true;
    try {
      const s = snapshot();
      accept(await call('/api/bridge/push', { method: 'POST', headers: HEADERS, body: JSON.stringify(s) }));
      lastSig = signature(s); lastPush = Date.now();
    } catch { /* the long poll below reports and backs off */ }
    finally { pushing = false; }
  }

  /* While the mod is reading: push on change, and as a heartbeat. */
  function check() {
    if (!watching) return;
    const stale = Date.now() - lastPush > HEARTBEAT;
    if (stale || signature(snapshot()) !== lastSig) push();
  }

  async function loop() {
    for (;;) {
      try {
        // Returns the instant a command is queued or the mod starts/stops reading, else after 20 s.
        const r = await call('/api/bridge/wait?ms=20000&w=' + (watching ? 1 : 0));
        const was = watching;
        accept(r);
        if (watching && !was) { lastSig = ''; push(); }   // the mod just started reading: send everything now
        failures = 0;
      } catch {
        failures++; watching = false;
        await sleep(Math.min(30000, 1000 * failures));    // the core is restarting, or this is not the native build
      }
    }
  }

  setInterval(check, CHECK);
  // A pause, seek or track change is worth telling the mod about straight away.
  ['play', 'pause', 'seeked', 'loadedmetadata'].forEach(ev => document.addEventListener(ev, () => { if (watching) push(); }, true));

  const start = () => setTimeout(loop, 800);
  if (document.readyState === 'complete') start(); else addEventListener('load', start);

  // Exposed for tests only.
  window.__WL_BRIDGE__ = { snapshot, signature, resolve };
})();
