# Updates from GitHub

Installed apps update themselves from GitHub Releases. **Settings → Updates** checks on launch (at most every
12 hours), downloads the signed bundle in the background and shows **Restart now**. Nothing is installed until
you restart, and the signature is checked before anything is touched.

How it fits together:

    git tag v2.9.6 && git push --tags
      → .github/workflows/release.yml builds macOS (universal), Windows, Linux x64 and Linux arm64,
        signs each updater bundle, uploads installers and latest.json to a draft release, then publishes it
      → installed apps read  https://github.com/OWNER/REPO/releases/latest/download/latest.json
        and install the bundle for their platform if the version is newer

## One-time setup

1. **Create the signing key.** Keep the private half secret; losing it means installed apps will refuse every
   later update.

       npx tauri signer generate -w ~/.tauri/wavelength.key

2. **Put the public key and your repository in the app.**

       node scripts/setup-github.js OWNER/REPO ~/.tauri/wavelength.key.pub

   This edits `src-tauri/tauri.conf.json` (`plugins.updater.endpoints` and `.pubkey`). Commit it. The default
   repository there is `thegoldertrone7-cyber/wavelength-tauri`; change it if yours differs.
3. **Add two repository secrets** (Settings → Secrets and variables → Actions):
   `TAURI_SIGNING_PRIVATE_KEY` (the *contents* of `wavelength.key`) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
   (the password you chose; leave empty if none).
4. Settings → Actions → General → Workflow permissions: **Read and write**.

Until step 2 is done the updater stays off: the app runs normally and Settings → Updates says
"Updates are not set up in this build". The release workflow refuses to run without the key, so you cannot
publish a release that nobody can install.

## Shipping an update

    npm run version:bump 2.9.6      # package.json, tauri.conf.json, Cargo.toml, Cargo.lock, api.rs
    # add "## 2.9.6" to CHANGELOG.md (it becomes the release notes shown in Settings → Updates)
    git commit -am "2.9.6" && git tag v2.9.6 && git push && git push --tags

The workflow checks that the tag, `package.json`, `tauri.conf.json` and `Cargo.toml` agree before building.
Builds run one platform at a time on purpose, because each one merges itself into the same `latest.json`.
The first release after setup has to be installed by hand; every later one arrives in-app.

## Notes

- **Windows**: the installer runs in passive mode and closes the app while it installs, then starts it again.
- **macOS**: the app bundle is replaced in place, so Wavelength must live somewhere you can write to
  (`/Applications` is fine for the user who installed it). Builds are unsigned unless you add Apple
  certificates to the workflow, so the first launch needs right-click → Open.
- **Linux**: in-app updates work for the AppImage. `.deb`/`.rpm`/Flatpak installs update through their own
  channels.
- **Android** is the separate Kotlin app and keeps its own update channel.
- `Cargo.lock` is committed and kept up to date; `cargo test --locked` passes. Regenerate and commit it after
  changing dependencies.
