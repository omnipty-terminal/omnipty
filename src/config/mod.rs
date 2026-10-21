pub mod schema;
pub mod theme;

use std::path::PathBuf;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use notify::{RecursiveMode, Watcher as _};
use notify_debouncer_full::{DebouncedEvent, Debouncer, FileIdMap, new_debouncer};

pub use schema::Config;
pub use theme::Theme;

pub fn config_path() -> PathBuf {
    crate::paths::config_path()
}

/// Load the config file. Returns the parsed config (or defaults) plus an error
/// message when the file exists but does not parse — the caller keeps running
/// with whatever it had and shows the message in a banner.
pub fn load() -> (Config, Option<String>) {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(config) => {
                let warning = validate(&config);
                (config, warning)
            }
            Err(e) => (
                Config::default(),
                Some(format!("config error: {}", first_line(&e.to_string()))),
            ),
        },
        Err(_) => {
            // Missing file: write a fully-commented default on first run.
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, default_config_file());
            }
            (Config::default(), None)
        }
    }
}

/// Re-parse the config file, for live reload. `Err` carries a banner message.
pub fn reload() -> Result<Config, String> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).map_err(|e| format!("config unreadable: {e}"))?;
    let config = toml::from_str::<Config>(&text)
        .map_err(|e| format!("config error: {}", first_line(&e.to_string())))?;
    if let Some(warning) = validate(&config) {
        return Err(warning);
    }
    Ok(config)
}

fn validate(config: &Config) -> Option<String> {
    let colors = &config.colors;
    for (key, preset) in [
        ("preset", &colors.preset),
        ("preset_dark", &colors.preset_dark),
        ("preset_light", &colors.preset_light),
    ] {
        if let Some(preset) = preset
            && theme::canonical_preset(preset).is_none()
        {
            return Some(format!(
                "unknown color {key} \"{preset}\" — available: {}",
                theme::PRESET_NAMES.join(", ")
            ));
        }
    }
    for host in &config.ssh.hosts {
        if theme::parse_hex(&host.accent).is_none() {
            return Some(format!(
                "ssh.hosts: bad accent \"{}\" for \"{}\" — use #rrggbb",
                host.accent, host.pattern
            ));
        }
    }
    if !(0.05..=1.0).contains(&config.window.inactive_pane_opacity)
        || !(0.05..=1.0).contains(&config.window.inactive_window_opacity)
    {
        return Some(
            "window.inactive_pane_opacity / inactive_window_opacity must be between 0.05 and 1.0"
                .into(),
        );
    }
    if !(0.0..=1.0).contains(&config.cursor.thickness) {
        return Some("cursor.thickness must be between 0 and 1 (a fraction of a cell)".into());
    }
    None
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("parse failed")
        .trim()
        .to_string()
}

/// Watch the config file's parent directory (editors write-and-rename, so a
/// direct file watch misses saves). Returns the watcher (keep it alive) and a
/// receiver that fires on debounced changes to the config file itself.
pub fn watch() -> Option<(
    Debouncer<notify::RecommendedWatcher, FileIdMap>,
    UnboundedReceiver<()>,
)> {
    let path = config_path();
    let dir = path.parent()?.to_path_buf();
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let file_name = path.file_name()?.to_os_string();
    let mut debouncer = new_debouncer(
        Duration::from_millis(200),
        None,
        move |result: Result<Vec<DebouncedEvent>, Vec<notify::Error>>| {
            if let Ok(events) = result {
                let relevant = events.iter().any(|e| {
                    e.paths
                        .iter()
                        .any(|p| p.file_name() == Some(file_name.as_os_str()))
                });
                if relevant {
                    tx.unbounded_send(()).ok();
                }
            }
        },
    )
    .ok()?;
    debouncer
        .watcher()
        .watch(&dir, RecursiveMode::NonRecursive)
        .ok()?;
    Some((debouncer, rx))
}

/// The generated config, with the key names in its comments spelled for
/// this platform. The template is written in macOS terms; on Linux each
/// `cmd-…` mention becomes its `LINUX` keymap equivalent so the comments
/// don't describe keys that don't exist here.
pub fn default_config_file() -> String {
    if cfg!(target_os = "macos") {
        return DEFAULT_CONFIG_FILE.to_string();
    }
    let mut text = DEFAULT_CONFIG_FILE.to_string();
    for (mac, linux) in LINUX_KEY_SPELLINGS {
        text = text.replace(mac, linux);
    }
    text
}

