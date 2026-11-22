/* Wavelength native audio bridge.
 *
 * The UI was written against HTMLAudioElement. Tauri has no Electron media
 * stack we want to rely on, so <audio> is replaced by <wl-audio>, a custom
 * element that forwards the same properties/methods/events to the Rust audio
 * engine (symphonia + cpal through rodio) and polls it for position/duration.
 *
 * The Web Audio analyser is patched to serve the engine's FFT tap, so the
 * real visualizer and smart-volume code keep working without a media element.
 */
(function () {
  'use strict';
  var native = location.port === '47823' || !!window.__TAURI_INTERNALS__;
  if (!native) return;
  window.__WL_NATIVE_AUDIO__ = true;

  var BASE = location.origin;
  var statusCache = { at: 0, data: null };

  function post(action, body) {
    return fetch(BASE + '/api/audio/' + action, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body || {})
    }).then(function (r) {
      if (!r.ok) throw new Error('audio ' + action + ' failed (' + r.status + ')');
      return r.json().catch(function () { return {}; });
    });
  }

  function getStatus(force) {
    var now = Date.now();
    if (!force && statusCache.data && now - statusCache.at < 100) return Promise.resolve(statusCache.data);
    return fetch(BASE + '/api/audio/status', { cache: 'no-store' })
      .then(function (r) { return r.json(); })
      .then(function (d) { statusCache = { at: Date.now(), data: d }; return d; })
      .catch(function () { return statusCache.data || null; });
  }

  /* ── FFT tap for the visualizer ────────────────────────────────────── */
  /* `target` is the newest reading from the engine (about 22 times a second while the visualizer is up); `bins` is what
     the page reads, eased toward it on every read so bars rise quickly and fall smoothly at the page's frame rate
     instead of stepping each time a reading arrives. */
  var spectrum = { at: 0, bins: new Uint8Array(256), target: new Uint8Array(256), wave: new Uint8Array(512), waveAt: 0, level: 0, eased: 0 };

  /* ── volume bridge ─────────────────────────────────────────────────── */
  /* The UI keeps its volume on a GainNode that feeds the (nonexistent)
     media element. Mirror that node's value into the native engine so the
     slider, mute key and smart volume all work. */
  var finalGain = null;
  var lastEngineVolume = -1;
  (function patchGraph() {
    var AN = window.AudioNode;
    if (!AN || !AN.prototype) return;
    var origConnect = AN.prototype.connect;
    AN.prototype.connect = function (dest) {
      try {
        if (
          dest &&
          this.context &&
          dest === this.context.destination &&
          this.gain &&
          typeof this.gain === 'object'
        ) {
          finalGain = this;
        }
      } catch (e) {}
      return origConnect.apply(this, arguments);
    };
  })();

  function syncVolume() {
    if (!finalGain) return;
    var v = Math.max(0, Math.min(1, Number(finalGain.gain.value) || 0));
    if (Math.abs(v - lastEngineVolume) < 0.004) return;
    lastEngineVolume = v;
    post('volume', { volume: v }).catch(function () {});
  }
  setInterval(syncVolume, 150);

  var fastPoll = false;
  function ease() {
    var now = performance.now(), dt = Math.min(0.1, Math.max(0, (now - (spectrum.eased || now)) / 1000));
    if (dt < 0.004) return;
    spectrum.eased = now;
    var up = 1 - Math.exp(-dt * 30), down = 1 - Math.exp(-dt * 8), sum = 0;
    for (var i = 0; i < spectrum.bins.length; i++) {
      var cur = spectrum.bins[i], t = spectrum.target[i];
      cur += (t - cur) * (t > cur ? up : down);
      spectrum.bins[i] = cur;
      sum += cur;
    }
    spectrum.level = Math.min(1, sum / (spectrum.bins.length * 255) * 2.2);
  }
  function pollSpectrum() {
    if (document.hidden) return;
    var gap = fastPoll ? 40 : 400;
    if (Date.now() - spectrum.at < gap) return;
    spectrum.at = Date.now();
    fetch(BASE + '/api/audio/spectrum?v=2', { cache: 'no-store' })
      .then(function (r) { return r.arrayBuffer(); })
      .then(function (buf) {
        var next = new Uint8Array(buf);
        if (next.length >= 256 + 512) {
          spectrum.target.set(next.subarray(0, 256));
          spectrum.wave.set(next.subarray(256, 256 + 512));
          spectrum.waveAt = performance.now();
        } else {
          // An older core that only knows the plain spectrum.
          for (var i = 0; i < 256 && i < next.length; i++) spectrum.target[i] = next[i];
        }
      })
      .catch(function () {});
  }
  setInterval(pollSpectrum, 20);
  /* The page says when the visualizer is on screen; otherwise the engine is only asked a few times a second. */
  window.wlSpectrumFast = function (on) { fastPoll = !!on; };

  function patchAnalyser(analyser) {
    try {
      analyser.getByteFrequencyData = function (arr) {
        ease();
        var n = Math.min(arr.length, spectrum.bins.length);
        for (var i = 0; i < n; i++) arr[i] = spectrum.bins[i];
        for (var j = n; j < arr.length; j++) arr[j] = 0;
        return arr;
      };
      analyser.getByteTimeDomainData = function (arr) {
        ease();
        if (performance.now() - spectrum.waveAt < 400) {
          // The real output samples, stretched or squeezed to the size asked for.
          var w = spectrum.wave, m = arr.length;
          for (var k = 0; k < m; k++) arr[k] = w[Math.min(w.length - 1, Math.floor(k * w.length / m))];
          return arr;
        }
        var t = performance.now() / 1000;
        var amp = 3 + spectrum.level * 46;
        for (var i = 0; i < arr.length; i++) {
          var phase = (i / Math.max(1, arr.length - 1)) * Math.PI * 2;
          arr[i] = Math.max(0, Math.min(255, 128 + Math.sin(t * 8 + phase * 3) * amp * (0.6 + 0.4 * Math.sin(phase * 7 + t))));
        }
        return arr;
      };
      analyser.getFloatFrequencyData = function (arr) {
        ease();
        for (var i = 0; i < arr.length; i++) {
          var v = spectrum.bins[Math.min(i, spectrum.bins.length - 1)] / 255;
          arr[i] = -100 + v * 70;
        }
        return arr;
      };
    } catch (e) {}
    return analyser;
  }
  /* For a webview with no Web Audio at all: a stand-in analyser fed by the engine. */
  window.WLNativeAnalyser = function () {
    return patchAnalyser({ fftSize: 512, frequencyBinCount: 256, smoothingTimeConstant: 0, connect: function () {}, disconnect: function () {} });
  };

  function patchWebAudio() {
    var AC = window.AudioContext || window.webkitAudioContext;
    if (!AC || !AC.prototype) return;
    var origAnalyser = AC.prototype.createAnalyser;
    AC.prototype.createAnalyser = function () {
      if (origAnalyser) {
        try { return patchAnalyser(origAnalyser.call(this)); } catch (e) {}
      }
      return patchAnalyser({ fftSize: 512, frequencyBinCount: 256, smoothingTimeConstant: 0.78, connect: function () {}, disconnect: function () {} });
    };
    // A native audio element cannot be routed into Web Audio.
    AC.prototype.createMediaElementSource = function () {
      return { connect: function () {}, disconnect: function () {}, mediaElement: null };
    };
  }
  patchWebAudio();

  /* ── the element ───────────────────────────────────────────────────── */
  class NativeAudio extends HTMLElement {
    constructor() {
      super();
      this._src = '';
      this._volume = 1;
      this._rate = 1;
      this._paused = true;
      this._ready = 0;
      this._dur = 0;
      this._pos = 0;
      this._err = null;
      this._loading = false;
      this._ended = false;
    }

    get channel() { return this.id === 'audio-b' ? 'b' : 'a'; }
    get src() { return this._src; }
    set src(v) {
      var s = String(v == null ? '' : v);
      if (s === this._src) return;
      this._src = s;
      this._ready = 0;
      this._dur = 0;
      this._pos = 0;
      this._err = null;
      this._ended = false;
      if (!s) {
        this._stop();
        this.dispatchEvent(new Event('emptied'));
      } else {
        this.dispatchEvent(new Event('loadstart'));
      }
    }
    get currentTime() { return this._pos; }
    set currentTime(v) {
      var p = Math.max(0, Number(v) || 0);
      this._pos = p;
      post('seek', { channel: this.channel, position: p }).catch(function () {});
      this.dispatchEvent(new Event('seeking'));
    }
    get duration() { return this._dur; }
    get paused() { return this._paused; }
    get ended() { return this._ended; }
    get readyState() { return this._ready; }
    get error() { return this._err; }
    get volume() { return this._volume; }
    set volume(v) {
      this._volume = Math.max(0, Math.min(1, Number(v) || 0));
      post('volume', { volume: this._volume }).catch(function () {});
    }
    get playbackRate() { return this._rate; }
    set playbackRate(v) {
      this._rate = Math.max(0.25, Math.min(4, Number(v) || 1));
      post('rate', { rate: this._rate }).catch(function () {});
    }
    get defaultPlaybackRate() { return this._rate; }
    set defaultPlaybackRate(v) { this._rate = Math.max(0.25, Math.min(4, Number(v) || 1)); }
    get preload() { return 'auto'; }
    set preload(_v) {}

    setAttribute(name, value) {
      super.setAttribute(name, value);
      if (name === 'src') this.src = value;
    }
    removeAttribute(name) {
      super.removeAttribute(name);
      if (name === 'src') { this._src = ''; this._stop(); }
    }
    load() {
      if (!this._src) return;
      this._loading = true;
      this.dispatchEvent(new Event('loadstart'));
      post('load', { channel: this.channel, src: this._src }).catch(function () {});
      this._startPoll();
    }
    _stop() {
      this._paused = true;
      this._loading = false;
      post('stop', { channel: this.channel }).catch(function () {});
    }
    play() {
      var self = this;
      if (!this._src) return Promise.reject(new DOMException('No audio source', 'NotSupportedError'));
      this._paused = false;
      this._ended = false;
      this._loading = true;
      this.dispatchEvent(new Event('play'));
      this._startPoll();
      return post('play', { channel: this.channel, src: this._src })
        .then(function () {
          self._ready = 4;
          self.dispatchEvent(new Event('canplay'));
          self.dispatchEvent(new Event('play'));
          self.dispatchEvent(new Event('playing'));
        })
        .catch(function (e) {
          self._paused = true;
          self._loading = false;
          self._err = { code: 4, message: String(e && e.message || e) };
          self.dispatchEvent(new Event('error'));
          throw e;
        });
    }
    pause() {
      if (this._paused) return;
      this._paused = true;
      post('pause', { channel: this.channel }).catch(function () {});
      this.dispatchEvent(new Event('pause'));
    }

    _startPoll() {
      var self = this;
      if (self._timer) return;
      self._timer = setInterval(function () {
        getStatus(true).then(function (s) {
          if (!s) return;
          var mine = !s.channel || s.channel === self.channel;
          if (!mine) return;
          var dur = Number(s.duration) || 0;
          if (dur > 0 && Math.abs(dur - self._dur) > 0.2) {
            var first = self._dur === 0;
            self._dur = dur;
            self._ready = Math.max(self._ready, 1);
            self.dispatchEvent(new Event('durationchange'));
            if (first) self.dispatchEvent(new Event('loadedmetadata'));
          }
          if (typeof s.position === 'number' && Math.abs(s.position - self._pos) > 0.12) {
            self._pos = s.position;
            self.dispatchEvent(new Event('timeupdate'));
          }
          if (s.loading) {
            self._loading = true;
            return;
          }
          if (self._loading) {
            self._loading = false;
            self._ready = 4;
            self.dispatchEvent(new Event('canplay'));
            self.dispatchEvent(new Event('canplaythrough'));
            if (!s.playing && !s.paused) self.dispatchEvent(new Event('playing'));
          }
          if (s.playing && self._paused) {
            self._paused = false;
            self.dispatchEvent(new Event('playing'));
          }
          if (s.error) {
            self._err = { code: 4, message: s.error };
            self._paused = true;
            self.dispatchEvent(new Event('error'));
          }
          if (s.ended && !self._ended) {
            self._ended = true;
            self._paused = true;
            self._pos = self._dur || self._pos;
            self.dispatchEvent(new Event('ended'));
            clearInterval(self._timer);
            self._timer = null;
          }
        });
      }, 250);
    }
  }

  if (!customElements.get('wl-audio')) customElements.define('wl-audio', NativeAudio);
  console.info('[wavelength] native audio bridge active');
})();
