# Changelog

## 2.9.4

- **Fixed a startup/page-open failure on macOS.** A missing `*/` in the router section of the page script commented out
  the route state declaration, so WebKit reported “This page could not be opened. Can't find variable:routeTok”.
  The header is closed again and a regression test guards it. Everything else in 2.9.3 is unchanged.

## 2.9.3

This release brings the features that were being prepared for 2.10.0 onto the hardened 2.9.2 base, so none of
the reliability fixes from 2.9.1 and 2.9.2 are lost. Better lyrics across four databases, in-game lyrics,
smoother Linux playlists, and an opt-in on-device lyric generator — with the durable saves, working updater
and Flatpak build from 2.9.2 intact.

- **A native lyrics service.** The Rust core asks LRCLIB, NetEase Music, KuGou and lyrics.ovh at once, scores each
  answer by title, artist and length (so a different cut of the song or a different song is rejected), and keeps
  the best. It stops early on a near-exact synced match, caches results on disk with a durable miss TTL, and
  keeps translations when NetEase has them. Android, which has no core, still asks LRCLIB directly.
- **One lookup for page and mod.** `lyrics::lookup` serves `/api/lyrics` and `/api/bridge/lyrics`, so the player
  and the Minecraft mod always show the same lines. "Try again" forgets the cached miss and asks every provider
  again.
- **Lyrics view.** The panel shows where the lyrics came from and what record matched, with translations under
  each line and a per-song timing control (±0.25 s, remembered for that song) that respects the file's own
  `[offset:]` tag. The next song's lyrics load while the current one plays; plain-lyrics and instrumental
  states are reported clearly.
- **Lyrics in Minecraft.** `/api/bridge/lyrics` gives the Wavelength Link mod 1.2.0 the synced lines for the
  playing track, found by the same service, for its in-game lyrics overlay.
- **Lyric generator (opt-in build).** When no lyric database has a song, the core can listen to it and write the
  lyrics down on the device with Whisper, with time stamps, and cache them like any other lyrics. It runs on its
  own thread; `/api/lyrics` reports `pending`, `stage`, `progress` so the page shows a progress bar and the mod
  shows "Writing the lyrics… NN%". Settings → Lyrics has the toggle, the model (tiny / base / small, downloaded
  once from Hugging Face) and a Download button; the song page has a **Generate lyrics** button. The speech
  engine needs cmake and a C++ compiler, so it is behind `cargo build --features whisper`; without it the rest
  still builds and Settings says so. Generated text is cleaned (bracketed sounds, "thanks for watching",
  looping repeats are dropped) and labelled "may contain mistakes".
- **Linux playlists, second pass.** Opening a playlist on the Linux webview now draws without entrance animation,
  deferred painting or promoted layers, loads 25 rows at a time, and does not resolve songs in the background.
  The router can no longer throw (a bad id used to leave the old page up), and if the playlist page has not
  appeared after 1.2 s a plain, image-free version of it is drawn. WebKitGTK's DMA-BUF renderer is disabled by
  default to avoid blank windows and web-process crashes.
- **Playlists no longer vanish on Linux.** Library changes still reach the disk immediately, the state is flushed
  again when the window closes, SQLite waits for the write to complete, and `state.json` is written atomically.
  A fresh install adopts the newest library left behind by another install layout (Flatpak, deb, AppImage), with
  an in-app restore from a local backup when the core comes back empty.
- **Device sync.** Hosting lists every real network address, answers the UDP discovery probe on port 38423,
  serves the legacy `/api/sync/*` paths and `X-Wavelength-Sync` header for older clients, uses letter-and-digit
  pairing codes, and resumes automatically when the app reopens.
- **Updates and Flatpak intact.** The signed in-app updater and the Flatpak release bundle from 2.9.2 are carried
  forward unchanged.

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
