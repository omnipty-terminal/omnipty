# Changelog

All notable changes to OmniPTY (Oxide before 0.9.0). The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

Add entries under **Unreleased** as work lands. `scripts/bump.sh` turns that
section into the versioned one, and `scripts/release.sh` publishes it as the
GitHub release notes. **Help → What's New** shows this file inside the app, so
write for users: what changed and why it matters, not which files moved.

## [Unreleased]

### Fixed

- **Command-line tools can ask for Calendar, Reminders and other access.**
  Tools that read your calendars or reminders (like `ical`), control other
  apps with AppleScript, or use the camera or microphone were silently denied
  in OmniPTY, with no prompt and no entry in System Settings. macOS now asks
  you the first time, as it does in Terminal and iTerm2.
  ([#11](https://github.com/omnipty-terminal/omnipty/issues/11))

## [0.9.2] - 2026-10-22

### Added

- **Crash reports, if you want them.** OmniPTY can now send the developer a
  report when it crashes: the stack trace, the OmniPTY version, and the OS
  version, so crashes on other people's machines get fixed instead of going
  unnoticed. A handful of failures that used to vanish silently are counted
  the same way: a shell that won't start, a `workspaces.json` that won't
  parse, an update that doesn't install. It's off until you say yes: the
  first launch asks once, in a toast with Send reports and No thanks, and
  either answer is written to `[crash_reports] enabled` in the config, where
  you can change it any time. A report never contains anything shown in or
  typed into a terminal: no output, commands, paths, directories,
  environment, or workspace and tab names, and no hostname, user ID, or IP
  address. Your home directory is replaced with `~` wherever it might
  appear. Reports go to Sentry. Saying yes also sends a start and a stop
  ping each launch, so a crash count means something next to a launch
  count, and one line after each update saying it landed. After a crash,
  the next launch shows the report's ID, and Help → Report an Issue
  pre-fills it into the new issue.

### Fixed

- On Linux, a command that printed and exited in the same instant (`exit 3`
  right after an error message, `omnipty -e <cmd>`, a workspace startup
  command) could leave its pane stuck on the dead shell's last screen, with
  no "[process exited]" overlay and no way to close it. The last line of
  output could be lost the same way, on both platforms. Both are fixed: the
  output is read before the exit is announced, and the hangup is treated as
  the end of the output, not as an error.

## [0.9.1] - 2026-10-18

### Verified

- OmniPTY has been officially verified to work natively an Xorg session under LXQT. ([(#8)](https://github.com/omnipty-terminal/omnipty/pull/8),
  thanks [@agustux](https://github.com/agustux) for contributing!)

### Fixed

- In bash, sending a grouped command or a loop to a file (`{ …; } > file`,
  `for …; done > file`) no longer puts an invisible escape sequence at the
  start of that file. The shell integration's "command started" signal now
  goes straight to the terminal. zsh wasn't affected.

## [0.9.0] - 2026-10-10

### Changed

- **Oxide is now OmniPTY.** Same terminal, new name: there's an unrelated
  company called Oxide and the clash was confusing people. If you already had
  Oxide installed, this update does the move for you: the app becomes
  `OmniPTY.app`, your `config.toml`, themes and pinned workspaces are copied
  to `~/.config/omnipty` and `~/.cache/omnipty` (the old directories are left
  as they were), and `theme = "oxide"` still selects the same palette, now
  also called `omnipty`. Shells get `OMNIPTY_SESSION` and `OMNIPTY_VERSION`;
  the old `OXIDE_*` names stay exported through 0.9.x, so update any
  dotfiles that read them before 0.10. A Dock icon pinned to the old
  `Oxide.app` path needs re-adding. Homebrew users: switch casks with
  `brew uninstall --cask --force oxide-terminal && brew install --cask
  --adopt omnipty-terminal/tap/omnipty`. The website is now omnipty.com;
  the old addresses redirect.
- `OMNIPTY_MANIFEST_URL` points the updater at a different manifest, for
  testing a release before it's published.

## [0.8.2] - 2026-10-09

### Changed

- What's New opens on its own after this update (instead of the usual
  toast), so the note above about the upcoming rename is seen once. The
  next update goes back to the toast.

### Security

- The status bar and file tree poll `git status` in whatever directory a
  pane is in. Git runs the `core.fsmonitor` command from a repository's own
  `.git/config` during `status`, so an unpacked archive or shared folder
  carrying a planted one could run code the moment you `cd` into it. Those
  polls, and the git segment of Oxide's generated prompt, now switch
  fsmonitor off.
- Dragging a file into the terminal, "Insert Path", and the `cd` fallback
  used without shell integration type the quoted path at the prompt. A
  filename holding control characters (`^U`, carriage return) could clear
  the line and run a command of its own. Such names are refused with a
  notice instead.
- OSC 8 hyperlinks only open `http`, `https` and `mailto` targets. Because
  the link's text can say anything, a hidden `file://` or app-scheme target
  could launch something on a cmd-click. While you hold cmd over a link,
  the real address is shown in the pane's corner, browser-style, and the
  whole link is underlined rather than one cell.
- OSC 7 working-directory reports are ignored while the pane's foreground
  process is `ssh`, so a remote host can't point the file tree and git
  status at a local directory of its choosing.

## [0.8.1] - 2026-10-07

### Fixed

- A markdown preview (and Help → What's New) now re-flows to fit its pane
  when the pane is resized. A preview opened in a thin split stayed that
  thin after the split was widened; now the text, tables and code boxes
  are laid out again for the new width.
- Running `source ~/.bashrc` in a bash tab no longer crashes the shell.
  Re-running starship's setup wrapped Oxide's prompt hook inside
  starship's, and the two then called each other until bash ran out of
  stack and segfaulted, taking the tab's unsaved history with it. The hook
  now notices when it is already running and steps aside, and sourcing
  Oxide's own init file twice can no longer make it call itself.

## [0.8.0] - 2026-10-02

### Added

- Inline images. Oxide now draws the pictures programs send, over all three
  protocols in use: kitty graphics (`kitten icat`, yazi, snacks.nvim,
  `timg`), iTerm2's (`imgcat`, `wezterm imgcat`) and sixel (`img2sixel`,
  `lsix`, `chafa -f sixel`). PNG, JPEG, WebP and GIF (the first frame) are
  all read, and on a Retina display images are drawn at the display's full
  resolution. A picture is part of the scrollback like the text around it:
  it scrolls, survives a window resize and a font zoom, goes when you
  `clear` or press `cmd-k`, and a selection copied across one is just the
  text. kitty's Unicode placeholders are supported, which is what lets
  images through tmux (`set -g allow-passthrough on`). Not there yet:
  animation, and kitty's layers — text printed over an image replaces that
  part of it instead of sitting on top. A new `[images]` section has
  `enabled`, and `memory_limit` for how much decoded image memory a pane may
  hold (128 MB by default).
- Opening a picture shows the picture. `enter` on a PNG, JPEG, GIF, WebP or
  BMP in the file tree — or picking one in the `cmd-p` finder, or
  `cmd-click`ing its path in output — opens it in a tab of its own, scaled
  down to fit when it's larger than the pane. Any key closes it. Set
  `images.preview_in = "split"` to have it open beside what you're doing
  instead. It used to be handed to `$EDITOR` like any other file.

### Changed

- Programs now see Oxide for what it is, and see its real size.
  `TERM_PROGRAM` is always `Oxide` — a shell launched from another terminal
  used to inherit that terminal's name, and tools would speak its dialect.
  The window's pixel size is reported in real device pixels (it used to be
  in points, rounded down, so image tools on a Retina display drew at half
  resolution), and stays right after a font zoom or a move to another
  display. Queries for the cell size (`CSI 16 t`) and the terminal's name
  and version (XTVERSION) are answered where they used to be ignored. With
  images on, the device-attributes reply is now `?62;4;22c` rather than
  `?6c`: the `4` is how sixel programs learn they can draw. Worth knowing if
  anything of yours matched the old value.

## [0.7.3] - 2026-10-02

### Fixed

- A startup command that opens an interactive session — `ssh -t` into
  another machine, a REPL — now shows what you type when your shell is bash.
  bash handed the command its line editor's terminal settings, with echo
  off, and `ssh` carried them over to the remote machine: the remote prompt
  came up, but your typing stayed invisible. zsh wasn't affected.

## [0.7.2] - 2026-09-30

### Added

- The text inputs — the command palette, the file finder, command history,
  the new/rename/move prompts, startup commands, and the tree's filter — now
  take the mouse and a selection. Click to put the cursor somewhere, drag or
  shift-click to select, or hold `shift` with any of the keys that move the
  cursor (`shift-←`, `shift-opt-←`, `shift-cmd-→`…). Typing or pasting
  replaces the selection. In the palette, finder, history, prompts and
  startup commands, `cmd-a` selects everything and `cmd-c` / `cmd-x` /
  `cmd-v` copy, cut and paste (`ctrl-shift-` on Linux).

### Changed

- A new app icon: the terminal prompt in a ring that's rusting away at one
  edge. It's in the Dock, the Linux app menu and on oxideterminal.com.

### Fixed

- A long startup command wraps onto more lines instead of running off the
  edge of its box, both while you type it and in the list of a workspace's
  commands. The other inputs wrap the same way.

## [0.7.1] - 2026-09-30

### Added

- `bell = "sound"` now plays the system bell sound on Linux, through
  `canberra-gtk-play` or `paplay`. With neither installed, it falls back to
  the visual flash. ([(#4)](https://github.com/omnipty-terminal/omnipty/pull/4),
  thanks [@agustux](https://github.com/agustux) for contributing!)
- Dragging a file or directory in the file tree scrolls the tree when the
  drag nears its top or bottom edge, so a destination that's out of view is
  one drag away instead of a drag, a scroll, and another drag.
- The file tree keeps a strip of empty space at the bottom of its panel. A
  tree full of directories used to leave nowhere to drop for the root; now
  there always is, wherever the tree is scrolled to.
- The file tree's right-click menu does more. On a file or directory it adds
  **Rename…** and **Delete** (to the Trash, after the same `y` the `d` key
  asks for); on a directory, **New file or folder…** inside it. Right-click
  the root's name at the top, or the empty space under the rows, to add to
  the root.
- `P` (`shift-p`) in the file tree previews the selected markdown file, the
  same as **Preview markdown** on right-click. It's the `tree::preview`
  action, if you'd rather bind another key (`p` alone is "select parent").
- Selecting text with the mouse scrolls the terminal when the drag reaches
  the pane's first or last row, and keeps scrolling for as long as you hold
  it there — faster once you're past the pane's edge — so a selection can
  run longer than the screen.
- Each workspace in the drawer has a `×` that deletes it, straight away.
  `d` still asks for a `y` first.
- `tabs.close_last = "workspace" | "new_tab"` chooses what closing a
  workspace's last tab does. `"workspace"` (the default, and how it has
  always worked) closes the workspace with it, and the window with the last
  workspace. `"new_tab"` keeps the workspace and leaves it a fresh tab in
  your home directory.
- Double-click the drawer's right edge to put it back to `tree.width`.

### Fixed

- Text in the small input boxes (the palette, rename, the tree's filter and
  prompts) stays put as the cursor moves through it. The caret used to push
  the letters after it sideways, so they jiggled with every arrow key.
- A tab's `×` closes the whole tab when it's the workspace's last one, as it
  does for any other tab. It used to close only the focused pane of a split.
- A new workspace is named `workspace 1` again once nothing else has that
  name, and otherwise takes the lowest number that's free. The number used
  to keep counting up for as long as the window was open, so deleting
  `workspace 1` and adding another gave you `workspace 2`.
- A drag in a program that tracks the mouse (neovim, tmux, lazygit) keeps
  going when the pointer leaves the pane: the program hears about the
  nearest cell, and about the release. It used to hear nothing past the
  pane's edge, so a neovim selection stopped scrolling there and the
  program could be left thinking the button was still down. Drags are also
  reported once per cell rather than sixty times a second on macOS.
- A mouse selection that ends outside the pane it started in now ends there.
  The pane used to miss the release and keep selecting the next time the
  pointer crossed it with a button down.

## [0.7.0] - 2026-09-28

### Added

- Updates now come from downloads.oxideterminal.com and are signed. Oxide
  checks each downloaded update against a key built into the app before
  offering to install it, and never installs one that doesn't match — even if
  the download or the server were tampered with. GitHub is no longer in the
  update path on macOS, so the check works even when GitHub is rate-limiting
  or down. Linux reads the same source to learn about a release; the pill
  still opens the release page, since packages come from the AUR or the
  tarball.

- Drag a file or directory in the file tree to move it: drop it on a
  directory to move it inside, on a file to move it beside that file, or on
  the empty space below the rows to move it to the tree's root. Nothing is
  overwritten, same as `m`.
- Drag a workspace in the drawer to reorder it, the way tabs already work.
  `cmd-alt-1..9` follow the new order, and so do pinned workspaces after a
  restart.
- Drag the drawer's right edge to resize it. The width is remembered across
  launches; changing `tree.width` in the config resets it.
- `markdown.preview_in = "tab" | "split"` chooses where a markdown preview
  opens. `"split"` puts it beside the pane you're in, laid out for that
  narrower width. The default is still a new tab.
- `editor.open_in = "tab" | "split"` chooses where a file opens when the
  focused pane can't take it (see Fixed). The default is a new tab.

### Changed

- The markdown preview (and Help → What's New) now reads all of markdown,
  not the subset it used to. New: ~~strikethrough~~, `_underscore_`
  emphasis, lists nested to any depth with paragraphs and code inside
  their items, numbered lists that keep their numbers, quotes inside
  quotes, GitHub's `> [!NOTE]` alerts, footnotes, reference-style links,
  `<https://…>` autolinks, headings underlined with `===`, indented code
  blocks, definition lists, YAML front matter, and backslash escapes.
  Styles nest properly, so a bold or struck-through sentence with `code` in
  it stays bold or struck through to its end. Long lines wrap under their
  own text instead of back at the left edge, and blocks are set apart by a
  blank line.
- Adding, renaming, and moving in the file tree, and adding and renaming
  workspaces, now ask for the name in a small modal in the middle of the
  window, titled with what you're doing ("Rename main.rs"), instead of a
  line squeezed into the drawer's footer. Renaming a tab uses the same one.
  The tree's `/` filter stays in the drawer, next to the rows it filters.

### Fixed

- The scroll wheel scrolls the view in Neovim, LazyGit, and other programs
  that track the mouse. It was being sent as arrow keys, so it moved the
  cursor instead. Programs that don't track the mouse (`less`, `man`) still
  get arrow keys, and shift + scroll still bypasses the program.
- Opening a second file from the file tree, the file finder, or a
  `cmd-click` while a terminal editor still has the first one open no longer
  does nothing. The file opens in a new tab (or a split, with
  `editor.open_in`) that closes when you quit the editor. The same goes for
  Open Settings, and for any pane that's busy running something else.
- macOS: `window.blur = true` blurs what's behind a translucent window
  again. On recent macOS it did nothing, and windows behind Oxide showed
  through sharp.
- `window.opacity` now applies to the whole window evenly. The file tree,
  tab bar, and status bar used to stay solid while the workspaces panel was
  more see-through than the terminal. The terminal also came out more
  opaque than the number you set (0.5 looked like 0.75); it now matches, so
  an existing setting will look a little more transparent than before.
  Panes dimmed by `window.inactive_pane_opacity` stay as see-through as the
  focused one: only their contents fade, where before the dimming also made
  them more solid.
  The active tab is the same: lighter than the bar, not more solid.
  Opening the command palette or any other modal no longer turns a
  translucent window dark and solid; the modal simply sits on top.
- Changing `window.opacity` or `window.blur` in the config applies to open
  windows straight away, including going between solid and translucent.
  That used to need a restart.
- A markdown preview or What's New page shorter than the pane opens at the
  top of it, not pushed down to the bottom.
- Text fields have a real cursor and word editing everywhere: the command
  palette, command history, file finder, startup commands, the tree filter,
  and the new prompt modal. Option+Left/Right move by word, Option+Delete
  deletes the word before the cursor, Cmd+Left/Right jump to the ends, and
  Cmd+Delete clears back to the start (Ctrl+arrows and Ctrl+Backspace on
  Linux). Before, most of these fields could only append and backspace.

## [0.6.3] - 2026-09-26

### Fixed

- macOS: Option+Left/Right now jump by word and Option+Delete deletes the
  previous word in zsh and bash, matching Terminal.app and Ghostty. Before,
  Option+arrows rang the bell and printed `;3D` or `;3C`, and Option+Delete
  only deleted one character. Works regardless of `option_as_meta`. Linux
  keeps the standard xterm encoding for Alt+arrows, which tmux, vim and fish
  depend on. (#3)

## [0.6.2] - 2026-09-25

### Fixed

- Linux: the release tarball starts again on Ubuntu 24.04, Debian 12 and any
  other distro with a glibc older than 2.44. Earlier tarballs were built on
  Arch and inherited its glibc 2.44, so everywhere else they died with a
  `GLIBC_2.43 not found` error before a window appeared. Releases are now
  built against glibc 2.35 — Ubuntu 22.04, Debian 12, Fedora 36 and
  everything newer — and the packaging step refuses to ship a binary that
  needs more. (#2)

## [0.6.1] - 2026-09-24

### Added

- Linux: a ☰ button in the window's top-left corner opens the same Oxide,
  File, Edit, View, Window, and Help menus macOS shows in the menu bar, each
  entry with its shortcut. Whichever bar is in that corner (file tree header,
  tab bar, or a top status bar) makes room for it. `app::menu` opens it from
  the keyboard or the palette, for when every bar is hidden.
- `oxide --version` / `-V` prints the version, and `--help` / `-h` the usage.
- `oxide -e <command> [args...]` runs a program in the first pane instead of
  the shell, and `--app-id <id>` sets the window's Wayland app-id / X11 class.
  Together they are what `xdg-terminal-exec` passes to a terminal, so Oxide
  can now be the system default on Linux: on Omarchy, `omarchy default
  terminal`-style launches (Super+Return, `omarchy launch tui btop`) open in
  Oxide. The pane closes when the program exits cleanly; a failure keeps its
  output on screen, and the `.desktop` entry now declares both flags.

### Changed

- **About Oxide** now opens a panel in the window with the app icon, the
  version, and a link to the site, instead of sending you straight to the
  website. `⏎` or `esc` closes it.
- `oxide` with no directory opens at the directory it was run from, as the
  docs always said, rather than at home. Launchers that hand the app `/`
  (Finder) still get home. Unknown options are now an error instead of being
  silently ignored.

### Fixed

- Linux (bash): the status bar no longer shows a `printf "\033]0;…"` title
  hook as a command that never finishes. bash 5.1+ keeps `PROMPT_COMMAND`
  as an array, and Arch's bashrc, starship, and zoxide all append to it;
  Oxide's shell integration kept only the first entry and left the rest
  running outside its hook, where they were logged as commands and stole
  the start marker from whatever you typed next. Every entry now runs inside
  Oxide's hook, and the init script itself is no longer logged either.

## [0.6.0] - 2026-09-22

### Added

- **Linux.** Oxide now builds and runs on Linux (Wayland and X11, Vulkan), as
  a pacman package (`makepkg -si` from `packaging/aur/oxide-terminal-bin`;
  the AUR listing follows once registration reopens) or a release tarball
  with an install script. Everything that isn't the window chrome works the same: the tree
  follows `cd` and tab titles follow the foreground command through procfs,
  finished commands notify through `notify-send` (and a click on the
  notification brings the pane back where the daemon supports it), deleted
  files go to the XDG trash, "Reveal in File Manager" opens your file
  manager, and a newer release shows up in the top-right pill (it opens the
  release page; the package manager does the install).
- A Linux keymap: `ctrl-shift-*` where macOS uses `cmd-*` (copy, paste, new
  tab, search, palette…), `alt-1..9` for tabs and `ctrl-alt-1..9` for
  workspaces, `ctrl-click` to open links and paths. Super is never bound —
  it belongs to the window manager. The `ctrl-w` chords are identical on
  both. Palette and overlay hints read `Ctrl+Shift+P` rather than `⇧⌘P`.
- Linux: selecting text sets the primary selection, and middle-click pastes
  it.
- Shift held while Oxide launches skips startup commands on Linux too; the
  key state arrives with keyboard focus rather than at launch, so it's read
  in the first moment after the window opens.
- Linux: closing the last window quits, rather than lingering with nothing
  to reopen from.

## [0.5.8] - 2026-09-21

### Added

- `m` in the file tree moves the selected file or directory (also **Move…** on
  right-click). The input is prefilled with its path relative to the tree root,
  cursor at the start so a directory can be typed in front; edit and press enter. An existing directory as the destination moves the
  entry into it, missing directories are created, `~/` and absolute paths work,
  and nothing is ever overwritten.
- The file tree's add, rename, and move inputs have a cursor: arrows, home/end
  (`cmd-←`/`cmd-→`, `ctrl-a`/`ctrl-e`), and forward delete work, so you can
  put a directory in front of a prefilled name instead of retyping it.

### Changed

- `a` in the file tree creates beside a collapsed directory, not inside it;
  an expanded directory still takes the new entry. Before, a tree of nothing
  but directories had no way to add at the top level. The input now says
  where the new entry will go.

### Fixed

- The "updated to vX" toast stayed until clicked. It now fades after
  twenty seconds, and every toast has an `×` to dismiss it without following
  its click action (the update toast opens What's New; Help → What's New
  gets you there later).

## [0.5.7] - 2026-09-20

### Added

- `cmd-alt-1` … `cmd-alt-9` jump straight to a workspace, in the order the
  workspaces panel lists them — the workspace twin of `cmd-1..9` for tabs.
  Rebind them through `workspace::select_1` … `workspace::select_9`.
- The tab bar can be hidden, like the status bar: **View → Toggle Tab Bar** (or
  `app::toggle_tab_bar` from the palette or a keybinding) flips it for the
  current window, and `tabs.enabled = false` keeps it off. `cmd-1..9`, `cmd-t`
  and the rest keep working without it.
- Tabs show a small position number at their left edge — the `n` in `cmd-n`.
  `tabs.show_numbers = false` turns them off.
- The status bar shows which tab you're on, in a chip beside the workspace
  name, so you keep your bearings with the tab bar hidden. It shows the tab's
  position by default — `2/5` for the second of five tabs;
  `status_bar.tab = "name"` shows its name instead.
- `tree.open_on_startup = false` starts Oxide with the drawer hidden, for when
  you'd rather have the full width and call the tree up with `cmd-b`. It
  defaults to `true`, so nothing changes unless you set it.

### Changed

- The command palette offers the workspace actions — rename, pin, delete,
  edit startup commands — even with the drawer hidden, and typing "rename
  workspace" or "pin workspace" finds them. Running one opens the drawer on
  the current workspace. Before, they only appeared while the drawer was
  showing, under titles those searches didn't match.

## [0.5.6] - 2026-09-19

### Changed

- Closing a markdown preview (or What's New) returns to the tab you opened it
  from, instead of jumping to the last tab.
- The key hints under the file finder and command history follow your keymap:
  rebind `overlay::confirm_reveal` (say, because a window manager owns
  `alt-enter`) and the footer shows the new key instead of `⌥⏎`.

### Fixed

- The file finder (`cmd-p`) stalled on every keystroke in large trees — and for
  a quarter of a second just opening it once you had a list of recent files.
  Opening is now instant and typing is 5–10× faster, with the same results in
  the same order. Command history search (`cmd-r`) got the same speed-up.
- Long file names in the tree wrapped onto a second line and overlapped the row
  below. They now stay on one line and end in an ellipsis, with the full name
  on hover. Long workspace names get the same treatment.
- Gitignored files vanished from the file tree entirely, with no way to bring
  them back. They now stay in the tree, dimmed. Set `respect_gitignore = false`
  under `[tree]` to show them like any other file.

## [0.5.5] - 2026-09-18

### Added

- **Preview markdown** on the file tree's right-click menu for `.md` and
  `.markdown` files. It opens the file rendered in a new tab, paged with
  `less`; `q` closes it. Headings, lists, task boxes, quotes, and inline styles
  render; code blocks are boxed, syntax-highlighted in your theme's own
  colors, and carry a **⧉ copy** link that puts the block on the clipboard
  (without a trailing newline, so a pasted command waits for you to press
  return); tables are drawn with
  aligned columns and cells wrapped to fit the tab; and README-style HTML
  (`<h1>`, centered `<p>`, `<a>`, `<img>`, `<kbd>`, comments, entities) is
  understood rather than shown as tags. Link URLs are shown so `cmd-click` opens them, and relative links
  resolve against the file's directory.

### Changed

- What's New and markdown previews wrap long lines at spaces instead of
  mid-word when your `less` supports `--wordwrap`. Links in What's New now show
  their URL, so they're clickable too.

## [0.5.4] - 2026-09-18

### Added

- 17 new theme presets from [Omarchy](https://omarchy.org/manual/themes/):
  `ethereal`, `everforest`, `flexoki-light`, `hackerman`, `kanagawa`,
  `last-horizon`, `lumon`, `lupine`, `matte-black`, `miasma`, `osaka-jade`,
  `retro-82`, `ristretto`, `rose-pine-dawn`, `solitude`, `vantablack`, and
  `white`. That makes 25, four of them light. The theme picker (`cmd-alt-t`)
  now scrolls to fit them all.

## [0.5.3] - 2026-09-18

### Fixed

- Right-click context menu in file tree drawer was clipping under the terminal and workspaces panel instead of displaying on top becasue it was a child of the tree rather than the app.

## [0.5.2] - 2026-09-13

### Added

- In-app toasts in the bottom-right corner replace the yellow strip across the
  top of the window. Config and keymap errors show in red and stay until the
  next clean reload; other notices fade after a few seconds. Click any toast to
  dismiss it.
- The first launch after an update shows a toast; clicking it opens the
  changelog, rendered, in a new tab of the current workspace. **Help → What's
  New** (`app::changelog`) opens the same tab any time, and it's in the command
  palette.
- A failed update download is reported even on the automatic check, since the
  check itself just succeeded so the machine isn't offline.
- `scripts/bump.sh <version>` prepares the release commit (Cargo.toml,
  Cargo.lock, this file, RELEASING.md), and `scripts/release.sh` builds the
  notarized DMG and publishes the GitHub release with notes from this file.

## [0.5.1] - 2026-09-13

### Fixed

- Double-clicking a directory in the file tree toggled it twice, so it ended up
  back where it started. Directories now open on the first click and stay open.
- A hardcoded path left over from testing was removed.

### Changed

- Each release now uploads the DMG twice: `Oxide-x.y.z.dmg` for the website's
  download button and `Oxide-x.y.z-update.dmg` for the in-app updater. GitHub
  counts downloads per asset, so the website's counter reflects people who
  chose to download rather than installed copies updating themselves. The
  updater prefers the `-update` asset and falls back to the plain one for
  older releases.
- Small cleanups from a code review: simpler pane and theme code, a redundant
  assertion dropped from the keymap tests.

## [0.5.0] - 2026-09-09

### Added

- **Startup commands**, the tmuxinator move. Give a pane a command with
  `ctrl-w r` (prefilled with the last thing that ran there) and a pinned
  workspace re-runs it on restore, once that pane's shell is actually at a
  prompt rather than blindly typing into a shell that's still loading its rc
  files.
- Per-pane **on exit** behaviour: drop back to the shell, close the pane, or
  restart with exponential backoff. A breaker stops the restarts after five
  exits inside a minute, so a crash-looping command can't spin forever; a run
  that lasts over a minute resets the count.
- `e` in the workspaces panel (also on right-click) edits every pane's startup
  command in one overlay.
- `--no-startup-commands` on the command line, or holding shift at launch,
  restores pinned layouts without running anything. The escape hatch for a
  command that wedges the app.
- `[workspaces]` config: `run_startup_commands` and `startup_timeout`.
- Shell integration gained a per-session run channel, so Oxide can hand a
  command to a specific shell without it echoing as typed input.

### Changed

- `workspaces.json` moved to a v3 format that carries startup commands. Older
  files still load.

## [0.4.0] - 2026-09-08

### Added

- **Scrollback search** grew regex, case-sensitive, and whole-word toggles as
  clickable chips (`cmd-alt-r` / `c` / `w`). A malformed regex says so instead
  of silently matching nothing.
- **Copy mode** (`ctrl-w [` or `cmd-shift-v`) turns the scrollback into a vim
  buffer: `hjkl`, word motions, `0 ^ $`, `gg G`, `H M L`, half and full page,
  counts like `5j`, `/` and `?` search with `n`/`N`, and `v` / `V` / `ctrl-v`
  selection. `y` yanks and leaves. Nothing typed reaches the shell until you
  `esc`.
- **tmux reflexes for panes**: `ctrl-w z` zooms a pane to the full tab,
  `ctrl-w b` broadcasts typing to every pane in the tab (with a red border and
  a status-bar pill so you can't forget it's on), `ctrl-w o` closes the
  others, `ctrl-w x` swaps with the neighbour.
- **Light and dark**: `follow_system = true` switches between `preset_dark`
  and `preset_light` with the macOS appearance. Every preset colour is
  individually overridable, including `selection_fg`.
- **SSH awareness**: Oxide watches the foreground process. An `ssh` session
  shows `ssh: host` in the status bar, and `[[ssh.hosts]]` glob patterns give
  a host an accent colour on the pane border, so a production box is visibly
  red. The process name also titles the tab (`vim`, `cargo`, `ssh prod-web`).
- **Tabs**: double-click or `ctrl-w ,` to rename (names persist with pinned
  workspaces), drag to reorder, `cmd-shift-t` reopens the last closed one,
  and a close-all-tabs action.
- A `[cursor]` section: block, bar, or underline; blink rate; how the cursor
  looks when the pane is unfocused. vim's per-mode DECSCUSR shapes are
  honoured.
- `window.inactive_pane_opacity` dims the panes you aren't in, and
  `inactive_window_opacity` dims the whole window when another app is
  frontmost.
- `font.family` accepts a fallback list, so CJK and emoji render even when the
  primary font lacks them.
- A "2,340 lines above" pill while scrolled up, `cmd-k` to clear scrollback,
  and a configurable bell.

### Changed

- The theme picker moved to `cmd-alt-t`; GPUI's chord handling made the bare
  `cmd-k` prefix lag.

## [0.3.4] - 2026-09-05

### Added

- **Command awareness.** The shell integration's OSC 133 markers are read
  straight off the PTY, so Oxide knows what's running, how long it took, and
  whether it failed: elapsed time in the status bar, activity dots on
  background tabs, a red flash on a background pane whose command failed, and
  a failure gutter you can click to jump back to the command.
- **Desktop notifications** for long or failed commands in panes you aren't
  watching. Clicking one focuses the pane. Programs can post their own with
  OSC 9 / OSC 777. Thresholds live under `[notifications]`.
- **Command history** (`cmd-r`) searches every command run in any pane, with
  its directory and exit status. `⏎` inserts it at the prompt, `⌘⏎` runs it.
  `cmd-shift-c` copies the last command's output.
- **Fuzzy file finder** (`cmd-p`) over everything under the tree root: `⏎`
  opens, `⌘⏎` inserts the path at the prompt, `⌥⏎` reveals it in the tree.
- **The tree/terminal seam**: `y` inserts the selected path at the prompt,
  quoted and relative (`Y` for absolute); `cmd-shift-r` reveals the shell's
  directory; right-click a row to re-root, copy the path, `cd` the shell
  there, or reveal in Finder; drag rows or drop files onto a pane.
- `cmd-click` a `path:line:col` in output opens it in `$EDITOR` at that line.
  nvim, VS Code, emacs, Sublime, and Helix argument dialects are built in;
  `editor.open_at_line` overrides.
- Tree rows are coloured by git status (`tree.git_status`).

## [0.3.3] - 2026-09-04

### Added

- **Command palette** (`cmd-shift-p`) lists every action with its binding,
  fuzzy-matched across title, category, and aliases, most-recently-used
  first. Focus returns to the pane you were in before the action runs.
- **Configurable keys.** A `[keymap]` table in config.toml rebinds anything:
  flat pairs bind at the root, subtables (`[keymap.terminal]`,
  `[keymap.file_tree]`, …) bind per context, `""` unbinds, and
  `replace_defaults = true` starts from nothing. An unknown action id gets a
  did-you-mean, a keystroke that doesn't parse is skipped, and a bare key that
  would steal from your shell is refused; all errors land in one banner while
  the rest of the map still binds. Reloads live.
- An action registry behind both: every action now has a stable public id,
  and the palette, the keymap, and the menu bar read from the same table.
- **Resizable splits.** Drag a divider, or `ctrl-w < > - +` to resize by a few
  cells and `ctrl-w =` to equalise. Panes have a 20×3 cell minimum.
- `scripts/docs.sh` serves the docs site locally.

### Changed

- `workspaces.json` gained a version field. v0.3.2 files still load and get
  even splits.

### Known issues

- Narrowing a pane past the width of a multi-line bash prompt can scroll the
  prompt's first line into scrollback until the next prompt is drawn.

## [0.3.2] - 2026-09-02

### Added

- The documentation site at oxideterminal.com/docs: install, terminal, file
  tree, tabs and splits, workspaces, prompt, themes, keybindings,
  configuration, and troubleshooting pages.
- Shell integration for fish, nushell, and the csh family, alongside zsh and
  bash. Anything Oxide types at a prompt is now routed through `/bin/sh` on
  shells where the Bourne one-liner would be a syntax error.

### Changed

- "Open Settings" runs the editor command silently instead of echoing it at
  your prompt.

### Fixed

- Paths containing `'`, `\`, or `!` open correctly in every supported shell.

## [0.3.1] - 2026-09-02

### Added

- JetBrainsMono Nerd Font Mono is bundled, so a machine with no Nerd Font
  installed still gets every powerline and tree glyph. Set `font.family` to
  use your own.
- Double-clicking the window's padding band zooms the window, respecting the
  System Settings double-click action, matching a real titlebar.

### Fixed

- Opening a file with no `$EDITOR` set now hands it to the macOS default text
  editor instead of failing with "command not found: nvim".
- On a Mac without the Command Line Tools, the git status poll no longer
  triggers Apple's "Install Developer Tools?" dialog every three seconds.
- Shell integration tests pass on Apple's bash 3.2 as well as Homebrew bash 5.

## [0.3.0] - 2026-09-01

### Added

- **Workspaces**: named sets of tabs and splits, tmux-session style, managed
  from a panel below the file tree (`a` add, `r` rename, `d` delete, `p` pin;
  `ctrl-w p` focuses the panel, `tab` toggles tree ↔ workspaces). Temporary by
  default; pinned ones survive restarts, restoring layout, tabs, splits, and
  each pane's directory with fresh shells. Right-click for the same actions.
- **In-app tabs.** A Zed-style tab bar replaces macOS window tabbing, so tabs
  work everywhere, including under tiling window managers like AeroSpace and
  yabai that previously turned every tab into a window. `cmd-1..9` jump
  straight to a tab.
- MIT license, and the oxideterminal.com landing page.

### Fixed

- Workspace persistence writes atomically, so a crash mid-save can't leave an
  empty file.
- New workspaces start at `~` rather than wherever the previous shell was.

## [0.2.0] - 2026-08-31

### Added

- **Split panes.** Split in any direction (`ctrl-w v` / `s`, or `cmd-d` /
  `cmd-shift-d`) and nest freely. `ctrl-w h j k l` or `cmd-opt-arrows` move
  between panes by what's on screen; `exit` or `ctrl-w q` closes a pane and
  its neighbour reclaims the space.
- The file tree follows the focused pane, so switching splits re-roots it to
  that shell's directory.
- The oxideterminal.com landing page.

### Known issues

- Splits divide their space evenly and can't be resized yet.

## [0.1.1] - 2026-08-31

### Added

- Native macOS window tabs (`cmd-t`); new tabs start in the current directory
  or `~/` (`window.new_tab_directory`).
- The prompt switches with the file tree: `cd`-ing in the shell re-roots the
  tree, and picking a directory in the tree changes the shell's directory.
- `cmd-click` opens URLs, copy-on-select as an option, font size at runtime.

### Fixed

- `ls` output columns rendered wrongly.

## [0.1.0] - 2026-08-31

First release: a native macOS terminal emulator built on GPUI and
`alacritty_terminal`, with a vim-navigable file tree drawer.

- **Terminal**: full VT emulation with truecolor, wide glyphs and combining
  marks, bracketed paste, SGR mouse reporting, alternate-screen scrolling,
  OSC 8 hyperlinks, and OSC 52 clipboard. vim, htop, and tmux work as expected.
- **File tree drawer**: modeless vim navigation (`j`/`k`/`gg`/`G`, nvim-tree
  `h`/`l`), `/` to filter, `a`/`r`/`d` to add, rename, and delete to Trash.
  Respects `.gitignore`, watches the filesystem, and follows the shell's `cd`.
- **Scrollback search** (`cmd-f`), live and case-insensitive.
- **Prompt jumping** (`cmd-↑`/`cmd-↓`) between previous prompts via OSC 133.
- **Status bar** with cwd, git branch, dirty state, and ahead/behind.
- **Theme picker** with live preview across eight presets.
- **Configurable prompt**: a powerline prompt compiled from TOML segments and
  injected without touching your dotfiles. `prompt.enabled = false` keeps
  starship or p10k.
- **Native menu bar** and a **self-updater** that installs later releases with
  one click.
- Signed with a Developer ID and notarized, so the DMG opens with a normal
  double-click.

### Known limitations

- No IME or dead-key composition yet.
- No tabs or splits.
- Apple Silicon only.
