use std::collections::BTreeMap;

use serde::Deserialize;

/// Resolved configuration. Every field has a serde default so a three-line
/// config file works; the `Default` impls below are the merge-over-defaults.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub shell: ShellConfig,
    pub tree: TreeConfig,
    pub colors: ColorsConfig,
    pub prompt: PromptConfig,
    pub status_bar: StatusBarConfig,
    pub tabs: TabsConfig,
    pub bell: BellMode,
    pub copy_on_select: bool,
    pub keymap: KeymapConfig,
    pub notifications: NotificationsConfig,
    pub commands: CommandsConfig,
    pub editor: EditorConfig,
    pub cursor: CursorConfig,
    pub ssh: SshConfig,
    pub workspaces: WorkspacesConfig,
    pub markdown: MarkdownConfig,
    pub images: ImagesConfig,
    pub crash_reports: CrashReportsConfig,
}

/// Crash reports sent to the OmniPTY developer through Sentry: the stack
/// trace, version and OS. Never anything shown in or typed into a terminal.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CrashReportsConfig {
    /// Read at launch. Turning it off in a live reload stops a running copy;
    /// turning it back on waits for the next launch.
    pub enabled: bool,
}

impl Default for CrashReportsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Inline images: the pictures programs send with the kitty, iTerm2 and
/// sixel protocols.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesConfig {
    /// Draw them. Off, queries go unanswered and programs fall back to text.
    pub enabled: bool,
    /// Decoded image memory per pane, in MB. Past it, images whose cells
    /// have left the buffer go first, then the least recently drawn.
    pub memory_limit: usize,
    /// Where an image file opened from the tree, the finder or a
    /// cmd-click is shown.
    pub preview_in: OpenIn,
}

impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            memory_limit: 128,
            preview_in: OpenIn::Tab,
        }
    }
}

/// Where something that needs a pane of its own opens: a new tab, or a
/// split to the right of the focused pane.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OpenIn {
    #[default]
    Tab,
    Split,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MarkdownConfig {
    /// Where the rendered preview of a `.md` file opens.
    pub preview_in: OpenIn,
}

/// Pinned-workspace restore: whether saved startup commands run, and how
/// long to wait for a shell to be ready before giving up on its command.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspacesConfig {
    /// Run saved startup commands when restoring a pinned workspace (and
    /// when reopening a closed tab that had them). `--no-startup-commands`
    /// on the command line, or shift held at launch, overrides this for one
    /// launch.
    pub run_startup_commands: bool,
    /// How long to wait for a shell to show its first prompt before giving
    /// up on that pane's startup command rather than firing it into a void.
    pub startup_timeout: DurationText,
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            run_startup_commands: true,
            startup_timeout: DurationText(std::time::Duration::from_secs(5)),
        }
    }
}

