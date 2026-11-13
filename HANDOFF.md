# Handoff — 2.9.0 (Minecraft link + GitHub updates)

## Verified here (no Rust toolchain, no network)

- `npm test`: 28/28 (20 existing + 8 for `public/js/bridge.js`, run against a fake core that behaves like the Rust one).
- Release scripts (`bump-version`, `release-notes`, `setup-github`) run on a scratch copy; both workflows parse as YAML.
- The mod (`../modv4-wavelength-link`): `tools/run-selftest.sh` passes: 59 core checks plus the real `BridgeClient`
  against a stand-in for the Rust link (NATIVE detected, port found from a stale 38421 config, commands carry the header).

## NOT verified — do these first, in order

1. `cargo build` then `cargo test --manifest-path src-tauri/Cargo.toml`. `bridge.rs` and `updater.rs` were written
   without a compiler. Likeliest fixes: Tauri updater API details in `updater.rs` (`Update::download/install`,
   `UpdaterExt::updater`), `tauri::Config.plugins.0` in `updater::configured`, and the `json!` casts in `bridge.rs`.
   `bridge.rs` has 14 unit tests (guard, queue, sanitising, bands, engine overlay) that have never run.
   Commit the `Cargo.lock` this produces (new dependencies); do not use `--locked` before that.
2. `npm run core`, then in a browser at http://127.0.0.1:47823 : play a song and
   `curl -H 'X-Wavelength: 1' -X POST -d '{"cmd":"next"}' http://127.0.0.1:47823/api/bridge/cmd` should skip it;
   `curl http://127.0.0.1:47823/api/bridge/state` should show the track, a moving `pos` and 24 `bands`.
   Also confirm a request with `-H 'Origin: https://example.com'` gets 403.
3. Start the mod (`./gradlew runClient`) with the app open: `/wavelength status` should say `NATIVE`, and the HUD,
   seek, volume and the level meter should work. The mod's Gradle build and in-game rendering have still never
   been run (see its README).
4. GitHub: `node scripts/setup-github.js OWNER/REPO key.pub`, add the two secrets (UPDATES.md), push tag `v2.9.0`,
   then install it and publish `2.9.1` to see the in-app update. `ubuntu-22.04-arm` and the macOS DMG step are the
   likeliest CI surprises; drop the arm row from the matrix if it is not available to your account.

## Assumptions to confirm

- Repositories: app `thegoldertrone7-cyber/wavelength-tauri`, mod `thegoldertrone7-cyber/wavelength-link`.
  Change in `tauri.conf.json` (setup-github.js) and `gradle.properties` (`mod_github_repo`).
- The updater is off until a public key is in `tauri.conf.json`; the release workflow refuses to run without it.

## Not done / ideas

- A Minecraft status line in Settings (the core already knows when the mod is watching: `Bridge::is_watched`).
- Bundling or auto-installing the mod jar from the app.
- WebP artwork still falls back to the placeholder in the mod (Java cannot decode it); the core could convert it.
- The rest of the existing gaps in PORTING.md (Spotify import, LAN discovery, media keys).
- Side note: the whole API (not just the bridge) sends permissive CORS headers, so any web page in your browser
  can call e.g. `/api/state`. The bridge is guarded against this; the rest is worth tightening.
