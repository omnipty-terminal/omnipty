# Linux Port

**Status (2026-10-13): ported.** OmniPTY builds, links, passes its test suite
and runs on Linux (Wayland, Vulkan). The `linux-port` branch carries the
work; this file is now the record of what was done, what was verified on
real hardware, and what is still open. Native Xorg support has also been
smoke-tested successfully under LXQt. The original plan's phases are kept
as headings so the decisions stay traceable.

Target machine: an Arch-based **Omarchy 4** desktop — Hyprland (Wayland),
tiling-first, Super-key-driven. Verified there on 2026-09-22 with the
graphical session (`WAYLAND_DISPLAY=wayland-1`, AMD Radeon 780M / RADV).
Nothing in the port is Arch- or Hyprland-specific; the packaging covers any
distro through the tarball and Arch through the AUR.

## What changed

Platform differences are confined to `cfg` sites in a handful of files.
Nothing else in the crate knows which OS it's on.

| area | macOS | Linux | where |
|---|---|---|---|
| bell | `NSBeep` | `canberra-gtk-play` (XDG theme), falling back to `paplay`,then the visual flash | `terminal/mod.rs` `system_beep` |
| foreground cwd | `proc_pidinfo(PROC_PIDVNODEPATHINFO)` | `/proc/<pgrp>/cwd` | `terminal/session.rs` |
| foreground name / argv | `proc_pidinfo` + `KERN_PROCARGS2` | `/proc/<pid>/comm` (+ `exe` for the untruncated name), `/proc/<pid>/cmdline` | `terminal/process.rs` |
| notifications | `UNUserNotificationCenter` (bundle) / `osascript` | `notify-send -A default=Open -w`; a click prints the action and routes back to the pane | `notifications.rs`, `Cargo.toml` (`objc`/`block` are macOS-only deps) |
| updater | download DMG, swap bundle, relaunch | announce only: pill opens the release page once the update manifest has a `linux-<arch>` entry | `update.rs`, `app.rs` `UpdateState::Available` |
| "installed?" gate | running from a `.app` | release build outside a `target/` dir | `update::is_installed` |
| trash | `~/.Trash` | XDG trash, via the `trash` crate on both | `tree/mod.rs` `delete_entry` |
| reveal | `open -R` | `App::reveal_path` (Finder / `org.freedesktop.FileManager1`) on both | `tree/mod.rs`, `keymap/actions.rs` title |
| no-`$EDITOR` fallback | `open -t` | `xdg-open` | `app.rs` `editor_snippet` |
| window | hidden titlebar + 30px inset, traffic lights | `app_id = "omnipty"`, no inset, compositor decorations | `app.rs` `open_omnipty_window`, `OmniPTY::render` |
| lifecycle | stays running with no windows, Dock reopens | last window closed → quit; no menu bar | `main.rs` |
| open-modifier | cmd-click / cmd-hover | ctrl-click / ctrl-hover | `terminal::open_modifier`, five call sites |
| keymap | `MACOS` table (`cmd-*`) | `LINUX` table (`ctrl-shift-*`, `alt-<n>`); `SHARED` for the rest | `keymap/default.rs` |
| key labels | `⇧⌘P` | `Ctrl+Shift+P` | `keymap/resolve.rs` `pretty_keys` |
| primary selection | — | selection → primary, middle-click pastes it | `terminal/mod.rs` |
| shift-at-launch | `window.modifiers()` at `new` | first `ModifiersChanged` within 1.5s of the window opening | `app.rs` `on_launch_modifiers`, `TerminalPane::cancel_startup` |

Packaging and CI:

- `scripts/linux-package.sh` → `target/omnipty-<version>-linux-<arch>.tar.gz`
  (binary, `.desktop`, hicolor icons, licence, `install.sh`).
- `scripts/linux-install.sh` (shipped as `install.sh`): `~/.local` by
  default, `--prefix`, `--uninstall`.
- `scripts/release-linux.sh`: build, `gh release upload`, bump the AUR
  PKGBUILD. The release is two-machine — see `RELEASING.md` §4.
- `packaging/aur/omnipty-bin/PKGBUILD`: from the release tarball.
  `depends` reflects `ldd` on the built binary (xcb/xkbcommon), plus the
  runtime-loaded `wayland`, `vulkan-icd-loader`, and `libnotify`.
- `assets/linux/`: `omnipty.desktop` (`StartupWMClass=omnipty` matches the
  `app_id`) and pre-rendered hicolor icons, so packaging needs no image tool.
- `scripts/omnipty-cli` branches on `uname`; on Linux it finds the real
  binary on PATH and detaches. Not needed for normal installs — the binary
  itself is `omnipty` and takes the same arguments.
- `.github/workflows/ci.yml`: the Linux job is now `cargo build
  --all-targets` + `cargo test`, required (no `continue-on-error`), with
  `fish zsh dash libnotify-bin` installed for the cross-shell tests.

## Verified on the Omarchy box (2026-09-22)

Done with the debug build, a scratch directory, `grim` screenshots and
`wtype` for keys (Omarchy's `hyprctl dispatch` is a Lua wrapper —
`sendshortcut`/`focuswindow` in the classic syntax silently error, and
focus follows the mouse, so keystroke injection is only reliable while the
pointer is over the OmniPTY window).