/// The cursor's shape and blink. Programs that set their own shape with
/// DECSCUSR (`\e[<n> q`, as vim does per mode) override `style` until they
/// reset it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CursorConfig {
    pub style: CursorStyleName,
    pub blink: bool,
    /// Milliseconds per half-cycle.
    pub blink_interval: u64,
    /// How the cursor looks in a pane that isn't focused.
    pub unfocused: UnfocusedCursor,
    /// Bar width / underline height, as a fraction of a cell.
    pub thickness: f32,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            style: CursorStyleName::Block,
            blink: true,
            blink_interval: 530,
            unfocused: UnfocusedCursor::Hollow,
            thickness: 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CursorStyleName {
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UnfocusedCursor {
    Hollow,
    Solid,
    Hidden,
}

/// Per-host accents for panes whose foreground process is `ssh`, so a
/// production box is visibly different from a dev one.
///
/// ```toml
/// [[ssh.hosts]]
/// match  = "*.prod.example.com"
/// accent = "#f38ba8"
/// ```
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SshConfig {
    pub hosts: Vec<SshHost>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SshHost {
    /// A glob over the host as typed on the command line (after any
    /// `user@`): `*` and `?` wildcards, case-insensitive.
    #[serde(rename = "match")]
    pub pattern: String,
    pub accent: String,
}

impl SshConfig {
    /// The accent for `host`, from the first pattern that matches.
    pub fn accent_for(&self, host: &str) -> Option<&str> {
        self.hosts
            .iter()
            .find(|h| glob_match(&h.pattern, host))
            .map(|h| h.accent.as_str())
    }
}

/// `*` matches any run (including none), `?` one character; ASCII
/// case-insensitive, since hostnames are.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let t: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// How cmd-clicking a `path:line` opens the editor. Built-in mappings cover
/// vim/nvim, VS Code and friends, emacs, sublime, and helix; anything else
/// gets the file without a line, unless `open_at_line` says otherwise.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// A shell command with `{path}`, `{line}`, and `{col}` substituted,
    /// e.g. `"myeditor --line {line} {path}"`. Overrides the mapping.
    pub open_at_line: Option<String>,
    /// Where a file opens when the focused pane is busy — a terminal editor
    /// still showing the last file, say — and can't take another.
    pub open_in: OpenIn,
}

/// Desktop notifications when a long or failed command finishes in a pane
/// you aren't looking at, plus passthrough for programs that post their own
/// (OSC 9 / OSC 777).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub enabled: bool,
    /// Commands shorter than this never notify. `"30s"`, `"2m"`, `"1.5s"`,
    /// or a bare number of seconds.
    pub min_duration: DurationText,
    /// Only notify when the pane isn't focused or the window isn't active.
    pub only_when_unfocused: bool,
    /// A non-zero exit notifies regardless of duration (still subject to
    /// `only_when_unfocused`).
    pub on_failure_always: bool,
    /// Let programs post notifications with OSC 9 / OSC 777.
    pub passthrough_osc9: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration: DurationText(std::time::Duration::from_secs(30)),
            only_when_unfocused: true,
            on_failure_always: false,
            passthrough_osc9: true,
        }
    }
}

/// The per-pane command log built from the shell integration's OSC 133
/// markers: what feeds the status bar, tab indicators, history search, and
/// the failure gutter.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CommandsConfig {
    pub track: bool,
    /// Have the shell send each command line to OmniPTY, so history search
    /// and notifications can name the command. The text stays in memory
    /// only; nothing is written to disk.
    pub emit_cmdline: bool,
    pub max_entries: usize,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            track: true,
            emit_cmdline: true,
            max_entries: 500,
        }
    }
}

/// A duration written the way people write them: `"30s"`, `"2m"`, `"1h"`,
/// `"1.5s"`, `"250ms"`, or a bare number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationText(pub std::time::Duration);

impl DurationText {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let split = text
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let value: f64 = number
            .trim()
            .parse()
            .map_err(|_| format!("bad duration \"{text}\" — try \"30s\", \"2m\", or \"1.5s\""))?;
        if !value.is_finite() || value < 0.0 {
            return Err(format!("bad duration \"{text}\""));
        }
        let secs = match unit.trim() {
            "" | "s" | "sec" | "secs" => value,
            "ms" => value / 1000.0,
            "m" | "min" | "mins" => value * 60.0,
            "h" | "hr" | "hrs" => value * 3600.0,
            other => {
                return Err(format!(
                    "bad duration unit \"{other}\" in \"{text}\" — use ms, s, m, or h"
                ));
            }
        };
        Ok(Self(std::time::Duration::from_secs_f64(secs)))
    }
}

impl<'de> Deserialize<'de> for DurationText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Text(String),
            Number(f64),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Text(t) => DurationText::parse(&t).map_err(serde::de::Error::custom),
            Raw::Number(n) if n.is_finite() && n >= 0.0 => {
                Ok(DurationText(std::time::Duration::from_secs_f64(n)))
            }
            Raw::Number(_) => Err(serde::de::Error::custom(
                "duration must be a non-negative number of seconds",
            )),
        }
    }
}

