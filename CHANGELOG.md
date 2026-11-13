# Changelog

## 2.9.0

- **Minecraft, built in.** The Rust core now answers the Wavelength Link mod's `/api/bridge/*` link itself, so
  the mod works without `gamelink.patch`. Position, play state and duration come from the native audio engine,
  and the level meter is the engine's real FFT. Commands reach the app in milliseconds through a long poll.
  Requests are limited to this computer and this app's own page.
- **Updates from GitHub.** Settings → Updates now checks GitHub Releases, downloads a signed bundle and
  restarts into it. Releases are built, signed and published for macOS, Windows and Linux by GitHub Actions.
- **Flatpak for Linux.** Releases now include a `Wavelength-2.9.0-x86_64.flatpak` that bundles `yt-dlp`,
  `ffmpeg` and `ffprobe`, so a sandboxed install plays without extra setup. Flatpak updates come from the
  next bundle; the in-app updater is for the macOS, Windows and AppImage/deb builds.
- Page script `public/js/bridge.js` supplies track, theme and like state to the core.

## 2.8.0

- Tauri 2 shell with the Rust core and native audio.
