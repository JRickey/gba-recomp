#!/usr/bin/env bash
# Stage and archive the release bundles for one target:
#
#   gba-recomp-cli      the static-recompiler toolkit (recomp + gba-pack)
#                       for building and packaging recomps
#   gba-recomp-launcher the player UI (gba-launcher + the recomp it spawns)
#                       as a native desktop artifact where possible
#
# `recomp` ships in both because it is both the build CLI and the play
# runtime. Every artifact carries the dual license and the third-party
# notice. The CLI remains the portable unzip/untar bundle; the launcher is
# wrapped as a macOS drag-install DMG, Linux AppImage/Flatpak, or Windows zip.
#
# Runs under bash on all three CI runners (macOS, Linux, and Windows via
# git-bash). Binaries must already be built under $BINDIR — and on macOS
# already lipo'd to a universal binary and ad-hoc signed (arm64 refuses
# to run unsigned). Env in: VERSION, TARGET, BINDIR, OUTDIR, EXE,
# RUNNER_OS.
set -euo pipefail

: "${VERSION:?}" "${TARGET:?}" "${BINDIR:?}" "${OUTDIR:?}"
EXE="${EXE:-}"
APP_ID="io.github.gba_recomp.launcher"
APP_NAME="gba-recomp"
LAUNCHER_BASE="gba-recomp-launcher-${VERSION}-${TARGET}"
FLATPAK_RUNTIME_VERSION="${FLATPAK_RUNTIME_VERSION:-25.08}"
DMG_BACKGROUND="${DMG_BACKGROUND:-assets/macos_dmg_banner.png}"
DMG_BG_LONG="${DMG_BG_LONG:-600}"

mkdir -p "$OUTDIR" stage

carry_licenses() { cp LICENSE-MIT LICENSE-APACHE THIRD-PARTY.md "$1"/; }

# Stage the bundled TinyCC fallback compiler beside the binaries, so a local
# "build to play" works on a machine with no system C compiler. $TCC_DIR is a
# prebuilt `tcc/` (scripts/build-tcc.sh on Unix, the win32 build on Windows).
# Without it the bundle still works wherever a system compiler exists; warn so
# a missing fallback is never silent.
stage_tcc() {
  local dir="$1"
  if [ -n "${TCC_DIR:-}" ] && [ -d "$TCC_DIR" ]; then
    rm -rf "$dir/tcc"; cp -R "$TCC_DIR" "$dir/tcc"
  else
    echo "package-release: WARNING: \$TCC_DIR unset/missing — '$dir' ships without the TinyCC fallback; local builds there need a system C compiler" >&2
  fi
}

# $1 = bundle short name, rest = binaries to include. Echoes the staged dir.
stage_bundle() {
  local short="$1"; shift
  local dir="stage/${short}-${VERSION}-${TARGET}"
  rm -rf "$dir"; mkdir -p "$dir"
  for b in "$@"; do cp "$BINDIR/${b}${EXE}" "$dir"/; done
  stage_tcc "$dir"
  carry_licenses "$dir"
  printf '%s' "$dir"
}

# $1 = staged dir
archive() {
  local dir="$1" base
  base="$(basename "$dir")"
  if [ "${RUNNER_OS:-}" = "Windows" ]; then
    (cd stage && 7z a -tzip -bso0 "$(pwd)/../$OUTDIR/${base}.zip" "${base}") >/dev/null
    echo "${OUTDIR}/${base}.zip"
  else
    tar -C stage -czf "${OUTDIR}/${base}.tar.gz" "${base}"
    echo "${OUTDIR}/${base}.tar.gz"
  fi
}

