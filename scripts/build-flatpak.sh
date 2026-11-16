#!/usr/bin/env bash
# Build a Flatpak bundle of the Tauri app.
#
# Flatpak builds need Linux with flatpak and flatpak-builder; the release
# workflow runs this on ubuntu-22.04 after building the Rust binary. macOS and
# Windows keep using the normal Tauri bundles.
#
#   bash scripts/build-flatpak.sh
#   -> dist/Wavelength-<version>-<arch>.flatpak
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(node -p "require('./package.json').version")"
ARCH="$(uname -m)"                       # x86_64 or aarch64
BIN="$ROOT/src-tauri/target/release/wavelength"
APP_ID="app.wavelength.music"
# GNOME 48 is the newest runtime we can rely on for webkit2gtk-4.1, which is
# what Tauri 2 links against. Bump this only after checking the runtime still
# ships libwebkit2gtk-4.1.so.0.
RUNTIME_VERSION="${FLATPAK_GNOME_VERSION:-48}"

if ! command -v flatpak-builder >/dev/null 2>&1; then
  echo "Flatpak builds run on Linux with flatpak and flatpak-builder installed." >&2
  echo "On macOS/Windows use: npm run build" >&2
  exit 1
fi

case "$ARCH" in
  x86_64)
    YTDLP_ASSET="yt-dlp_linux"
    FFMPEG_ASSET="ffmpeg-master-latest-linux64-gpl.tar.xz"
    ;;
  aarch64|arm64)
    ARCH="aarch64"
    YTDLP_ASSET="yt-dlp_linux_aarch64"
    FFMPEG_ASSET="ffmpeg-master-latest-linuxarm64-gpl.tar.xz"
    ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

if [ ! -x "$BIN" ]; then
  echo "No Linux binary yet; building it first (this takes a few minutes)…"
  npx tauri build --no-bundle
fi

mkdir -p "$ROOT/vendor" "$ROOT/dist" "$ROOT/flatpak"
rm -rf "$ROOT/flatpak-build" "$ROOT/flatpak-repo" "$ROOT/flatpak/payload"
mkdir -p "$ROOT/flatpak/payload"

# The app finds its interface at <exe_dir>/../lib/Wavelength/public, which is
# the same layout the deb and AppImage packages use.
cp "$BIN" "$ROOT/flatpak/payload/wavelength"
cp -a "$ROOT/public" "$ROOT/flatpak/payload/public"

# Flatpak cannot assume the host has yt-dlp or ffmpeg, so bundle static builds.
YTDLP="$ROOT/vendor/yt-dlp_linux"
if [ ! -x "$YTDLP" ]; then
  echo "Downloading yt-dlp…"
  curl -L --fail --retry 3 -o "$YTDLP" \
    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/$YTDLP_ASSET"
  chmod +x "$YTDLP"
fi

FFMPEG_TAR="$ROOT/vendor/ffmpeg.tar.xz"
if [ ! -f "$FFMPEG_TAR" ]; then
  echo "Downloading ffmpeg…"
  curl -L --fail --retry 3 -o "$FFMPEG_TAR" \
    "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$FFMPEG_ASSET"
fi

cat > "$ROOT/flatpak/$APP_ID.desktop" <<'EOF'
[Desktop Entry]
Name=Wavelength
Comment=Free, ad-free music player
Exec=wavelength
Icon=app.wavelength.music
Terminal=false
Type=Application
Categories=Audio;Music;Player;
Keywords=music;player;youtube;
StartupWMClass=Wavelength
EOF

cat > "$ROOT/flatpak/$APP_ID.yml" <<EOF
app-id: $APP_ID
runtime: org.gnome.Platform
runtime-version: '$RUNTIME_VERSION'
sdk: org.gnome.Sdk
command: wavelength
finish-args:
  - --share=network
  - --share=ipc
  - --socket=wayland
  - --socket=fallback-x11
  - --socket=pulseaudio
  - --device=dri
  - --filesystem=xdg-music:ro
  - --filesystem=xdg-download
  # Read-only view of the library a deb/AppImage install keeps outside the
  # sandbox, so starting the Flatpak can bring those playlists across.
  - --filesystem=~/.local/share/app.wavelength.music.tauri:ro
modules:
  - name: yt-dlp
    buildsystem: simple
    build-commands:
      - install -Dm755 yt-dlp_linux /app/bin/yt-dlp
    sources:
      - type: file
        path: ../vendor/yt-dlp_linux

  - name: ffmpeg
    buildsystem: simple
    build-commands:
      - mkdir -p ff
      - tar -xJf ffmpeg.tar.xz -C ff --strip-components=1
      - install -Dm755 ff/bin/ffmpeg /app/bin/ffmpeg
      - install -Dm755 ff/bin/ffprobe /app/bin/ffprobe
    sources:
      - type: file
        path: ../vendor/ffmpeg.tar.xz

  - name: wavelength
    buildsystem: simple
    build-commands:
      - install -Dm755 wavelength /app/bin/wavelength
      - mkdir -p /app/lib/Wavelength
      - cp -a public /app/lib/Wavelength/public
      - install -Dm644 $APP_ID.desktop /app/share/applications/$APP_ID.desktop
      - install -Dm644 icon-512.png /app/share/icons/hicolor/512x512/apps/$APP_ID.png
    sources:
      - type: dir
        path: payload
      - type: file
        path: $APP_ID.desktop
      - type: file
        path: ../build/icon-512.png
EOF

if ! flatpak info --user "org.gnome.Sdk//$RUNTIME_VERSION" >/dev/null 2>&1; then
  echo "Installing the GNOME SDK $RUNTIME_VERSION (once, several hundred MB)…"
  flatpak remote-add --user --if-not-exists flathub \
    https://dl.flathub.org/repo/flathub.flatpakrepo
  flatpak install --user -y --noninteractive flathub \
    "org.gnome.Sdk//$RUNTIME_VERSION" "org.gnome.Platform//$RUNTIME_VERSION"
fi

flatpak-builder --user --install-deps-from=flathub --disable-rofiles-fuse \
  --force-clean --repo="$ROOT/flatpak-repo" \
  "$ROOT/flatpak-build" "$ROOT/flatpak/$APP_ID.yml"

OUT="$ROOT/dist/Wavelength-${VERSION}-${ARCH}.flatpak"
flatpak build-bundle "$ROOT/flatpak-repo" "$OUT" "$APP_ID"
ls -lh "$OUT"
