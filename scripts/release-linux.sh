#!/bin/bash
# The Linux half of a release. release.sh runs on the Mac (DMGs, manifest,
# cask, site changelog) and GPUI can't be cross-compiled, so the tarball is
# built here, published to downloads.omnipty.com (R2) beside the DMGs,
# mirrored onto the same GitHub release, and added to the update manifest:
#
#   scripts/release-linux.sh            build, sign, upload, bump the AUR PKGBUILD
#   NO_UPLOAD=1 scripts/release-linux.sh   just build and bump
#
# Run it from the release commit (the one tagged v<version>), after
# release.sh has finished. Then push the AUR package:
#
#   cd packaging/aur/omnipty-bin
#   makepkg --printsrcinfo > .SRCINFO
#   git -C <aur clone> ... (see RELEASING.md)
#
# One-time setup is in RELEASING.md ("One-time setup on the Linux box").
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
TAG="v$VERSION"
ARCH=$(uname -m)
TARBALL="target/omnipty-${VERSION}-linux-${ARCH}.tar.gz"
PKGBUILD="packaging/aur/omnipty-bin/PKGBUILD"

BUCKET="${R2_BUCKET:-omnipty-releases}"
DOWNLOADS="${DOWNLOADS_URL:-https://downloads.omnipty.com}"
UPDATE_KEY="${UPDATE_KEY:-$HOME/.config/omnipty-release/update.key}"
PREFIX="omnipty/$VERSION"
WRANGLER=(npx --yes wrangler)

# The binary must be what the tag describes. HEAD may sit past the tag as
# long as nothing that goes into the build changed since — docs commits
# after a release are fine, a source change is not.
if ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  echo "error: no tag $TAG. Run release.sh on the Mac first, then: git fetch --tags" >&2
  exit 1
fi
if [[ "$(git describe --tags --exact-match 2>/dev/null || true)" != "$TAG" ]]; then
  if ! git diff --quiet "$TAG" HEAD -- Cargo.toml Cargo.lock src assets scripts/linux-package.sh packaging/docker; then
    echo "error: HEAD differs from $TAG in the sources. Build from the tag:" >&2
    echo "       git checkout $TAG" >&2
    exit 1
  fi
  echo "note: HEAD is past $TAG but the sources are identical; building anyway"
fi

if [[ -z "${NO_UPLOAD:-}" ]]; then
  # Everything the upload needs, checked before the slow build.
  command -v minisign >/dev/null || { echo "error: minisign not installed (pacman -S minisign)" >&2; exit 1; }
  command -v jq >/dev/null || { echo "error: jq not installed (pacman -S jq)" >&2; exit 1; }
  [[ -f "$UPDATE_KEY" ]] || { echo "error: update signing key $UPDATE_KEY not found (see RELEASING.md)" >&2; exit 1; }
  if ! "${WRANGLER[@]}" whoami 2>/dev/null | grep -q 'You are logged in'; then
    echo "error: wrangler isn't logged in (npx wrangler login)" >&2
    exit 1
  fi
  gh release view "$TAG" >/dev/null 2>&1 || {
    echo "error: release $TAG doesn't exist yet — run release.sh on the Mac first." >&2
    exit 1
  }
  # The Mac side writes the manifest; this script only adds to it.
  # Query string: keep a miss off Cloudflare's cache (see remote_size below).
  curl -fsS "$DOWNLOADS/releases/$VERSION.json?check=$RANDOM$RANDOM" -o "target/manifest-$VERSION.json" || {
    echo "error: $DOWNLOADS/releases/$VERSION.json isn't there — release.sh didn't finish?" >&2
    exit 1
  }
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

# Size of an object as served through the custom domain, or empty if it isn't
# there. The query string keeps this off Cloudflare's cache: a plain request
# for a missing object caches the 404 for minutes, which would then hide the
# upload from the check below.
remote_size() {
  curl -sI --max-time 20 "$DOWNLOADS/$1?check=$RANDOM$RANDOM" | tr -d '\r' | tr '[:upper:]' '[:lower:]' \
    | awk '/^http\// { ok = ($2 == "200") } /^content-length:/ { len = $2 } END { if (ok) print len }'
}

scripts/linux-package.sh
SHA=$(sha256sum "$TARBALL" | cut -d' ' -f1)

