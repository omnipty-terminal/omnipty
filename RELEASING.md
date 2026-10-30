# Releasing OmniPTY

The loop: **note changes in `CHANGELOG.md` → `bump.sh` → commit → `release.sh`**.

## Steps

### 1. Land the feature work

Commit and push your changes as usual, with whatever descriptive messages you
like. Add a line for anything user-visible under **Unreleased** in
`CHANGELOG.md` as you go. The version bump is deliberately *not* part of these
commits.

### 2. Make the release commit

```sh
./scripts/bump.sh 0.9.3
git commit -am "release v0.9.3" && git push
```

`bump.sh` does four things, and refuses to run if **Unreleased** in
`CHANGELOG.md` is empty:

- sets `version` in `Cargo.toml`
- runs `cargo check`, which rewrites `Cargo.lock`'s own version line (skip
  this and the release build rewrites the lock afterwards, leaving a stray
  second commit; pinned dependency versions are untouched)
- renames the **Unreleased** heading in `CHANGELOG.md` to the version and
  today's date, and starts a fresh empty **Unreleased** above it
- updates the example version in this file

Commit everything it touched as one commit.

Two reasons this is one commit of its own:

- The binary must correspond to exactly one commit, so the tag you create in
  step 3 points at precisely what you shipped. `dmg.sh` refuses to build from a
  dirty tree for this reason (`ALLOW_DIRTY=1` overrides for local experiments).
- Installed copies decide whether to offer an update by comparing the release
  tag against the version compiled into the binary. A release that reuses the
  old version number is invisible to the updater.

### 3. Build and publish

```sh
./scripts/release.sh
```

This runs `dmg.sh` (sign, notarize, staple; it compiles the crash-report DSN
from `~/.config/omnipty-release/sentry.env` into the binary and refuses to
build without one, `NO_SENTRY=1` to override), then:

1. signs the update DMG with the minisign key (`~/.config/omnipty-release/update.key`,
   or `$UPDATE_KEY`), once per architecture in the binary;
2. uploads the DMGs and signatures to the `omnipty-releases` R2 bucket under
   `omnipty/<version>/`, and checks they serve from
   `https://downloads.omnipty.com/omnipty/<version>/`;
3. creates the GitHub release with both DMGs, with the `## [<version>]`
   section of `CHANGELOG.md` as the notes;
4. writes the update manifest — `releases/stable.json` in the bucket, plus a
   `releases/<version>.json` copy for rollbacks;
5. refreshes `omnipty/latest/OmniPTY.dmg`, the one object that changes in place:
   it's what the website's download links point at, so they never go stale;
6. bumps the Homebrew cask and regenerates the site changelog;
7. waits for the new changelog page to go live, then pings IndexNow
   (`site/scripts/indexnow.py`) so Bing recrawls it. A slow deploy or a
   rejected ping only prints a warning with the command to rerun.

It refuses to run if the changelog section is missing or empty, so a
forgotten changelog rename fails before the slow build starts, and likewise
if `update.pub` is still the placeholder, minisign or the key is missing,
wrangler isn't logged in, or the bucket already has this version.

If it fails partway (an upload, GitHub, the cask), fix the cause and rerun
with `SKIP_BUILD=1 ./scripts/release.sh`: it reuses the DMG in `target/`,
skips objects that are already in the bucket with the same size, and leaves
an existing GitHub release alone, so it picks up where it stopped. The DMG
goes up under two names:

- `OmniPTY-<version>.dmg` — what the website's download button and the cask serve
- `OmniPTY-<version>-update.dmg` — the same bytes, what the in-app updater fetches

Both R2 and GitHub count downloads per object, so the fresh-install count stays
apart from updates.

The order matters: the manifest is written last, after the DMG it points at
is confirmed reachable, so no installed copy ever sees an update it can't
fetch. GitHub stays a mirror: copies older than the manifest switch still
look there, and the release page is what the Linux pill opens.

The tag lands on current HEAD — the commit you just built from, because of
step 2. The script refuses to run if the tag already exists.

Notes on the DMG build:

- Notary credentials come from the `omnipty-notary` keychain profile
  (falls back to `APPLE_ID` / `APPLE_TEAM_ID` / `APPLE_PASSWORD` env vars).
  To (re)create the profile:

  ```sh
  xcrun notarytool store-credentials omnipty-notary \
    --apple-id "$APPLE_ID" --team-id "$APPLE_TEAM_ID" --password "$APPLE_PASSWORD"
  ```

- Notarization normally clears in ~1–3 minutes. (An account's first-ever
  submission gets extended review and can take an hour.)

#### Doing it by hand

If the build succeeded but publishing failed, or you need `--target <sha>` to
tag a different commit (`gh` rejects abbreviated SHAs; pass the full hash):

