#!/bin/bash
# Build a signed, notarized, stapled OmniPTY-<version>.dmg ready for distribution.
#
# Requires:
#   - a "Developer ID Application" identity in the keychain (auto-detected,
#     or pass SIGN_ID explicitly)
#   - notary credentials, either (preferred, keeps the secret out of argv):
#       xcrun notarytool store-credentials omnipty-notary \
#         --apple-id ... --team-id ... --password <app-specific>
#     or APPLE_ID / APPLE_TEAM_ID / APPLE_PASSWORD in the environment.
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

# A release binary should correspond to exactly one commit, so refuse to build
# from a dirty tree. ALLOW_DIRTY=1 overrides for local experiments.
if [[ -z "${ALLOW_DIRTY:-}" && -n "$(git status --porcelain 2>/dev/null)" ]]; then
  echo "error: working tree has uncommitted changes." >&2
  echo "       The built binary would not match any commit or tag." >&2
  echo "       Commit first, or re-run with ALLOW_DIRTY=1." >&2
  git status --short >&2
  exit 1
fi

# Cargo.lock carries this package's own version. If it lags Cargo.toml the
# build silently rewrites it, so the released commit would miss the change.
LOCK_VERSION=$(awk '/^name = "omnipty"$/{getline; gsub(/[",]/, "", $3); print $3; exit}' Cargo.lock)
if [[ "$LOCK_VERSION" != "$VERSION" ]]; then
  echo "error: Cargo.lock says $LOCK_VERSION but Cargo.toml says $VERSION." >&2
  echo "       Run 'cargo check' to refresh the lock, then amend your release commit." >&2
  exit 1
fi
# Crash reports: the DSN is compiled in from the environment
# (support/SENTRY_PLAN.md). A release without one would ship silent.
SENTRY_ENV="${SENTRY_ENV:-$HOME/.config/omnipty-release/sentry.env}"
if [[ -f "$SENTRY_ENV" ]]; then
  # shellcheck source=/dev/null
  source "$SENTRY_ENV"
fi
if [[ -n "${NO_SENTRY:-}" ]]; then
  unset OMNIPTY_SENTRY_DSN
elif [[ -z "${OMNIPTY_SENTRY_DSN:-}" ]]; then
  echo "error: OMNIPTY_SENTRY_DSN isn't set (looked in $SENTRY_ENV)." >&2
  echo "       Re-run with NO_SENTRY=1 to build without crash reports." >&2
  exit 1
else
  export OMNIPTY_SENTRY_DSN
fi
DMG="target/OmniPTY-${VERSION}.dmg"
NOTARY_PROFILE="${NOTARY_PROFILE:-omnipty-notary}"

SIGN_ID="${SIGN_ID:-$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')}"
[[ -n "$SIGN_ID" ]] || { echo "error: no Developer ID Application identity found" >&2; exit 1; }

if xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  notarize() {
    xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait
  }
else
  : "${APPLE_ID:?set APPLE_ID}" "${APPLE_TEAM_ID:?set APPLE_TEAM_ID}" "${APPLE_PASSWORD:?set APPLE_PASSWORD}"
  notarize() {
    # Note: argv is visible in `ps` while this runs; prefer the keychain
    # profile (see header) on shared machines.
    xcrun notarytool submit "$1" \
      --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD" \
      --wait
  }
fi

echo "==> building and signing with: $SIGN_ID"
SIGN_ID="$SIGN_ID" ./scripts/bundle.sh
if [[ -n "${OMNIPTY_SENTRY_DSN:-}" ]] && ! grep -qaF "$OMNIPTY_SENTRY_DSN" target/release/omnipty; then
  echo "error: the DSN didn't make it into target/release/omnipty." >&2
  exit 1
fi

echo "==> notarizing the app"
ditto -c -k --keepParent target/OmniPTY.app target/OmniPTY-notarize.zip
notarize target/OmniPTY-notarize.zip
rm -f target/OmniPTY-notarize.zip
xcrun stapler staple target/OmniPTY.app

echo "==> building the dmg"
STAGE=$(mktemp -d)
cp -R target/OmniPTY.app "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil create -volname "OmniPTY" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"

echo "==> signing and notarizing the dmg"
codesign --force --sign "$SIGN_ID" "$DMG"
notarize "$DMG"
xcrun stapler staple "$DMG"

echo "==> done: $DMG"
spctl -a -t open --context context:primary-signature -v "$DMG" || true