launcher_readme() {
  cat <<EOF
gba-recomp launcher ${VERSION} (${TARGET})

Open ${APP_NAME} and select your cartridge image and BIOS. The launcher
spawns the bundled 'recomp' runtime beside it and uses gamedb.sqlite to seed
full native first-launch builds by ROM sha256.

This bundle contains no game or BIOS data; those are supplied by you at
runtime and verified by hash.

Licensed MIT OR Apache-2.0 (LICENSE-MIT / LICENSE-APACHE). gamedb.sqlite is
dedicated to the public domain under CC0 (LICENSE-CC0). Third-party components
are listed in THIRD-PARTY.md.
EOF
}

stage_launcher_payload() {
  local dir="$1"
  mkdir -p "$dir"
  cp "$BINDIR/gba-launcher${EXE}" "$dir"/
  cp "$BINDIR/recomp${EXE}" "$dir"/
  cp gamedb.sqlite gamedb.sqlite.license LICENSE-CC0 "$dir"/
  stage_tcc "$dir"
  carry_licenses "$dir"
  launcher_readme > "$dir/README-LAUNCHER.txt"
}

write_launcher_desktop() {
  local path="$1" exec_cmd="$2"
  cat > "$path" <<EOF
[Desktop Entry]
Type=Application
Name=${APP_NAME}
Comment=Static recompiling launcher
Exec=${exec_cmd}
Icon=${APP_ID}
Terminal=false
Categories=Game;
StartupNotify=true
EOF
}

write_png_icon() {
  local out="$1" size="$2"
  if command -v cargo >/dev/null && [ -f crates/launcher/src/bin/gba-icon.rs ]; then
    cargo run --quiet --profile dist -p gba-launcher --bin gba-icon -- "$out" "$size" >/dev/null
  elif command -v sips >/dev/null; then
    sips -z "$size" "$size" assets/icon/app-icon.png --out "$out" >/dev/null
  else
    echo "cannot render ${size}x${size} launcher icon: cargo or sips is required" >&2
    return 1
  fi
}

write_macos_info_plist() {
  local path="$1"
  cat > "$path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" \
"http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>             <string>${APP_NAME}</string>
  <key>CFBundleDisplayName</key>      <string>${APP_NAME}</string>
  <key>CFBundleExecutable</key>       <string>gba-launcher</string>
  <key>CFBundleIconFile</key>         <string>gba-launcher</string>
  <key>CFBundleIdentifier</key>       <string>io.github.gba-recomp.launcher</string>
  <key>CFBundlePackageType</key>      <string>APPL</string>
  <key>CFBundleShortVersionString</key> <string>${VERSION}</string>
  <key>CFBundleVersion</key>          <string>${VERSION}</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.games</string>
  <key>LSMinimumSystemVersion</key>   <string>11.0</string>
  <key>NSHighResolutionCapable</key>  <true/>
</dict>
</plist>
EOF
}

write_macos_icns() {
  local iconset="$1.iconset" out="$2"
  rm -rf "$iconset"
  mkdir -p "$iconset"
  for spec in \
    "16 icon_16x16.png" "32 icon_16x16@2x.png" \
    "32 icon_32x32.png" "64 icon_32x32@2x.png" \
    "128 icon_128x128.png" "256 icon_128x128@2x.png" \
    "256 icon_256x256.png" "512 icon_256x256@2x.png" \
    "512 icon_512x512.png" "1024 icon_512x512@2x.png"
  do
    set -- $spec
    sips -z "$1" "$1" assets/icon/app-icon.png --out "$iconset/$2" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$out"
  rm -rf "$iconset"
}

