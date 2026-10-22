#!/bin/bash
# Build the Linux release tarball: the analogue of bundle.sh + dmg.sh.
#
#   scripts/linux-package.sh                        build + package
#   ALLOW_DIRTY=1 scripts/linux-package.sh          package an uncommitted tree
#   DOCKER="sudo docker" scripts/linux-package.sh   if your user can't reach the daemon
#   NO_DOCKER=1 scripts/linux-package.sh            build on the host (see below)
#
# Produces target/omnipty-<version>-linux-<arch>.tar.gz containing the binary,
# a .desktop entry, hicolor icons, the licence and an install script. That
# name is what the in-app update check looks for on Linux, so keep it.
#
# The binary is built inside the packaging/docker container, not on the host.
# A binary links against the glibc symbol versions of whatever built it and
# refuses to start anywhere older; issue #2 was an Arch-built (glibc 2.44)
# tarball that Ubuntu 24.04 (2.39) couldn't load. The container is Ubuntu
# 22.04, glibc 2.35, and the check after the build refuses to package a
# binary needing more — so NO_DOCKER=1 only yields a shippable tarball on a
# host at least that old. Run the script as yourself, not under sudo: the
# build runs as your uid so the output stays yours.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(uname -m)
NAME="omnipty-${VERSION}-linux-${ARCH}"
STAGE="$ROOT/target/$NAME"
TARBALL="$ROOT/target/$NAME.tar.gz"

# Oldest glibc the tarball may require. Matches the base image in
# packaging/docker/Dockerfile; change them together.
GLIBC_FLOOR="2.35"
DOCKER="${DOCKER:-docker}"
IMAGE="omnipty-linux-build"
# Everything the container writes lands here: cargo's target dir, plus its
# registry and git caches under cargo-home/ so a rebuild doesn't refetch every
# crate. Kept apart from target/release so host and container builds never
# invalidate each other's artefacts.
OUT="$ROOT/target/docker"

# A release binary should correspond to exactly one commit, so refuse to build
# from a dirty tree. ALLOW_DIRTY=1 overrides for local experiments.
if [[ -z "${ALLOW_DIRTY:-}" && -n "$(git status --porcelain 2>/dev/null)" ]]; then
  echo "error: working tree has uncommitted changes." >&2
  echo "       The built binary would not match any commit or tag." >&2
  echo "       Commit first, or re-run with ALLOW_DIRTY=1." >&2
  git status --short >&2
  exit 1
fi

LOCK_VERSION=$(awk '/^name = "omnipty"$/{getline; gsub(/[",]/, "", $3); print $3; exit}' Cargo.lock)
if [[ "$LOCK_VERSION" != "$VERSION" ]]; then
  echo "error: Cargo.lock says $LOCK_VERSION but Cargo.toml says $VERSION." >&2
  echo "       Run 'cargo check' to refresh the lock, then amend your release commit." >&2
  exit 1
fi

if [[ -n "${NO_DOCKER:-}" ]]; then
  cargo build --release --locked
  BIN="$ROOT/target/release/omnipty"
else
  # $DOCKER is unquoted on purpose: "sudo docker" is two words.
  if ! $DOCKER info >/dev/null 2>&1; then
    echo "error: can't reach the Docker daemon." >&2
    echo "       Start it (sudo systemctl start docker); if your user isn't in the" >&2
    echo "       docker group, re-run with DOCKER=\"sudo docker\"." >&2
    exit 1
  fi
  # Create the mount point first. Docker creates a missing bind-mount source
  # itself, as root, and the build (which runs as you) then can't write to it.
  mkdir -p "$OUT"
  if [[ ! -O "$OUT" ]]; then
    echo "error: $OUT isn't owned by you (a docker run probably created it)." >&2
    echo "       Remove it and re-run: sudo rm -rf $OUT" >&2
    exit 1
  fi
  $DOCKER build -t "$IMAGE" packaging/docker
  # The source is mounted read-only: with a separate target dir and --locked,
  # nothing in the build has any business writing to the tree.
  $DOCKER run --rm \
    --user "$(id -u):$(id -g)" \
    --volume "$ROOT:/src:ro" \
    --volume "$OUT:/out" \
    --env HOME=/tmp \
    --env CARGO_HOME=/out/cargo-home \
    --env CARGO_TARGET_DIR=/out \
    --env OMNIPTY_SENTRY_DSN \
    "$IMAGE" cargo build --release --locked
  BIN="$OUT/release/omnipty"
fi
# `--env NAME` above copies the host's value and passes nothing when unset,
# so a build from release-linux.sh carries the crash-report DSN and a plain
# local one doesn't. Make sure the build saw it when it was meant to.
if [[ -n "${OMNIPTY_SENTRY_DSN:-}" ]] && ! grep -qaF "$OMNIPTY_SENTRY_DSN" "$BIN"; then
  echo "error: the DSN didn't make it into $BIN." >&2
  exit 1
fi

# Check what the binary actually asks of glibc before it goes anywhere. This
# is the guard that turns the next toolchain or dependency bump from a bug
# report into a failed packaging step.
if ! command -v objdump >/dev/null; then
  echo "error: objdump (binutils) is needed to check the binary's glibc requirement." >&2
  exit 1
fi
TOO_NEW=$(objdump -T "$BIN" | awk -v floor="$GLIBC_FLOOR" '
  function newer(a, b,  x, y) {
    split(a, x, "."); split(b, y, ".")
    return x[1] + 0 > y[1] + 0 || (x[1] + 0 == y[1] + 0 && x[2] + 0 > y[2] + 0)
  }
  match($0, /GLIBC_[0-9]+\.[0-9]+/) {
    v = substr($0, RSTART + 6, RLENGTH - 6)
    if (newer(v, floor)) print "         " $NF " needs GLIBC_" v
  }' | sort -u)
if [[ -n "$TOO_NEW" ]]; then
  echo "error: this binary won't start on glibc $GLIBC_FLOOR:" >&2
  echo "$TOO_NEW" >&2
  if [[ -n "${NO_DOCKER:-}" ]]; then
    echo "       NO_DOCKER=1 built it against this host's glibc $(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')." >&2
    echo "       Drop it to build in the container." >&2
  else
    echo "       The container's glibc is $GLIBC_FLOOR, so something links to a newer one" >&2
    echo "       by hand — check packaging/docker/Dockerfile and the build output above." >&2
  fi
  exit 1
fi
GLIBC_NEEDED=$(objdump -T "$BIN" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | sed 's/GLIBC_//' | sort -uV | tail -1)

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
cp "$BIN" "$STAGE/omnipty"
# Only the debug info: a full strip would also drop the symbol table, and
# crash reports would then arrive as bare addresses (Cargo already leaves
# the debug info out of release builds; this is belt and braces).
strip --strip-debug "$STAGE/omnipty" 2>/dev/null || true
cp "$ROOT/assets/linux/omnipty.desktop" "$STAGE/omnipty.desktop"
cp -R "$ROOT/assets/linux/icons" "$STAGE/icons"
cp "$ROOT/LICENSE" "$STAGE/LICENSE"
cp "$ROOT/scripts/linux-install.sh" "$STAGE/install.sh"
chmod +x "$STAGE/omnipty" "$STAGE/install.sh"

tar -C "$ROOT/target" -czf "$TARBALL" "$NAME"
rm -rf "$STAGE"

echo "built $TARBALL (needs glibc $GLIBC_NEEDED; floor $GLIBC_FLOOR)"
sha256sum "$TARBALL"