/// User keybindings. Written flat — keystroke on the left, action id on the
/// right — with context-scoped subtables:
///
/// ```toml
/// [keymap]
/// "cmd-j" = "pane::split_down"
/// "cmd-d" = ""                   # unbind
/// [keymap.file_tree]
/// "y" = "tree::refresh"
/// ```
///
/// Top-level pairs bind at `Root`. Parsed by hand from the raw table because
/// serde's `flatten` and `deny_unknown_fields` can't be combined, and a typo'd
/// context name deserves an error rather than a silent no-op.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(try_from = "BTreeMap<String, toml::Value>")]
pub struct KeymapConfig {
    /// Start from an empty map instead of merging over the defaults.
    pub replace_defaults: bool,
    pub root: BTreeMap<String, String>,
    pub terminal: BTreeMap<String, String>,
    /// Copy mode: keys don't reach the shell here, so bare letters are fine.
    pub terminal_vi: BTreeMap<String, String>,
    pub file_tree: BTreeMap<String, String>,
    pub workspaces: BTreeMap<String, String>,
    pub overlay: BTreeMap<String, String>,
}

impl KeymapConfig {
    pub const CONTEXTS: &[&str] = &[
        "root",
        "terminal",
        "terminal_vi",
        "file_tree",
        "workspaces",
        "overlay",
    ];

    fn table_mut(&mut self, name: &str) -> Option<&mut BTreeMap<String, String>> {
        Some(match name {
            "root" => &mut self.root,
            "terminal" => &mut self.terminal,
            "terminal_vi" => &mut self.terminal_vi,
            "file_tree" => &mut self.file_tree,
            "workspaces" => &mut self.workspaces,
            "overlay" => &mut self.overlay,
            _ => return None,
        })
    }
}

impl TryFrom<BTreeMap<String, toml::Value>> for KeymapConfig {
    type Error = String;

