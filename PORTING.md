# Wavelength — Tauri 2 / Rust core port

This folder is the isolated port. The shipping app in `../wavelength` is
untouched, so nothing here can break the released build. The web UI is a
byte-for-byte copy of `public/` except for three deliberate changes:

- `index.html` loads `js/native-audio.js` before `app.js`
- `<audio>` elements became `<wl-audio>` custom elements
- nothing else in the UI or feature code was rewritten

## What actually changed

| Layer | Before | Now |
| --- | --- | --- |
| Shell | Electron (`electron/main.js`) | Tauri 2 (`src-tauri/`) |
| HTTP API | Node `server.js` | Rust `axum` (`src/api.rs`) |
| Persistence | `state.json` on disk | SQLite (`data/wavelength.db`) + `state.json` mirror |
| Playback | Chromium `<audio>` + Web Audio | `rodio` → `symphonia` decode → `cpal` output |
| Visualizer | Web Audio analyser | FFT of the real playback tap (`rustfft`) |
| Audio input | `/api/stream/:id` proxied by Node | `/api/stream/:id` resolves with `yt-dlp`, native engine downloads + caches |
| Device sync | Node LAN host/guest | Rust LAN host on `:47824` + guest client |

The UI still calls the same 40-ish `/api/*` routes and still thinks it is
talking to an `<audio>` element. `public/js/native-audio.js` is the shim:
it forwards `src`, `play()`, `pause()`, `currentTime`, `volume`,
`playbackRate`, `load()`, `readyState`, `duration`, `error` and the media
events to the Rust engine. It also watches the Web Audio `GainNode` the UI
connects to `destination` and mirrors its value into the native engine, so
the volume slider, mute key and smart-volume attenuation still work even
though the samples never pass through Web Audio.

## Native audio path

1. UI sets `wl-audio.src = "/api/stream/<ytId>?q=…"` and calls `play()`.
2. `/api/audio/play` tells the engine thread which URL and channel (`a`/`b`).
3. A worker resolves the direct YouTube audio URL (`yt-dlp -g`, preferring
   m4a/AAC) and downloads it once into
   `~/Library/Application Support/Wavelength/cache/audio/<ytId>.m4a`.
   YouTube downloads go through `yt-dlp` itself because raw googlevideo URLs
   are bound to the requesting client and answer 403 to a plain HTTP client.
4. The audio thread builds a `rodio::Player`, appends the decoder wrapped in a
   `Tap` source, and starts playback. `Decoder::try_from(File)` (rodio 0.23)
   supplies the file length, which is what makes m4a/AAC seeking work;
   rodio 0.19 panics on MP4 without it.
5. Every 120 ms the engine publishes position/playing/duration; the shim
   turns that into `timeupdate`, `loadedmetadata`, `ended`, etc.
6. `Tap` keeps the last 8192 samples; `GET /api/audio/spectrum` runs a 2048
   point FFT (`rustfft`) and returns 256 bins for the visualizer and
   smart-volume code. Web Audio's `createAnalyser` / `createMediaElementSource`
   are patched in the shim so the UI graph still builds without a media
   element.

Seeking uses `Player::try_seek` on the cached file. Gapless preloading works by
having the spare element call `load()`; the engine downloads and registers the
file without interrupting the active sink.

## Listening recap fix (carried into Rust)

The old Node core clamped every timestamp with `Math.min(1e12, …)`, so a real
`Date.now()` (~1.79e12) became 2001-09-09 and a stale Mac snapshot could
overwrite a phone's newer statistics. Both problems are fixed in the Rust core:

- timestamps are clamped at `864e13` (`store::stamp`), not `1e12`
- `mergeListening` keeps the snapshot with more listening time when two
  devices tie on timestamp (`store::listening_weight`)
- **every** state write merges `listening` instead of replacing it
  (`Store::write_state`), so a delayed full-state save cannot erase a phone
- `PUT/GET /api/sync/listening` merges into SQLite and, on a guest, forwards
  the snapshot to the host and takes the combined result back
- the LAN host merges listening from every device into the same document

That means the recap is device-independent as long as each device reaches the
host at least once; playback can happen on the phone and the Mac still shows
the combined totals.

## Run it

```bash
cd wavelength-tauri
npm install                 # only fetches the Tauri CLI
npm run dev                 # Tauri window + Rust core
# or the core alone, for UI/API work:
npm run core                # http://127.0.0.1:47823
```

Requirements: Rust stable, Node 20+, Xcode command line tools,
`yt-dlp` and `ffmpeg` on `PATH` (`brew install yt-dlp ffmpeg`). Health checks
in the UI report missing binaries instead of failing silently.

Headless mode is useful for testing the existing UI in a browser:

```bash
WL_HEADLESS=1 cargo run --manifest-path src-tauri/Cargo.toml
# open http://127.0.0.1:47823
```

The shim activates whenever the page is served from port `47823` (headless or
inside the Tauri window), so headless runs exercise the same native audio path
as the app. Opening `public/index.html` from disk or another port leaves the
shim off and the page behaves like the browser build.

## Ported API surface

Implemented: state, health, config, stats, search, match, info, related,
stream, warm, cached, silence, download(s), cache clear, local files
(list/upload/delete/stream), cover upload/delete/serve, discord stub,
Spotify stubs, update stubs, sync (status/host/join/stop/listening/push/pull/
merge), native audio control/status/spectrum.

Known gaps, in priority order:

1. **Spotify import** — intentionally stubbed. The Node implementation needs
   the Spotify Web API and a browser redirect; add it as a Rust OAuth flow.
2. **LAN discovery** (UDP broadcast) — host/join by address works; "find
   devices" returns an empty list. Add the same UDP probe as `server.js`.
3. **Sync folder** — the shared-folder transport is stubbed; use Host/Join
   (the direct LAN pair) in the native build. The `/api/sync/folder` response
   carries an explicit error so the UI can explain that.
4. **Local file extras** — `ffprobe` metadata is read, but embedded cover art
   is not extracted and there is no folder-based library scan (files live in
   `<data>/local`).
5. **Download progress percentages** — jobs report queued/downloading/done/
   error, not byte progress.
6. **Opus/WebM** — engine prefers m4a/AAC. If a video only offers Opus, add
   the `symphonia-ogg` / Opus path to `rodio` features.
7. **Playback-rate changes during a track** restart decoding at the current
   position instead of resampling live.
8. **Media keys / Now Playing** — `navigator.mediaSession` is still wired by
   the UI, but Tauri needs an `MPNowPlayingInfoCenter` bridge for real system
   integration.
9. **Update channel** — native builds update as an app bundle; the web-layer
   updater is intentionally disabled.

## Build a bundle

```bash
npm run build
# or: npx @tauri-apps/cli@^2 build
```

The bundle embeds `public/` as a resource and the Rust core serves it from
`127.0.0.1:47823`. The app never exposes that port off-device; the separate
sync listener on `:47824` is only started when the user hosts.

## Deliberate non-goals for this pass

- The alternative music platform is still deferred.
- The Android shell is untouched; a future Android core should be Kotlin +
  Media3 rather than embedding this desktop stack.
- No Minecraft mod work was started because the loader/version/behaviour
  questions are still unanswered.
