# Handoff — 2.9.6 (ported 2.10 feature set on the 2.9.2 base)

2.9.6 takes the features that were being prepared as 2.10.0 and ports them onto the hardened 2.9.2 release.
The 2.9.1/2.9.2 durability, lyrics, sync, updater and Flatpak fixes are all present; none were dropped.

## Verified for this release

- `npm test`: 52/52 (adds 7 artist-splitting tests to the 45 from 2.9.4).
- `cargo test` and `cargo test --locked`: 38/38 (store recovery/durability, lyrics service and generation,
  bridge 14, diag). `Cargo.lock` is committed and up to date — do not delete it.
- `node --check` clean on all 18 JS files; `git diff --check` clean.
- Versions consistent at 2.9.6 in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`,
  `src-tauri/tauri.conf.json`, `api.rs::VERSION` and the release workflow's tag gate.
- Updater public key present in `tauri.conf.json`; endpoints point at the public GitHub `latest.json`.
- Flatpak job in `.github/workflows/release.yml` plus the standalone `flatpak.yml`; `scripts/build-flatpak.sh`
  is the Tauri/GNOME build (no Electron).

## What shipped in 2.9.6

- Settings → Edited song details now resets titles, artists or cover art on their own or all at once, restoring
  stored originals everywhere and supporting undo.
- Desktop visualizer: browser-like spectrum scaling, spread-out bass bars, ~22 Hz engine readings eased at frame
  rate, real output samples for waveforms, and a fallback for webviews without Web Audio.
- Smarter recap artists (individual artists, variants folded together) and cross-device recap sync with a Sync now button.
- Themed video in the full-screen player, using the current theme's colours, dots and corner shape.
- Linux: software compositing by default (`WL_GPU=1` restores acceleration), faster playlist highlighting, and a crash guard that turns on Safe playlist mode.
- Updater: real install/restart errors, download progress, "Update and restart", Flatpak-aware messaging and the correct release link.
- The 2.9.3 macOS page-open failure is fixed: the router banner is closed again, so `routeTok` is defined
  and every page opens. A regression test guards the banner/declaration pair.
- Native four-provider lyrics service shared by the page (`/api/lyrics`) and the Minecraft mod
  (`/api/bridge/lyrics`), with durable on-disk cache, translations, match info and a per-song timing control.
- Opt-in on-device Whisper lyric generator behind `cargo build --features whisper`. Default builds do not
  include it and work normally. Generated lines are labelled "may contain mistakes".
- Linux WebKitGTK playlist hardening (no view transitions / deferred painting / DMA-BUF renderer, chunked
  rows, lite mode, capped yt-dlp concurrency, breadcrumb logging to `wavelength.log` in the data folder).

## Known limitations / not verified locally

- The `whisper` feature build was **not compiled here**: `cmake` is not installed in this environment. The
  default-feature build and all tests pass. If a release should include the generator, build with
  `--features whisper` in CI (the release workflow uses default features, so shipped binaries do not include
  the generator). Enabling it by default needs a build with cmake present and an API check against whisper-rs 0.14.
- The provider response shapes (LRCLIB, NetEase, KuGou, lyrics.ovh) were written against the services' public
  APIs and unit-tested, but live network calls were not exercised in this environment. `tried` in a lyrics
  reply lists which providers answered; use
  `curl 'http://127.0.0.1:47823/api/lyrics?title=…&artist=…&duration=…'` to check one.
- The Linux playlist fix is inferred from the WebKitGTK view-transition behaviour, not reproduced on Linux
  here (this machine is macOS). The `wavelength.log` breadcrumbs (`open playlist`, `drawn playlist`,
  `error …`) are there if it still misbehaves; send that file.
- Flatpak cannot be built or run on macOS. Validate the flatpak artifact from the release workflow.
- Mod performance idea from the old WIP (cache `Link.resolveTheme()` per frame) was not ported; it is not a
  blocker.

## Repositories and release

- App: `thegoldertrone7-cyber/wavelength-tauri`; mod: `thegoldertrone7-cyber/wavelength-link`.
- Ports: HTTP core 47823, LAN sync 47824, UDP discovery 38423.
- Tauri identifier `app.wavelength.music.tauri` (Flatpak id `app.wavelength.music`).
- Tag `v2.9.6` and push to run the public release workflow; it publishes macOS/Windows/Linux bundles,
  signatures, `latest.json` and the Flatpak, gated on the version match and updater key.