    fn try_from(raw: BTreeMap<String, toml::Value>) -> Result<Self, String> {
        let mut out = KeymapConfig::default();
        for (key, value) in raw {
            match (key.as_str(), value) {
                ("replace_defaults", toml::Value::Boolean(b)) => out.replace_defaults = b,
                ("replace_defaults", _) => {
                    return Err("keymap.replace_defaults must be true or false".into());
                }
                (name, toml::Value::Table(table)) => {
                    let Some(target) = out.table_mut(name) else {
                        return Err(format!(
                            "unknown keymap context [keymap.{name}] — expected one of {}",
                            Self::CONTEXTS.join(", ")
                        ));
                    };
                    for (keys, action) in table {
                        match action {
                            toml::Value::String(action) => {
                                target.insert(keys, action);
                            }
                            _ => {
                                return Err(format!(
                                    "keymap.{name}.\"{keys}\" must be an action id string (use \"\" to unbind)"
                                ));
                            }
                        }
                    }
                }
                (keys, toml::Value::String(action)) => {
                    out.root.insert(keys.to_string(), action);
                }
                (keys, _) => {
                    return Err(format!(
                        "keymap.\"{keys}\" must be an action id string (use \"\" to unbind)"
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StatusBarConfig {
    pub enabled: bool,
    pub position: StatusBarPosition,
    /// What the current-tab chip shows. It matters most with the tab bar
    /// hidden, when the chip is the only thing saying which tab this is.
    pub tab: StatusBarTab,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: StatusBarPosition::Bottom,
            tab: StatusBarTab::Number,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusBarTab {
    Number,
    Name,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TabsConfig {
    /// Show the tab bar. `app::toggle_tab_bar` flips it for the session.
    pub enabled: bool,
    /// A small position number at the left of each tab — the `n` in `cmd-n`.
    pub show_numbers: bool,
    /// What closing a workspace's last tab does.
    pub close_last: CloseLastTab,
}

impl Default for TabsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_numbers: true,
            close_last: CloseLastTab::default(),
        }
    }
}

/// Closing a workspace's last tab either takes the workspace with it (and
/// the window with the last workspace), or leaves the workspace with a
/// fresh tab in the home directory.
#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseLastTab {
    #[default]
    Workspace,
    NewTab,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StatusBarPosition {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// One family, or a list: the first is the terminal font, the rest are
    /// fallbacks for glyphs it lacks (CJK, emoji), tried in order.
    pub family: FontFamily,
    pub size: f32,
    pub line_height: f32,
    pub weight: FontWeightName,
    pub ligatures: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: FontFamily::default(),
            size: 14.0,
            line_height: 1.25,
            weight: FontWeightName::Normal,
            ligatures: false,
        }
    }
}

/// `family = "X"` or `family = ["X", "Y", "Z"]`. Never empty: an empty list
/// falls back to the bundled font.
#[derive(Debug, Clone, PartialEq)]
pub struct FontFamily(Vec<String>);

impl FontFamily {
    pub const BUNDLED: &str = "JetBrainsMono Nerd Font Mono";

    pub fn new(families: Vec<String>) -> Self {
        let families: Vec<String> = families
            .into_iter()
            .filter(|f| !f.trim().is_empty())
            .collect();
        if families.is_empty() {
            Self(vec![Self::BUNDLED.to_string()])
        } else {
            Self(families)
        }
    }

    /// The font glyphs are shaped in.
    pub fn primary(&self) -> &str {
        &self.0[0]
    }

    /// Families consulted, in order, for glyphs the primary lacks.
    pub fn fallbacks(&self) -> &[String] {
        &self.0[1..]
    }
}

impl Default for FontFamily {
    fn default() -> Self {
        Self(vec![Self::BUNDLED.to_string()])
    }
}

impl<'de> Deserialize<'de> for FontFamily {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            Many(Vec<String>),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::One(s) => FontFamily::new(vec![s]),
            Raw::Many(v) => FontFamily::new(v),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FontWeightName {
    Normal,
    Medium,
    Bold,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct WindowConfig {
    pub padding: Padding,
    pub opacity: f32,
    pub blur: bool,
    pub titlebar: TitlebarMode,
    pub new_tab_directory: NewTabDirectory,
    /// Panes other than the focused one are dimmed to this (1.0 = no
    /// dimming). Only applies when the tab has more than one pane.
    pub inactive_pane_opacity: f32,
    /// The whole window is dimmed to this while another app is active.
    pub inactive_window_opacity: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding: Padding { x: 12.0, y: 8.0 },
            opacity: 1.0,
            blur: false,
            titlebar: TitlebarMode::Hidden,
            new_tab_directory: NewTabDirectory::Pwd,
            inactive_pane_opacity: 1.0,
            inactive_window_opacity: 1.0,
        }
    }
}

/// Where a new tab or window starts.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NewTabDirectory {
    /// Inherit the current tab's working directory.
    Pwd,
    /// Always start at ~/.
    Home,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Padding {
    pub x: f32,
    pub y: f32,
}

impl Default for Padding {
    fn default() -> Self {
        Self { x: 12.0, y: 8.0 }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitlebarMode {
    Native,
    Hidden,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ShellConfig {
    /// Default: $SHELL, then /bin/zsh.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub scrollback: usize,
    pub option_as_meta: OptionAsMeta,
    /// Install the shell-integration hooks (OSC 133 markers and the silent-cd
    /// widget). Independent of prompt styling — keep your own prompt and still
    /// get integration.
    pub integration: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            program: None,
            args: vec!["-l".into()],
            scrollback: 10_000,
            option_as_meta: OptionAsMeta::None,
            integration: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OptionAsMeta {
    None,
    Left,
    Right,
    Both,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TreeConfig {
    pub width: f32,
    pub show_hidden: bool,
    pub respect_gitignore: bool,
    pub indent: f32,
    pub icons: bool,
    pub follow_cwd: bool,
    /// Colour rows by git state (modified, added, untracked, deleted,
    /// conflicted), rolled up onto collapsed directories.
    pub git_status: bool,
    /// Whether a new window starts with the drawer showing. Startup only:
    /// `cmd-b` still toggles it, and a config reload leaves it alone.
    pub open_on_startup: bool,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            width: 280.0,
            show_hidden: false,
            respect_gitignore: true,
            indent: 16.0,
            icons: true,
            follow_cwd: true,
            git_status: true,
            open_on_startup: true,
        }
    }
}

/// A named preset supplies every color; explicit fields override individually.
///
/// With `follow_system = true`, `preset_dark` / `preset_light` are chosen by
/// the macOS appearance (each falling back to `preset`), and the explicit
/// overrides apply on top of whichever is active.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ColorsConfig {
    pub preset: Option<String>,
    pub preset_dark: Option<String>,
    pub preset_light: Option<String>,
    pub follow_system: bool,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub cursor: Option<String>,
    pub selection_bg: Option<String>,
    /// Text colour inside a selection; unset keeps each cell's own colour.
    pub selection_fg: Option<String>,
    pub black: Option<String>,
    pub red: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub blue: Option<String>,
    pub magenta: Option<String>,
    pub cyan: Option<String>,
    pub white: Option<String>,
    pub bright_black: Option<String>,
    pub bright_red: Option<String>,
    pub bright_green: Option<String>,
    pub bright_yellow: Option<String>,
    pub bright_blue: Option<String>,
    pub bright_magenta: Option<String>,
    pub bright_cyan: Option<String>,
    pub bright_white: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub enabled: bool,
    pub separator: String,
    pub end: String,
    pub newline_before_input: bool,
    pub segments: Vec<SegmentConfig>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            separator: "\u{e0b0}".into(), // powerline right arrow
            end: "\u{e0b0}".into(),
            newline_before_input: false,
            segments: vec![
                SegmentConfig {
                    kind: SegmentKind::Cwd,
                    fg: Some("#11111b".into()),
                    bg: Some("#89b4fa".into()),
                    bold: true,
                    options: SegmentOptions {
                        style: Some(CwdStyle::TruncateToRepo),
                        max_len: Some(40),
                        ..Default::default()
                    },
                },
                SegmentConfig {
                    kind: SegmentKind::Git,
                    fg: Some("#11111b".into()),
                    bg: Some("#a6e3a1".into()),
                    bold: false,
                    options: SegmentOptions {
                        show_dirty: Some(true),
                        dirty_bg: Some("#f9e2af".into()),
                        ahead_behind: Some(true),
                        ..Default::default()
                    },
                },
                SegmentConfig {
                    kind: SegmentKind::ExitStatus,
                    fg: Some("#11111b".into()),
                    bg: Some("#f38ba8".into()),
                    bold: false,
                    options: SegmentOptions {
                        hide_on_success: Some(true),
                        ..Default::default()
                    },
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SegmentConfig {
    pub kind: SegmentKind,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub options: SegmentOptions,
}

impl Default for SegmentConfig {
    fn default() -> Self {
        Self {
            kind: SegmentKind::Text,
            fg: None,
            bg: None,
            bold: false,
            options: SegmentOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    Cwd,
    Git,
    ExitStatus,
    Time,
    User,
    Host,
    Duration,
    Text,
    Env,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SegmentOptions {
    // cwd
    pub style: Option<CwdStyle>,
    pub max_len: Option<usize>,
    // git
    pub show_dirty: Option<bool>,
    pub dirty_bg: Option<String>,
    pub ahead_behind: Option<bool>,
    // exit_status
    pub hide_on_success: Option<bool>,
    // time
    pub format: Option<String>,
    // text
    pub text: Option<String>,
    // env
    pub var: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CwdStyle {
    Full,
    TruncateToRepo,
    Basename,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BellMode {
    #[default]
    None,
    Sound,
    Visual,
}

#[cfg(test)]
mod keymap_config_tests {
    use super::*;

    #[test]
    fn flat_pairs_bind_at_root_and_subtables_scope() {
        let text = r#"
[keymap]
"cmd-shift-p" = "app::palette"
"cmd-d" = ""

[keymap.file_tree]
"y" = "tree::refresh"

[keymap.terminal]
"cmd-shift-c" = "terminal::copy"
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.keymap.root["cmd-shift-p"], "app::palette");
        assert_eq!(config.keymap.root["cmd-d"], "");
        assert_eq!(config.keymap.file_tree["y"], "tree::refresh");
        assert_eq!(config.keymap.terminal["cmd-shift-c"], "terminal::copy");
        assert!(!config.keymap.replace_defaults);
    }

    #[test]
    fn explicit_root_table_and_replace_flag() {
        let text = r#"
[keymap]
replace_defaults = true
[keymap.root]
"cmd-j" = "tab::next"
"#;
        let config: Config = toml::from_str(text).unwrap();
        assert!(config.keymap.replace_defaults);
        assert_eq!(config.keymap.root["cmd-j"], "tab::next");
    }

    #[test]
    fn unknown_context_and_bad_values_are_errors() {
        let err =
            toml::from_str::<Config>("[keymap.filetree]\n\"y\" = \"tree::refresh\"\n").unwrap_err();
        assert!(err.to_string().contains("unknown keymap context"), "{err}");
        let err = toml::from_str::<Config>("[keymap]\n\"cmd-j\" = 3\n").unwrap_err();
        assert!(
            err.to_string().contains("must be an action id string"),
            "{err}"
        );
        let err = toml::from_str::<Config>("[keymap]\nreplace_defaults = \"yes\"\n").unwrap_err();
        assert!(err.to_string().contains("replace_defaults"), "{err}");
    }

    #[test]
    fn missing_keymap_is_empty() {
        let config: Config = toml::from_str(
            "[font]
size = 12
",
        )
        .unwrap();
        assert_eq!(config.keymap, KeymapConfig::default());
    }
}

#[cfg(test)]
mod font_and_glob_tests {
    use super::*;

    #[test]
    fn family_accepts_a_string_or_a_list() {
        let c: Config = toml::from_str("[font]\nfamily = \"Menlo\"\n").unwrap();
        assert_eq!(c.font.family.primary(), "Menlo");
        assert!(c.font.family.fallbacks().is_empty());
        let c: Config =
            toml::from_str("[font]\nfamily = [\"Menlo\", \"Apple Color Emoji\", \"\"]\n").unwrap();
        assert_eq!(c.font.family.primary(), "Menlo");
        assert_eq!(
            c.font.family.fallbacks(),
            &["Apple Color Emoji".to_string()]
        );
        // An empty list can't leave the terminal without a font.
        let c: Config = toml::from_str("[font]\nfamily = []\n").unwrap();
        assert_eq!(c.font.family.primary(), FontFamily::BUNDLED);
    }

    #[test]
    fn glob_matches_hosts() {
        assert!(glob_match("*.prod.example.com", "web-01.prod.example.com"));
        assert!(glob_match("*.PROD.example.com", "web-01.prod.Example.com"));
        assert!(!glob_match(
            "*.prod.example.com",
            "web-01.staging.example.com"
        ));
        assert!(glob_match("prod-??", "prod-01"));
        assert!(!glob_match("prod-??", "prod-001"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn ssh_hosts_and_cursor_parse() {
        let text = r##"
[cursor]
style = "bar"
blink = false
unfocused = "hidden"

[[ssh.hosts]]
match  = "*.prod.example.com"
accent = "#f38ba8"
[[ssh.hosts]]
match  = "*"
accent = "#89b4fa"
"##;
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.cursor.style, CursorStyleName::Bar);
        assert!(!c.cursor.blink);
        assert_eq!(c.cursor.unfocused, UnfocusedCursor::Hidden);
        assert_eq!(c.cursor.blink_interval, 530);
        assert_eq!(c.ssh.accent_for("db.prod.example.com"), Some("#f38ba8"));
        assert_eq!(c.ssh.accent_for("dev"), Some("#89b4fa"));
        assert!(
            toml::from_str::<Config>("[[ssh.hosts]]\naccent = \"#fff\"\n").is_err(),
            "match is required"
        );
        let c: Config =
            toml::from_str("[colors]\nfollow_system = true\npreset_light = \"catppuccin-latte\"\n")
                .unwrap();
        assert!(c.colors.follow_system);
        assert_eq!(c.colors.preset_light.as_deref(), Some("catppuccin-latte"));
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parses_common_spellings() {
        assert_eq!(
            DurationText::parse("30s").unwrap().0,
            Duration::from_secs(30)
        );
        assert_eq!(
            DurationText::parse("2m").unwrap().0,
            Duration::from_secs(120)
        );
        assert_eq!(
            DurationText::parse("1.5s").unwrap().0,
            Duration::from_millis(1500)
        );
        assert_eq!(
            DurationText::parse("250ms").unwrap().0,
            Duration::from_millis(250)
        );
        assert_eq!(
            DurationText::parse("1h").unwrap().0,
            Duration::from_secs(3600)
        );
        assert_eq!(
            DurationText::parse("45").unwrap().0,
            Duration::from_secs(45)
        );
        assert!(DurationText::parse("soon").is_err());
        assert!(DurationText::parse("3 fortnights").is_err());
        assert!(DurationText::parse("-1s").is_err());
    }

    #[test]
    fn config_accepts_strings_and_numbers() {
        let c: Config = toml::from_str("[notifications]\nmin_duration = \"2m\"\n").unwrap();
        assert_eq!(c.notifications.min_duration.0, Duration::from_secs(120));
        let c: Config = toml::from_str("[notifications]\nmin_duration = 10\n").unwrap();
        assert_eq!(c.notifications.min_duration.0, Duration::from_secs(10));
        assert!(toml::from_str::<Config>("[notifications]\nmin_duration = \"never\"\n").is_err());
        let c: Config =
            toml::from_str("[commands]\nemit_cmdline = false\nmax_entries = 50\n").unwrap();
        assert!(!c.commands.emit_cmdline);
        assert_eq!(c.commands.max_entries, 50);
        assert!(c.commands.track);
    }
}

#[cfg(test)]
mod tabs_config_tests {
    use super::*;

    #[test]
    fn tab_options_parse_and_default_to_visible_numbers() {
        let d = Config::default();
        assert!(d.tabs.enabled && d.tabs.show_numbers);
        assert_eq!(d.status_bar.tab, StatusBarTab::Number);

        let c: Config = toml::from_str(
            "[tabs]\nenabled = false\nshow_numbers = false\n[status_bar]\ntab = \"name\"\n",
        )
        .unwrap();
        assert!(!c.tabs.enabled && !c.tabs.show_numbers);
        assert_eq!(c.status_bar.tab, StatusBarTab::Name);
        assert!(toml::from_str::<Config>("[status_bar]\ntab = \"title\"\n").is_err());
    }

    #[test]
    fn open_in_defaults_to_a_tab() {
        let d = Config::default();
        assert_eq!(d.editor.open_in, OpenIn::Tab);
        assert_eq!(d.markdown.preview_in, OpenIn::Tab);
        assert_eq!(d.images.preview_in, OpenIn::Tab);
        let c: Config =
            toml::from_str("[editor]\nopen_in = \"split\"\n[markdown]\npreview_in = \"split\"\n[images]\npreview_in = \"split\"\n")
                .unwrap();
        assert_eq!(c.editor.open_in, OpenIn::Split);
        assert_eq!(c.markdown.preview_in, OpenIn::Split);
        assert_eq!(c.images.preview_in, OpenIn::Split);
        assert!(toml::from_str::<Config>("[editor]\nopen_in = \"window\"\n").is_err());
    }

    #[test]
    fn closing_the_last_tab_closes_the_workspace_by_default() {
        assert_eq!(Config::default().tabs.close_last, CloseLastTab::Workspace);
        let c: Config = toml::from_str("[tabs]\nclose_last = \"new_tab\"\n").unwrap();
        assert_eq!(c.tabs.close_last, CloseLastTab::NewTab);
        assert!(toml::from_str::<Config>("[tabs]\nclose_last = \"window\"\n").is_err());
    }
}