package_macos_launcher() {
  local root app
  root="stage/${LAUNCHER_BASE}"
  app="${root}/${APP_NAME}.app"
  rm -rf "$root"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  stage_launcher_payload "$app/Contents/MacOS"
  mv "$app/Contents/MacOS/README-LAUNCHER.txt" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/LICENSE-MIT" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/LICENSE-APACHE" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/LICENSE-CC0" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/THIRD-PARTY.md" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/gamedb.sqlite" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/gamedb.sqlite.license" "$app/Contents/Resources/"
  mv "$app/Contents/MacOS/tcc" "$app/Contents/Resources/"
  write_macos_info_plist "$app/Contents/Info.plist"
  write_macos_icns "stage/gba-launcher" "$app/Contents/Resources/gba-launcher.icns"

  codesign --force --sign - --timestamp=none "$app/Contents/Resources/tcc/libtcc.dylib"
  codesign --force --sign - --timestamp=none "$app/Contents/MacOS/recomp"
  codesign --force --sign - --timestamp=none "$app/Contents/MacOS/gba-launcher"
  codesign --force --sign - --timestamp=none "$app"
  codesign --verify --verbose "$app"
  codesign --verify --verbose "$app/Contents/Resources/tcc/libtcc.dylib"
  codesign --verify --verbose "$app/Contents/MacOS/recomp"
  codesign --verify --verbose "$app/Contents/MacOS/gba-launcher"

  make_macos_dmg "$app" "${OUTDIR}/${LAUNCHER_BASE}.dmg"
}

make_macos_dmg() {
  local app="$1" out="$2" dmg_stage dmg_bg_dir bg_src bg_w bg_h
  command -v create-dmg >/dev/null \
    || { echo "create-dmg not in PATH — install with: brew install create-dmg" >&2; return 1; }

  rm -f "$out"
  dmg_stage="stage/${LAUNCHER_BASE}-dmg-stage"
  dmg_bg_dir="stage/${LAUNCHER_BASE}-dmg-bg"
  bg_src="$DMG_BACKGROUND"
  [ -f "$bg_src" ] || bg_src="assets/icon/app-icon.png"
  rm -rf "$dmg_stage" "$dmg_bg_dir"
  mkdir -p "$dmg_stage" "$dmg_bg_dir"
  ditto "$app" "$dmg_stage/$(basename "$app")"

  sips -Z $((DMG_BG_LONG * 2)) "$bg_src" --out "$dmg_bg_dir/bg@2x.png" >/dev/null
  sips -Z "$DMG_BG_LONG" "$bg_src" --out "$dmg_bg_dir/bg.png" >/dev/null
  sips -s dpiWidth 72 -s dpiHeight 72 "$dmg_bg_dir/bg.png" >/dev/null
  sips -s dpiWidth 144 -s dpiHeight 144 "$dmg_bg_dir/bg@2x.png" >/dev/null
  bg_w="$(sips -g pixelWidth "$dmg_bg_dir/bg.png" | awk '/pixelWidth/ {print $2}')"
  bg_h="$(sips -g pixelHeight "$dmg_bg_dir/bg.png" | awk '/pixelHeight/ {print $2}')"
  tiffutil -cathidpicheck "$dmg_bg_dir/bg.png" "$dmg_bg_dir/bg@2x.png" \
    -out "$dmg_bg_dir/background.tiff" >/dev/null

  if [ -d "/Volumes/$APP_NAME" ]; then
    hdiutil detach "/Volumes/$APP_NAME" -force >/dev/null 2>&1 || true
  fi

  create-dmg \
    --volname "$APP_NAME" \
    --background "$dmg_bg_dir/background.tiff" \
    --window-pos 200 120 \
    --window-size "$bg_w" "$bg_h" \
    --icon-size 128 \
    --icon "$(basename "$app")" $((bg_w / 4)) $((bg_h / 2)) \
    --app-drop-link $((bg_w * 3 / 4)) $((bg_h / 2)) \
    --hide-extension "$(basename "$app")" \
    --hdiutil-quiet \
    --no-internet-enable \
    "$out" \
    "$dmg_stage"

  rm -rf "$dmg_stage" "$dmg_bg_dir"
  hdiutil verify "$out"
  echo "$out"
}