- [x] `cargo build --all-targets` links; `cargo test`: 173 passed
- [x] `cargo run` opens a window under Hyprland; `hyprctl clients` shows
      `class: omnipty`, `xwayland: false`, tiled with no top inset
- [x] shell spawns, prompt renders with the bundled Nerd Font, powerline
      glyphs correct
- [x] tree follows `cd` (procfs `cwd`); status bar and title update
- [x] tab title shows the foreground command (`sleep` via procfs `comm`)
- [x] `ctrl-shift-t` new tab, `alt-1` tab select
- [x] `ctrl-w v` split, `ctrl-w <` narrows — shifted punctuation arrives as
      the composed character, as predicted
- [x] Linux keymap tables pass the parse/registry/coverage tests; palette
      labels are `Ctrl+Shift+…` (unit-tested)
- [x] `notify-send -A default=Open -w` is accepted by the running daemon
      (Omarchy's quickshell) and waits on the notification
- [x] `window.opacity = 0.85` + `blur = true`: the wallpaper shows through
      and looks blurred (KDE blur protocol, Hyprland)
- [x] SIGTERM/quit leaves no orphaned shells
- [x] trash: `trash::delete` unit path; `~/.local/share/Trash` is what the
      crate targets

Still to eyeball by hand (input injection was stopped once the pointer moved
to another window):

- [ ] tree `d` → `y` lands the file in `~/.local/share/Trash/files/`
- [ ] "Reveal in File Manager" opens the file manager on the containing
      folder (GPUI's `org.freedesktop.FileManager1` call)
- [ ] a long command finishing unfocused → notification; clicking it
      focuses the pane
- [ ] ctrl-click opens a URL / path, ctrl-hover underlines; Super+click
      still moves the window
- [ ] middle-click pastes the primary selection
- [ ] shift held while launching skips startup commands (needs a pinned
      workspace with a startup command)
- [ ] closing the last window (`ctrl-shift-w` on the last pane) exits the
      process
- [x] **X11**: Tested on a native Xorg session under LXQT. WAYLAND_DISPLAY= omnipty
      successfully creates and renders a window, spawns the shell, and runs
      normally. No X11-specific issues observed during testing.
- [ ] resize storms with `htop` running (SIGWINCH debounce)
- [ ] config live-reload (inotify)

## Decisions (as built)

1. **Linux keymap**: `ctrl-shift-*` for the `cmd-*` set, `alt-1..9` tabs,
   `ctrl-alt-1..9` workspaces, `ctrl-pageup/pagedown` tab cycling,
   `ctrl-shift-o` file finder, `ctrl-w shift-t` theme picker. Font size is
   `ctrl-+`/`ctrl-=`/`ctrl--`/`ctrl-0` (GPUI's xkb path reports a shifted
   symbol as the symbol with shift dropped, so `ctrl-shift-=` *is*
   `ctrl-+`). No `ctrl-_` — readline's undo. Super is never bound. hide /
   hide-others / minimise / fullscreen are the compositor's.
2. **Last-window-close on Linux**: quit.
3. **Trash**: the `trash` crate, both platforms.
4. **Updater on Linux**: announce-only pill → release page; requires the
   manifest's Linux entry so the pill never points at a mac-only release.
5. **`window.titlebar`**: ignored on Linux; GPUI requests server-side
   decorations (Hyprland: none; KDE/sway: a title bar; GNOME: none, no SSD).
6. **Shift-at-launch**: key-event fallback, 1.5s window. `--no-startup-
   commands` stays the universal out.
7. **Notifications**: `notify-send` with `-A`/`-w` for click routing, plain
   retry if the flags are rejected (libnotify < 0.7.9).
8. **Blur**: left on; works under Hyprland.
9. **cmd-click**: ctrl-click.
10. **Release artifact**: built on the Linux box, uploaded after the mac
    release; PKGBUILD bumped by `release-linux.sh`.
11. **Window bounds** (deviation from the plan): still saved and restored on
    Linux. Harmless under a tiling WM (the compositor overrides) and useful
    under a floating one.
12. **Binary name**: the Linux binary is `omnipty`; there is no wrapper in
    the tarball or the package. `scripts/omnipty-cli` is optional, for the
    macOS-style detached launch.

## Open items / follow-ups

- ~~**`-e <command>` flag** so OmniPTY can be an `xdg-terminal-exec` target
  (Omarchy's Super+Return).~~ Done: `-e` and `--app-id` in `cli.rs`, the
  first pane runs the command with no shell integration, and
  `assets/linux/omnipty.desktop` declares `X-TerminalArgExec` /
  `X-TerminalArgAppId`. Pinned workspaces are not restored for a `-e` launch.
- ~~**X11** (above).~~
- ~~**`bell = "sound"`** on Linux: XDG sound theme / PipeWire, if anyone
  asks.~~ Done: `canberra-gtk-play` plays the XDG theme's bell sound,
  falling back to `paplay` on the stock freedesktop sound if libcanberra's
  CLI isn't installed. Runs on a background thread.
- **Self-update on Linux**: AppImage would allow it; not planned.
- **Flatpak**: still not worth it (sandboxed terminals fight the host shell).
- **Generated config template** (`config/mod.rs`): key names in comments are
  localised at write time; keep the token table there in step with the
  Linux keymap when bindings change.
- `Cargo.lock` grew Windows-only entries from the `trash` crate; harmless.