/// Template-comment key names and their Linux spellings, longest first so
/// `cmd-shift-p` isn't rewritten as `ctrl-shift-shift-p`.
const LINUX_KEY_SPELLINGS: &[(&str, &str)] = &[
    (
        "\"cmd-shift-p\" = \"app::palette\"",
        "\"ctrl-shift-p\" = \"app::palette\"",
    ),
    ("\"cmd-d\"       = \"\"  ", "\"ctrl-shift-k\" = \"\""),
    ("the n in cmd-n", "the n in alt-n"),
    ("cmd-t", "ctrl-shift-t"),
    ("cmd-b", "ctrl-shift-b"),
    ("cmd-r", "ctrl-shift-r"),
];

const DEFAULT_CONFIG_FILE: &str = r##"# OmniPTY configuration.
# This file was generated on first run; every value shown is the default.
# Font and color changes apply live; [shell] and [prompt] changes apply to
# newly started sessions.

#bell = "none"                # none | sound | visual

[font]
family      = "JetBrainsMono Nerd Font Mono"
                              # or a list: the rest are fallbacks for CJK/emoji,
                              # e.g. ["JetBrainsMono Nerd Font Mono", "Apple Color Emoji"]
size        = 14.0
line_height = 1.25
weight      = "normal"        # normal | medium | bold
ligatures   = false

[window]
padding  = { x = 12, y = 8 }
opacity  = 1.0                # 0.0 - 1.0; < 1.0 makes the whole window's background translucent
blur     = false              # blur what's behind a translucent window
titlebar = "hidden"           # native | hidden
new_tab_directory = "pwd"     # pwd | home — where cmd-t starts
inactive_pane_opacity   = 1.0 # dim the panes you aren't in (0.5–0.9 typical)
inactive_window_opacity = 1.0 # dim the whole window when another app is active

[cursor]
style          = "block"      # block | bar | underline (programs can override via DECSCUSR)
blink          = true
blink_interval = 530          # ms per half-cycle
unfocused      = "hollow"     # hollow | solid | hidden — the cursor in an unfocused pane
thickness      = 0.15         # bar width / underline height, as a fraction of a cell

[shell]
# program = "/bin/zsh"        # default: $SHELL
args           = ["-l"]
scrollback     = 10000
option_as_meta = "none"       # none | left | right | both
                              # (left/right currently behave like "both")
integration    = true         # OSC 133 markers + silent cd from the file tree.
                              # Independent of [prompt]: keep your own prompt
                              # (starship, p10k) and still get integration.

[tree]
width             = 280
show_hidden       = false
respect_gitignore = true      # dim gitignored files in the drawer
indent            = 16
icons             = true      # nerd-font icons in the drawer
follow_cwd        = true      # re-root the tree when the shell cd's
git_status        = true      # colour rows by git state (modified, added, untracked…)
open_on_startup   = true      # false starts with the drawer hidden (cmd-b shows it)

[editor]
open_in = "tab"               # tab | split — where a file opens when the focused pane is
                              # busy (a terminal editor still showing the last file)
# open_at_line = "myeditor --line {line} {path}"   # for editors OmniPTY doesn't know

[markdown]
preview_in = "tab"            # tab | split — where a markdown preview opens

# bell = "none"               # none | sound | visual
# copy_on_select = false      # mouse selection copies to clipboard on release

[status_bar]
enabled  = true               # native bar showing cwd + git branch/dirty
position = "bottom"           # top | bottom
tab      = "number"           # number (2/5) | name — the current-tab chip beside the workspace

[tabs]
enabled      = true           # show the tab bar (View → Toggle Tab Bar flips it for the session)
show_numbers = true           # small position number on each tab — the n in cmd-n
close_last   = "workspace"    # workspace | new_tab — closing a workspace's last tab closes the
                              # workspace, or leaves it with a fresh tab in your home directory

[notifications]
enabled             = true    # notify when a command finishes in a pane you aren't watching
min_duration        = "30s"   # ...if it ran at least this long ("2m", "1.5s", or seconds)
only_when_unfocused = true    # stay quiet when the pane is focused and the window active
on_failure_always   = false   # a non-zero exit notifies regardless of duration
passthrough_osc9    = true    # let programs post notifications (OSC 9 / OSC 777)

[commands]
track        = true           # the command log: status bar, tab dots, cmd-r history, gutter
emit_cmdline = true           # the shell sends each command line to OmniPTY (memory only)
max_entries  = 500

[images]
enabled      = true           # draw the images programs send (kitty, iTerm2 and sixel)
memory_limit = 128            # decoded image memory per pane, in MB; past it, images that
                              # have left the buffer go first, then the least recently drawn
preview_in   = "tab"          # tab | split — where a picture opened from the tree is shown