stage_linux_appdir() {
  local appdir="$1"
  rm -rf "$appdir"
  mkdir -p \
    "$appdir/usr/bin" \
    "$appdir/usr/share/applications" \
    "$appdir/usr/share/icons/hicolor/512x512/apps" \
    "$appdir/usr/share/doc/gba-recomp"
  stage_launcher_payload "$appdir/usr/bin"
  mv "$appdir/usr/bin/README-LAUNCHER.txt" "$appdir/usr/share/doc/gba-recomp/"
  mv "$appdir/usr/bin/LICENSE-MIT" "$appdir/usr/share/doc/gba-recomp/"
  mv "$appdir/usr/bin/LICENSE-APACHE" "$appdir/usr/share/doc/gba-recomp/"
  mv "$appdir/usr/bin/LICENSE-CC0" "$appdir/usr/share/doc/gba-recomp/"
  mv "$appdir/usr/bin/THIRD-PARTY.md" "$appdir/usr/share/doc/gba-recomp/"
  mv "$appdir/usr/bin/gamedb.sqlite.license" "$appdir/usr/share/doc/gba-recomp/"
  write_png_icon "$appdir/usr/share/icons/hicolor/512x512/apps/${APP_ID}.png" 512
  write_launcher_desktop "$appdir/usr/share/applications/${APP_ID}.desktop" "gba-launcher"
}

