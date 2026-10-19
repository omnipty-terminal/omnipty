<p align="center">
  <img src="assets/icon_1024.png" width="176" alt="OmniPTY icon — a terminal prompt in a rusting ring" />
</p>

<h1 align="center">OmniPTY</h1>

<p align="center">
  <strong>The tools you'd normally bolt onto your terminal, built in.</strong><br/>
  A native terminal for macOS and Linux, written entirely in Rust.
</p>

<p align="center">
  <a href="#about">About</a> ·
  <a href="#download">Download</a> ·
  <a href="https://omnipty.com/docs/">Documentation</a> ·
  <a href="#contributing">Contributing</a> ·
  <a href="https://discord.gg/APV9FYGgeh">Discord</a>
</p>

<p align="center">
  <a href="https://github.com/omnipty-terminal/omnipty/releases/latest"><img src="https://img.shields.io/github/v/release/omnipty-terminal/omnipty?color=e2725b" alt="Latest release" /></a>
  <a href="https://github.com/omnipty-terminal/omnipty/actions/workflows/ci.yml"><img src="https://github.com/omnipty-terminal/omnipty/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/omnipty-terminal/omnipty" alt="MIT license" /></a>
  <a href="https://discord.gg/APV9FYGgeh"><img src="https://img.shields.io/badge/Discord-join-5865F2?logo=discord&logoColor=white" alt="Join the Discord" /></a>
</p>

<p align="center">
  <img src="assets/screenshots/main.webp" alt="OmniPTY: the file tree drawer with its workspaces panel, two tabs above three split panes running git status, cargo test and cargo run, and a git-aware status bar" />
</p>

## About

If your terminal is really a terminal plus a stack of add-ons (tmux for splits and
sessions, a file manager in another pane, a prompt framework, a copy-mode plugin),
OmniPTY builds those in, all configured from one TOML file that reloads when you save.

It's GPU-rendered on [GPUI](https://www.gpui.rs) (Zed's UI framework), with
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal) doing the emulation,
so vim, htop, and tmux itself still just work. No account, no AI, no telemetry: the only
thing OmniPTY asks the network is whether there's a newer release.

OmniPTY was called Oxide until 0.9.0. Installed copies carry themselves across on update;
see [upgrading from Oxide](https://omnipty.com/docs/install/#from-oxide) for Homebrew and dotfiles.

## What sets it apart

- **A file tree that follows you.** A vim-driven drawer beside your panes that re-roots
  when your shell `cd`s. `y` drops the selected path at the prompt, and `cmd-click` on a
  `path:line` in output opens your editor at that line.
- **Workspaces instead of tmux sessions.** Named sets of tabs and splits. Pin one and it
  reopens with the same layout, directories, and startup commands.
- **tmux reflexes, no tmux.** `ctrl-w` splits, zoom, broadcast input, and a vim copy mode
  over the scrollback.
- **It knows what your commands did.** Run time in the status bar, a red flash when a
  background pane fails, desktop notifications for long or failed commands, and history
  searchable across every pane.
- **It knows where you are.** Git branch and status in the status bar; `ssh` sessions show
  the host, and a per-host accent can put a red border on production.
- **Images inline.** kitty, iTerm2, and sixel graphics, living in the scrollback like text.
- **Make it yours.** 25 themes that can follow the system's light and dark mode, rebindable
  keys, and an optional powerline prompt that never touches your dotfiles.

## Download

**macOS** 12 or later, Apple Silicon or Intel:

```sh
brew install --cask omnipty-terminal/tap/omnipty
```

Or grab the [signed, notarized DMG](https://downloads.omnipty.com/omnipty/latest/OmniPTY.dmg).
OmniPTY keeps itself up to date from there.

**Linux** x86_64, Wayland or X11: the
[release tarball](https://downloads.omnipty.com/omnipty/latest/omnipty-linux-x86_64.tar.gz)
installs into `~/.local` with `./install.sh`, and Arch users can build the pacman package in
[`packaging/aur`](packaging/aur/omnipty-bin).

Requirements, every install route, and building from source are in the
[install docs](https://omnipty.com/docs/install/).

## Documentation

The docs live at **[omnipty.com/docs](https://omnipty.com/docs/)**, with a page for every
feature plus [configuration](https://omnipty.com/docs/configuration/),
[keybindings](https://omnipty.com/docs/keybindings/), and
[troubleshooting](https://omnipty.com/docs/troubleshooting/). The
[changelog](https://omnipty.com/changelog/) covers each release, and there's an
[honest comparison](https://omnipty.com/compare/) with iTerm2, Ghostty, kitty, WezTerm,
Alacritty, and Warp.

## Contributing

Pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers getting a build
running, how the code is put together, and what to update alongside a change. For anything
bigger than a small fix, open an issue first so we can talk it through before you write
the code.

Not a Rust person? [Sponsoring on GitHub](https://github.com/sponsors/omnipty-terminal) or
[buying me a coffee](https://www.buymeacoffee.com/bobbycoleman) helps too.

## Issues and feature requests

[Open an issue](https://github.com/omnipty-terminal/omnipty/issues/new/choose) and pick the
bug report or feature request template. The app gets you there too: **Help → Report an
Issue** (the ☰ menu on Linux), or *Report an Issue* in the command palette.

Questions and setup help go to [Discord](https://discord.gg/APV9FYGgeh). Please report
security issues privately to [security@omnipty.com](mailto:security@omnipty.com).

## License

MIT, see [LICENSE](LICENSE). OmniPTY builds on [GPUI](https://www.gpui.rs) and
[`alacritty_terminal`](https://crates.io/crates/alacritty_terminal), both Apache-2.0.