```sh
cp target/OmniPTY-0.9.3.dmg target/OmniPTY-0.9.3-update.dmg
minisign -S -s ~/.config/omnipty-release/update.key \
  -m target/OmniPTY-0.9.3-update.dmg -x target/OmniPTY-0.9.3-update.dmg.macos-aarch64.minisig \
  -t "omnipty 0.9.3 macos-aarch64"
for f in OmniPTY-0.9.3.dmg OmniPTY-0.9.3-update.dmg OmniPTY-0.9.3-update.dmg.macos-aarch64.minisig; do
  npx wrangler r2 object put "omnipty-releases/omnipty/0.9.3/$f" --remote --file "target/$f" \
    --cache-control "public, max-age=31536000, immutable"
done
gh release create v0.9.3 target/OmniPTY-0.9.3-update.dmg target/OmniPTY-0.9.3.dmg \
  --title "OmniPTY v0.9.3" \
  --notes "$(sed -n '/^## \[0.9.3\]/,/^## \[/p' CHANGELOG.md | sed '1d;$d')"
```

Then the manifest. Its shape (the `signature` is the `.minisig` file's contents
as one JSON string; `jq -Rs . < file.minisig` produces it):

```json
{
  "version": "0.9.3",
  "pub_date": "2026-10-30T18:00:00Z",
  "notes_url": "https://omnipty.com/changelog/#v0.9.3",
  "release_url": "https://github.com/omnipty-terminal/omnipty/releases/tag/v0.9.3",
  "download_url": "https://downloads.omnipty.com/omnipty/0.9.3/OmniPTY-0.9.3.dmg",
  "sha256": "<sha256 of the DMG>",
  "assets": {
    "macos-aarch64": {
      "url": "https://downloads.omnipty.com/omnipty/0.9.3/OmniPTY-0.9.3-update.dmg",
      "size": 11705200,
      "sha256": "<sha256 of the DMG>",
      "signature": "untrusted comment: …\n…\ntrusted comment: omnipty 0.9.3 macos-aarch64\n…\n"
    },
    "linux-x86_64": {
      "url": "https://downloads.omnipty.com/omnipty/0.9.3/omnipty-0.9.3-linux-x86_64.tar.gz",
      "size": 8000000,
      "sha256": "<sha256 of the tarball>",
      "signature": "…"
    }
  }
}
```

(`linux-x86_64` is added by `release-linux.sh`; `release_url` is what the
Linux pill opens; `size` is for the website's download button. Finally
`omnipty/latest/OmniPTY.dmg` gets a copy of the DMG with
`--cache-control "public, max-age=300"`.)

The trusted comment must be exactly `omnipty <version> macos-<arch>` — the
updater checks it after the signature, so a real signature can't be reused for
another version. Upload it as `releases/0.9.3.json` (immutable) and
`releases/stable.json` (`--cache-control "public, max-age=60"`).

### 4. The Linux build (on the Linux box)

GPUI can't be cross-compiled, so the Linux tarball is built on the Linux
machine and attached to the release `release.sh` just created. The tag
points at the release commit, which is main's HEAD, so a plain pull lands
on it:

```sh
ssh linux-box
cd ~/Code/omnipty/omnipty
git pull --ff-only && git fetch --tags    # HEAD is at (or just past) v0.9.3
./scripts/release-linux.sh
```

This runs `linux-package.sh`, then signs `omnipty-<version>-linux-x86_64.tar.gz`
with the same minisign key, uploads it and its `.minisig` to the bucket
under `omnipty/<version>/`, mirrors it onto the GitHub release, adds a
`linux-x86_64` entry to `releases/<version>.json` and `releases/stable.json`,
refreshes `omnipty/latest/omnipty-linux-x86_64.tar.gz` for the website's Linux
download link, and bumps `packaging/aur/omnipty-bin/PKGBUILD` to the
new version, URL and checksum. It refuses to run if the release or the manifest doesn't
exist yet, if the bucket already has this tarball, or if anything that goes
into the build (`Cargo.*`, `src/`, `assets/`, `packaging/docker/`) changed
since the tag — commits after the release that only touch docs are fine.

The manifest step is last, after the tarball is confirmed reachable, so
installed Linux copies only start announcing a release they can get. If
`stable.json` has moved on to a newer version in the meantime (a hotfix
released from the Mac before this ran), only the per-version file is
updated and the script says so.

`linux-package.sh` does the release build inside the `packaging/docker`
container (Ubuntu 22.04), not on the host. A binary links against the glibc
of whatever built it and won't start anywhere older, and the Linux box runs
Arch, whose glibc is newer than everyone's — that was issue #2. The
container's glibc 2.35 covers Ubuntu 22.04, Debian 12, Fedora 36 and later,
and the script checks the built binary against that floor and refuses to
package one that needs more. Then strip, and the tarball with the `.desktop`
entry, icons and `install.sh`. The first build compiles every dependency
from scratch and takes a while; later ones reuse `target/docker/`. Commit
that bump:

```sh
git commit -am "aur: omnipty-bin 0.9.3" && git push
```

The package isn't on the AUR yet, so that's the end of the Linux release.
Once there's an AUR account (see the setup list below), each release also
publishes it:

```sh
cd packaging/aur/omnipty-bin && makepkg --printsrcinfo > .SRCINFO
cp PKGBUILD .SRCINFO ~/aur/omnipty-bin/
cd ~/aur/omnipty-bin && git add -A && git commit -m "v0.9.3" && git push
```

Before the first push, `makepkg -si` in `packaging/aur/omnipty-bin`
is the local check that the PKGBUILD fetches and installs from
downloads.omnipty.com.

Installed Linux copies look for the manifest's `linux-x86_64` entry, so the
update pill only appears once this step is done. The tarball is signed for
the record (the AUR package pins the sha256 instead); nothing on Linux
installs in place, so nothing verifies it yet.

One-time setup on the Linux box:

- `gh auth login` (the GitHub mirror goes through `gh`).
- `pacman -S minisign jq`, and a copy of the signing key at
  `~/.config/omnipty-release/update.key` (or `UPDATE_KEY=…`). It's the same key
  as on the Mac — copy it over `scp`, never through a repo.
- A copy of `~/.config/omnipty-release/sentry.env` (see "Crash reports"
  below): `release-linux.sh` won't build without it either.
- Node, then `npx wrangler login` (the browser step works over SSH by pasting
  the URL it prints). Set `CLOUDFLARE_ACCOUNT_ID` if `npx wrangler whoami`
  lists more than one account.
- Docker, for the build container. The daemon isn't enabled at boot on the
  Arch box, so `sudo systemctl start docker` before a release. Run the
  script as yourself, never under `sudo` (the build runs as your uid so the
  output stays yours); if your user isn't in the `docker` group,
  `DOCKER="sudo docker" ./scripts/release-linux.sh`. The group is
  root-equivalent, so `sudo` for an occasional release is the safer habit.
- Not yet done — an [AUR account](https://aur.archlinux.org/register) with
  your SSH public key added, then the package clone; the first push creates
  the package:
  `git clone ssh://aur@aur.archlinux.org/omnipty-bin.git ~/aur/omnipty-bin`.
  Add `aur.archlinux.org` to `~/.ssh/known_hosts` on first contact.

## The legacy Oxide channel

Copies installed as Oxide (0.8.x and earlier) read
`https://downloads.oxideterminal.com/releases/stable.json` from the old
`oxide-releases` bucket, and only accept a signature whose trusted comment
is `oxide <version> macos-<arch>`. That bucket is frozen at the 0.9.3 bridge
release and is never written by `release.sh`; it carries those copies to
0.9.3, after which they update from here like everyone else. How it was set
up is in `support/RENAME_RUNBOOK.md`. Leave it serving.

## What happens after publishing

Installed macOS copies read
`https://downloads.omnipty.com/releases/stable.json` on launch and every
6 hours (or immediately via **OmniPTY → Check for Updates…**). They download the
DMG in the background, verify its minisign signature against the key compiled
into the app (`update.pub`), and only then show the top-right "click to
install" pill; on click they swap the bundle and relaunch. A download whose
signature doesn't check out is deleted and reported, never installed.

Installed Linux copies read the same manifest and, once it has a
`linux-x86_64` entry, show the pill; clicking it opens the release page
(`release_url` in the manifest) since nothing installs in place there.
Copies older than the manifest switch still read GitHub, which is one reason
the release is mirrored there.

Nothing else to do on the publishing side.

## Crash reports

Release builds carry the Sentry DSN (`src/telemetry.rs` reads it from
`OMNIPTY_SENTRY_DSN` at compile time; `cargo run`, CI and forks build without
one and send nothing). `dmg.sh` and `release-linux.sh` load it from
`~/.config/omnipty-release/sentry.env` on each machine, `chmod 600`:

```sh
# Compiled into release builds by dmg.sh and release-linux.sh.
export OMNIPTY_SENTRY_DSN="https://<key>@o<org>.ingest.us.sentry.io/<project>"
```

The DSN is under Project Settings → Client Keys (DSN) in Sentry. It isn't a
secret (it can only submit events, and `strings` finds it in any shipped
binary), but it stays out of the repo so forks and dev builds don't report
into the project. Don't export it in your shell profile: every local
`cargo build --release` would then report as `production`. If it's ever
abused, rotate it in Sentry and ship a release.

Both scripts check the DSN made it into the binary. `NO_SENTRY=1` builds a
release without crash reports. The full design, including the readable
stack traces still to do, is in `support/SENTRY_PLAN.md`.

## Publishing to downloads.omnipty.com

One-time setup on the release machine. The bucket (`omnipty-releases`, custom
domain `downloads.omnipty.com`) already exists in the Cloudflare account.

1. **Signing key.** Generate a minisign keypair and keep the secret key out of
   every repo:

   ```sh
   brew install minisign
   mkdir -p ~/.config/omnipty-release
   minisign -G -p ~/.config/omnipty-release/update.pub -s ~/.config/omnipty-release/update.key
   cp ~/.config/omnipty-release/update.pub update.pub    # in the omnipty repo; commit it
   ```

   `minisign -G` asks for a password; `-S` asks for it again at each release.
   `-W` skips the password if you'd rather rely on the disk being encrypted.
   Back the `.key` file up somewhere that isn't this machine (a password
   manager works): if it's lost, every installed copy is stuck until it's
   updated by hand to a build with a new `update.pub`. Never commit it.

   `update.pub` in the repo is the placeholder until this is done. The app
   builds either way, but the updater refuses everything until the real key
   is in place.