package_appimage_launcher() {
  local linuxdeploy="${LINUXDEPLOY:-./linuxdeploy-x86_64.AppImage}"
  local appdir="stage/${LAUNCHER_BASE}.AppDir"
  stage_linux_appdir "$appdir"
  rm -f ./*.AppImage
  VERSION="$VERSION" "$linuxdeploy" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/gba-launcher" \
    --executable "$appdir/usr/bin/recomp" \
    --desktop-file "$appdir/usr/share/applications/${APP_ID}.desktop" \
    --icon-file "$appdir/usr/share/icons/hicolor/512x512/apps/${APP_ID}.png" \
    --output appimage
  local built
  built="$(find . -maxdepth 1 -name '*.AppImage' -print -quit)"
  [ -n "$built" ] || { echo "linuxdeploy did not produce an AppImage" >&2; return 1; }
  mv "$built" "${OUTDIR}/${LAUNCHER_BASE}.AppImage"
  chmod +x "${OUTDIR}/${LAUNCHER_BASE}.AppImage"
  echo "${OUTDIR}/${LAUNCHER_BASE}.AppImage"
}

write_flatpak_metainfo() {
  local path="$1"
  local today
  today="$(date -u +%F)"
  cat > "$path" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<component type="desktop-application">
  <id>${APP_ID}</id>
  <metadata_license>CC0-1.0</metadata_license>
  <project_license>MIT OR Apache-2.0</project_license>
  <name>${APP_NAME}</name>
  <summary>Static recompiling launcher</summary>
  <description>
    <p>Launcher and play runtime for local cartridge images, with first-launch native recompilation seeded by ROM hash metadata.</p>
  </description>
  <launchable type="desktop-id">${APP_ID}.desktop</launchable>
  <categories>
    <category>Game</category>
  </categories>
  <releases>
    <release version="${VERSION}" date="${today}"/>
  </releases>
</component>
EOF
}

write_flatpak_manifest() {
  local manifest="$1" srcdir="$2"
  cat > "$manifest" <<EOF
app-id: ${APP_ID}
branch: stable
runtime: org.freedesktop.Platform
runtime-version: '${FLATPAK_RUNTIME_VERSION}'
sdk: org.freedesktop.Sdk
command: gba-launcher
appstream-compose: false
finish-args:
  - --share=ipc
  - --socket=wayland
  - --socket=fallback-x11
  - --socket=pulseaudio
  - --device=dri
  - --device=all
  - --filesystem=xdg-config/gba-recomp:create
  - --filesystem=xdg-cache/gba-recomp:create
modules:
  - name: gba-recomp-launcher
    buildsystem: simple
    build-options:
      no-debuginfo: true
      strip: false
    sources:
      - type: dir
        path: ${srcdir}
    build-commands:
      - install -Dm755 gba-launcher /app/bin/gba-launcher
      - install -Dm755 recomp /app/bin/recomp
      - if [ -d tcc ]; then mkdir -p /app/bin/tcc && cp -R tcc/. /app/bin/tcc/; fi
      - install -Dm644 gamedb.sqlite /app/bin/gamedb.sqlite
      - install -Dm644 ${APP_ID}.desktop /app/share/applications/${APP_ID}.desktop
      - install -Dm644 ${APP_ID}.metainfo.xml /app/share/metainfo/${APP_ID}.metainfo.xml
      - install -Dm644 ${APP_ID}.png /app/share/icons/hicolor/512x512/apps/${APP_ID}.png
      - install -Dm644 README-LAUNCHER.txt /app/share/doc/gba-recomp/README-LAUNCHER.txt
      - install -Dm644 LICENSE-MIT /app/share/doc/gba-recomp/LICENSE-MIT
      - install -Dm644 LICENSE-APACHE /app/share/doc/gba-recomp/LICENSE-APACHE
      - install -Dm644 LICENSE-CC0 /app/share/doc/gba-recomp/LICENSE-CC0
      - install -Dm644 THIRD-PARTY.md /app/share/doc/gba-recomp/THIRD-PARTY.md
      - install -Dm644 gamedb.sqlite.license /app/share/doc/gba-recomp/gamedb.sqlite.license
EOF
}

package_flatpak_launcher() {
  local root src src_abs build repo manifest
  root="stage/${LAUNCHER_BASE}-flatpak"
  src="$root/src"
  build="$root/build"
  repo="$root/repo"
  manifest="$root/${APP_ID}.yml"
  rm -rf "$root"
  mkdir -p "$src"
  stage_launcher_payload "$src"
  write_png_icon "$src/${APP_ID}.png" 512
  write_launcher_desktop "$src/${APP_ID}.desktop" "gba-launcher"
  write_flatpak_metainfo "$src/${APP_ID}.metainfo.xml"
  src_abs="$(cd "$src" && pwd -P)"
  write_flatpak_manifest "$manifest" "$src_abs"
  flatpak-builder --force-clean --user --install-deps-from=flathub --repo="$repo" "$build" "$manifest"
  flatpak build-bundle "$repo" "${OUTDIR}/${LAUNCHER_BASE}.flatpak" "$APP_ID" stable \
    --runtime-repo=https://flathub.org/repo/flathub.flatpakrepo
  echo "${OUTDIR}/${LAUNCHER_BASE}.flatpak"
}

package_linux_launcher() {
  package_appimage_launcher
  package_flatpak_launcher
}

package_portable_launcher() {
  local rt
  rt="$(stage_bundle gba-recomp-launcher gba-launcher recomp)"
  cp gamedb.sqlite gamedb.sqlite.license LICENSE-CC0 "$rt"/
  launcher_readme > "$rt/README-LAUNCHER.txt"
  archive "$rt"
}

# --- CLI toolkit -----------------------------------------------------
cli="$(stage_bundle gba-recomp-cli recomp gba-pack)"
cp README.md BUILDING.md "$cli"/
mkdir -p "$cli/docs"
cp docs/labels.md docs/packaging.md "$cli/docs"/
cat > "$cli/README-CLI.txt" <<EOF
gba-recomp CLI toolkit ${VERSION} (${TARGET})

  recomp     static recompiler + play runtime (build / runc / verify /
             labels / play). See BUILDING.md and docs/.
  gba-pack   package a mapped image into a distributable recomp.
             See docs/packaging.md.

Licensed MIT OR Apache-2.0 (LICENSE-MIT / LICENSE-APACHE). Third-party
components are listed in THIRD-PARTY.md. This bundle contains no game or
BIOS data; those are supplied by you at runtime and verified by hash.
EOF

echo "staged:"
archive "$cli"
case "${RUNNER_OS:-}" in
  macOS) package_macos_launcher ;;
  Linux) package_linux_launcher ;;
  Windows) package_portable_launcher ;;
  *) package_portable_launcher ;;
esac
