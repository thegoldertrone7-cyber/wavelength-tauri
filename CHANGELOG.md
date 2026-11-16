# Changelog

## 2.9.2

- **Playlists no longer vanish on Linux.** Library changes used to be batched for a fraction of a second
  before saving; closing the window in that gap lost the last change, and the Flatpak and deb/AppImage
  builds kept separate libraries, so switching between them looked like an empty account. Saves now
  reach the disk immediately, the state is flushed again when the window closes, SQLite waits for the
  write to complete, and `state.json` is written atomically. If a freshly started install finds no
  playlists, it adopts the newest library left behind by another install layout, covers included.
- **Better lyrics.** Lookups now run in the Rust core with a durable on-disk cache, so lyrics appear
  instantly on replay and still work offline. Matching handles upload titles such as
  "Artist - Song (Official Video) feat. …", prefers synced lyrics and rejects results whose length is
  far from the song. The panel says which record matched and offers a per-song timing control
  (±0.25 s, remembered for that song) that respects the file's own `[offset:]` tag. The next song's
  lyrics load while the current one plays.
- Plain-lyrics and instrumental states are reported clearly, and a failed lookup can be retried without
  reopening the app.

## 2.9.1

- **Device sync works again.** Hosting now lists every real network address (Wi-Fi, Ethernet, Tailscale)
  instead of falling back to `127.0.0.1`, so the Devices panel no longer says "No network address found"
  while the computer is online.
- **Find devices works.** The Rust core answers the same UDP discovery probe on port 38423 as the old
  server and advertises the sync port, so phones and other computers can find the host automatically.
- **Older clients can pair.** The LAN server also serves the `/api/sync/*` paths and the
  `X-Wavelength-Sync` header used by the Android app and earlier desktop builds, and returns the merged
  library from a join.
- **Pairing codes are letters and digits only.** The generator could emit punctuation, which the phone
  stripped, making a correct code look wrong.
- Hosting resumes automatically when the app reopens, and the desktop "Find devices" button probes the
  network instead of always returning nothing.

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