2. **Wrangler.** The upload goes through Cloudflare's CLI:

   ```sh
   npx wrangler login
   npx wrangler whoami       # should list the account with the bucket
   ```

   With more than one Cloudflare account, set `CLOUDFLARE_ACCOUNT_ID` in the
   environment. `release.sh` invokes it as `npx --yes wrangler`, so there's
   nothing to install globally. `wrangler r2 object put` uploads in one
   request, which caps objects at a few hundred MB — far above the DMG.

3. **Caching.** Objects are uploaded with their `Cache-Control` set:
   versioned files a year and immutable, `stable.json` one minute, the
   `omnipty/latest/` copies five minutes. Cloudflare's
   edge honours these on the custom domain, so nothing further is needed. If
   an update ever seems slow to appear, purge `releases/stable.json` from the
   dashboard (Caching → Configuration → Custom Purge).

4. **Cross-origin reads.** The homepage's download button reads
   `stable.json` from the browser to show the version, date and size, which
   needs a CORS policy on the bucket (R2 → `omnipty-releases` → Settings → CORS
   policy). Without it the button still works; the version line just stays
   blank.

   ```json
   [
     {
       "AllowedOrigins": ["https://omnipty.com"],
       "AllowedMethods": ["GET", "HEAD"],
       "AllowedHeaders": ["*"],
       "MaxAgeSeconds": 3600
     }
   ]
   ```

5. **Try it** before a real release with `R2_BUCKET`/`DOWNLOADS_URL` pointed
   at a scratch bucket, or just check the pieces:

   ```sh
   npx wrangler r2 object put omnipty-releases/test.txt --remote --pipe <<< hi
   curl -fsS https://downloads.omnipty.com/test.txt
   npx wrangler r2 object delete omnipty-releases/test.txt --remote
   ```

**Rolling back** a bad release: put the previous version's manifest back as
`stable.json` —

```sh
npx wrangler r2 object get omnipty-releases/releases/0.6.2.json --remote --file /tmp/m.json
npx wrangler r2 object put omnipty-releases/releases/stable.json --remote --file /tmp/m.json \
  --content-type application/json --cache-control "public, max-age=60"
```

— and delete the bad GitHub release (`gh release delete v0.9.3 --yes`).
Installed copies never downgrade, so anyone who already got the bad build
needs the next release; ship it as a new version number.

## Gotchas

- **Never overwrite a published object.** `release.sh` refuses to run if the
  bucket already has this version. A fix goes out as a new version number;
  old versions stay so the cask's pinned URL and rollbacks keep working.
- **The tag must be `v<Cargo.toml version>`** (e.g. `v0.9.3` for `0.9.3`) and
  the GitHub release must have a `.dmg` asset, or updaters older than the
  manifest switch ignore it. They prefer `-update.dmg` and fall back to the
  plain one.
- **`update.pub` is part of the build.** Changing the signing key means a
  release signed with the *old* key that carries the new `update.pub`, then
  releases signed with the new one from there on. Installed copies only ever
  trust the key they were built with.
- Version numbering: bug fixes and polish get a patch bump (`0.1.1`), a new
  user-facing capability gets a minor bump (`0.2.0`).
- **Don't rebuild while a notarization is in flight** — `bundle.sh`/`dmg.sh`
  overwrite `target/OmniPTY.app`, and the submitted ticket only staples to the
  exact bytes that were uploaded.
- Development builds (`cargo run`) never auto-check for updates; only
  installed `.app` bundles do — and on Linux, release builds outside a
  `target/` directory.