if [[ -z "${NO_UPLOAD:-}" ]]; then
  # Same shape as the DMG's signature: the trusted comment names the version
  # and platform, so a signature only ever vouches for the file it was made for.
  echo "==> signing the tarball"
  rm -f "$TARBALL.minisig"
  minisign -S -s "$UPDATE_KEY" -m "$TARBALL" -x "$TARBALL.minisig" \
    -t "omnipty $VERSION linux-$ARCH" -c "OmniPTY $VERSION linux-$ARCH"
  minisign -V -p update.pub -m "$TARBALL" -x "$TARBALL.minisig" -q

  put() {  # put <local file> <key> <content-type> <cache-control>
    "${WRANGLER[@]}" r2 object put "$BUCKET/$2" --remote --file "$1" --content-type "$3" --cache-control "$4"
  }
  IMMUTABLE="public, max-age=31536000, immutable"
  NAME=$(basename "$TARBALL")

  echo "==> uploading to $DOWNLOADS/$PREFIX/"
  # Published once: already up with the same size is a rerun, skip it; a
  # different size means a reused version number, stop.
  HAVE=$(remote_size "$PREFIX/$NAME")
  if [[ -n "$HAVE" && "$HAVE" == "$(stat -c %s "$TARBALL")" ]]; then
    echo "    $PREFIX/$NAME already published, skipping"
  elif [[ -n "$HAVE" ]]; then
    echo "error: $DOWNLOADS/$PREFIX/$NAME exists with different contents; published files are never overwritten" >&2
    exit 1
  else
    put "$TARBALL" "$PREFIX/$NAME" application/gzip "$IMMUTABLE"
  fi
  put "$TARBALL.minisig" "$PREFIX/$NAME.minisig" text/plain "$IMMUTABLE"
  for attempt in 1 2 3 4 5 6; do
    [[ "$(remote_size "$PREFIX/$NAME")" == "$(stat -c %s "$TARBALL")" ]] && break
    [[ $attempt == 6 ]] && { echo "error: $DOWNLOADS/$PREFIX/$NAME isn't serving the uploaded bytes; not touching the manifest" >&2; exit 1; }
    sleep 5
  done

  echo "==> mirroring on GitHub"
  gh release upload "$TAG" "$TARBALL" --clobber

  # Add this build to the manifest. Only now, with the tarball confirmed
  # reachable, do installed Linux copies start announcing the release.
  echo "==> adding linux-$ARCH to the update manifest"
  jq --arg k "linux-$ARCH" --arg url "$DOWNLOADS/$PREFIX/$NAME" --arg sha "$SHA" \
     --rawfile sig "$TARBALL.minisig" \
     --argjson size "$(stat -c %s "$TARBALL")" \
     '.assets[$k] = {url: $url, size: $size, sha256: $sha, signature: $sig}' \
     "target/manifest-$VERSION.json" > "target/manifest-$VERSION-linux.json"
  jq -e --arg k "linux-$ARCH" '.assets[$k].url' "target/manifest-$VERSION-linux.json" >/dev/null
  put "target/manifest-$VERSION-linux.json" "releases/$VERSION.json" application/json "$IMMUTABLE"
  # stable.json may already have moved on to a newer version (a hotfix on the
  # Mac before this ran); only replace it if it still describes this release.
  STABLE_VERSION=$(curl -fsS "$DOWNLOADS/releases/stable.json?check=$RANDOM$RANDOM" | jq -r .version)
  if [[ "$STABLE_VERSION" == "$VERSION" ]]; then
    put "target/manifest-$VERSION-linux.json" "releases/stable.json" application/json "public, max-age=60"
  else
    echo "note: stable.json is at $STABLE_VERSION, not $VERSION; left it alone"
  fi

  # What the website's Linux download link points at. Only when this is the
  # current release, for the same reason as stable.json above.
  if [[ "$STABLE_VERSION" == "$VERSION" ]]; then
    echo "==> refreshing $DOWNLOADS/omnipty/latest/omnipty-linux-$ARCH.tar.gz"
    put "$TARBALL" "omnipty/latest/omnipty-linux-$ARCH.tar.gz" application/gzip "public, max-age=300"
  fi
fi

# Bump the AUR PKGBUILD to this release. sha256 is the tarball's; pkgrel
# resets to 1 with each new version.
sed -i \
  -e "s/^pkgver=.*/pkgver=$VERSION/" \
  -e "s/^pkgrel=.*/pkgrel=1/" \
  -e "s#^source=.*#source=(\"$DOWNLOADS/omnipty/\${pkgver}/omnipty-\${pkgver}-linux-x86_64.tar.gz\")#" \
  -e "s/^sha256sums=.*/sha256sums=('$SHA')/" \
  "$PKGBUILD"
echo "bumped $PKGBUILD to $VERSION ($SHA)"
echo
echo "Next: commit the PKGBUILD, then publish it:"
echo "  cd packaging/aur/omnipty-bin && makepkg --printsrcinfo > .SRCINFO"
echo "  (copy PKGBUILD + .SRCINFO into your AUR clone, commit, push)"
