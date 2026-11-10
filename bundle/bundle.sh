#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# AGENTSMON_DIST: build somewhere else than dist/, e.g. to leave a running
# dist/AgentsMon.app alone.
APP="${AGENTSMON_DIST:-$ROOT/dist}/AgentsMon.app"
BUILD_PROFILE="${AGENTSMON_BUILD_PROFILE:-release}"

cd "$ROOT"
# The updater compares the Cargo version with the release tag, and the
# relaunch-after-update check reads Info.plist: they must agree.
./bundle/verify-version.sh
RUSTC="$(rustup which --toolchain stable rustc)" \
RUSTDOC="$(rustup which --toolchain stable rustdoc)" \
  rustup run stable cargo build --profile "$BUILD_PROFILE" -p agentsmon -p agentsmon-helper

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/bin" "$APP/Contents/Resources/codex-plugin/bin"
cp "target/$BUILD_PROFILE/agentsmon" "$APP/Contents/MacOS/agentsmon"
cp "target/$BUILD_PROFILE/agentsmon-helper" "$APP/Contents/Resources/bin/agentsmon-helper"
cp -R plugins/agentsmon-codex/. "$APP/Contents/Resources/codex-plugin/"
cp "target/$BUILD_PROFILE/agentsmon-helper" "$APP/Contents/Resources/codex-plugin/bin/agentsmon-helper"
cp bundle/Info.plist "$APP/Contents/Info.plist"
chmod +x "$APP/Contents/MacOS/agentsmon" "$APP/Contents/Resources/bin/agentsmon-helper" "$APP/Contents/Resources/codex-plugin/bin/agentsmon-helper"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  # Release build. --deep does not sign executables under Resources, and the
  # notary service rejects any unsigned Mach-O: sign both helpers first.
  SIGN=(codesign --force --options runtime --timestamp --sign "$CODESIGN_IDENTITY")
  "${SIGN[@]}" "$APP/Contents/Resources/bin/agentsmon-helper" \
    "$APP/Contents/Resources/codex-plugin/bin/agentsmon-helper"
  "${SIGN[@]}" --entitlements "$ROOT/bundle/entitlements.plist" "$APP"
elif LOCAL_IDENTITY="$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')" \
  && [[ -n "$LOCAL_IDENTITY" ]]; then
  # Local build: a stable identity keeps macOS permissions (Automation for
  # terminal focus) and login-item approval across rebuilds. No hardened
  # runtime here: it would need an Apple Events entitlement for osascript.
  codesign --force --deep --sign "$LOCAL_IDENTITY" "$APP"
else
  codesign --force --deep --sign - "$APP"
fi

echo "$APP"