[workspaces]
run_startup_commands = true   # re-run each pane's saved startup command when a pinned
                              # workspace is restored (skip once: --no-startup-commands,
                              # or hold shift while OmniPTY launches)
startup_timeout      = "5s"   # give up on a pane's command if its shell isn't ready by then

# [crash_reports]             # OmniPTY asks once, on first launch; set this to change your answer
# enabled = false             # when OmniPTY crashes, send the developer the stack trace,
                              # version and OS — never anything in your terminal. Also a
                              # start/stop ping each launch, so crash rates mean something

# [keymap]                    # keystroke = "action id"; see the keybindings docs
# "cmd-shift-p" = "app::palette"
# "cmd-d"       = ""          # unbind a default

# [[ssh.hosts]]                # colour a pane's border while ssh'd into a matching host
# match  = "*.prod.example.com"
# accent = "#f38ba8"

[colors]
# Presets: catppuccin-mocha | catppuccin-latte | gruvbox-dark | tokyonight
#          | dracula | nord | solarized-dark | omnipty
#          | ethereal | everforest | flexoki-light | hackerman | kanagawa
#          | last-horizon | lumon | lupine | matte-black | miasma | osaka-jade
#          | retro-82 | ristretto | rose-pine-dawn | solitude | vantablack | white
preset = "catppuccin-mocha"
# Follow the system appearance instead, switching between two presets:
# follow_system = true
# preset_dark   = "catppuccin-mocha"
# preset_light  = "catppuccin-latte"
# Any color can override the preset individually:
# background   = "#11111b"
# foreground   = "#cdd6f4"
# cursor       = "#f5e0dc"
# selection_bg = "#414458"
# selection_fg = "#cdd6f4"    # text inside a selection; unset keeps each cell's colour
# black / red / green / yellow / blue / magenta / cyan / white
# bright_black / bright_red / ... / bright_white

# The prompt is compiled into a zsh init script and injected via ZDOTDIR.
# Your own ~/.zshrc is sourced first; only PROMPT is overridden.
# Requires zsh or bash; other shells keep their own prompt.
[prompt]
enabled              = true
separator            = ""   # powerline right arrow
end                  = ""
newline_before_input = false

# Segment kinds: cwd | git | exit_status | time | user | host | duration
#                | text (options.text) | env (options.var)
[[prompt.segments]]
kind = "cwd"
fg   = "#11111b"
bg   = "#89b4fa"
bold = true
options = { style = "truncate_to_repo", max_len = 40 }   # full | truncate_to_repo | basename

[[prompt.segments]]
kind = "git"
fg   = "#11111b"
bg   = "#a6e3a1"
options = { show_dirty = true, dirty_bg = "#f9e2af", ahead_behind = true }

[[prompt.segments]]
kind = "exit_status"
fg   = "#11111b"
bg   = "#f38ba8"
options = { hide_on_success = true }

# [[prompt.segments]]
# kind = "time"
# options = { format = "%H:%M" }

# [[prompt.segments]]
# kind = "duration"
"##;

/// Whether the config text settles the crash-reports question: an explicit
/// `[crash_reports]` section, however it was written. The generated default
/// leaves it commented out so a fresh install is asked once. Text that
/// won't parse counts as answered: the error banner is enough for one launch.
pub fn crash_reports_answered(text: &str) -> bool {
    text.parse::<toml_edit::DocumentMut>()
        .map(|doc| doc.contains_table("crash_reports"))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_reports_question_is_settled_by_an_explicit_section() {
        assert!(!crash_reports_answered(""));
        assert!(!crash_reports_answered(&default_config_file()));
        assert!(crash_reports_answered("[crash_reports]\nenabled = false\n"));
        assert!(crash_reports_answered("[crash_reports]\nenabled = true\n"));
        // A file that won't parse: don't pile a question onto the error banner.
        assert!(crash_reports_answered("[crash_reports\n"));
    }

    #[test]
    fn generated_config_parses_and_speaks_this_platforms_keys() {
        let text = default_config_file();
        let parsed: Result<Config, _> = toml::from_str(&text);
        assert!(parsed.is_ok(), "{:?}", parsed.err());
        if cfg!(target_os = "macos") {
            assert!(text.contains("cmd-shift-p"));
        } else {
            assert!(
                !text.contains("cmd-"),
                "a cmd- key survived localisation:\n{}",
                text.lines()
                    .filter(|l| l.contains("cmd-"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            assert!(text.contains("\"ctrl-shift-p\" = \"app::palette\""));
            assert!(text.contains("the n in alt-n"));
        }
    }
}
