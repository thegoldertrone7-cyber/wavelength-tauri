# Wavelength Tauri port — progress snapshot

Snapshot date: 2026-11-11. The original app in `../wavelength` is untouched.

## What works now

- Tauri 2 shell with the existing web UI (no layout or styling fork).
- Rust core (`src-tauri/`) serves the UI on `127.0.0.1:47823` and implements the
  API the UI already calls: state, health, search, info, related, stream, warm,
  cached, local files (list/upload/delete/stream), cover uploads, downloads,
  silence detection, cache clear, LAN sync (host/join/push/pull/merge/listening),
  and native audio control.
- Native playback (`src-tauri/src/audio.rs`): `yt-dlp` resolves and caches the
  m4a, symphonia decodes it through rodio 0.23, CoreAudio outputs it, and a
  sample tap feeds a Rust FFT for the real visualizer. Play/pause/seek/volume/
  rate/gapless preload all go through the engine.
- The web UI is unchanged apart from one script: `public/js/native-audio.js`
  defines `<wl-audio>`, forwards the media-element API to the engine, mirrors
  the UI's Web Audio `GainNode` volume, and feeds the analyser from `/api/audio/spectrum`.
- Cross-device recap: Rust `store.rs` clamps bad timestamps, merges listening
  stats on every state write, ties by listening weight, and exposes
  `POST /api/sync/listening`, so phone stats show up on the Mac.
- `npm test` passes 20/20. Debug and release builds compile without warnings.

## Run it

```bash
cd wavelength-tauri
npm run dev            # Tauri window
npm run core           # headless core only, then open http://127.0.0.1:47823
npm test
```

`yt-dlp` and `ffmpeg` are found in `/opt/homebrew/bin`, `/usr/local/bin`, and
`PATH`; override with `WL_YTDLP`, `WL_FFMPEG`, `WL_FFPROBE`.

## Not done yet

- Spotify import (intentionally stubbed), LAN UDP discovery, shared-folder sync
  (Host/Join works), embedded local cover extraction, byte-level download
  progress, Opus/WebM, live rate changes, system Now Playing/media keys, and
  the native update channel. Details and priorities are in `PORTING.md`.
- Android is still the Kotlin/Media3 app; the Tauri port is desktop only.
- The Minecraft mod compatibility and the alternative music platform are both
  still waiting on scope details.

## Build a macOS app

```bash
export PATH="$HOME/.cargo/bin:$PATH"
npx @tauri-apps/cli@^2 build --bundles app
# -> src-tauri/target/release/bundle/macos/Wavelength.app
```

A prebuilt copy of this snapshot is at
`~/Downloads/Wavelength-2.8.0-tauri.app.zip`. The `.dmg` step needs Finder
automation, which times out in headless sessions; the `.app` itself builds and
runs.
