pub mod click;
pub mod colors;
pub mod commands;
pub mod element;
pub mod event_loop;
pub mod graphics;
pub mod images;
pub mod keys;
pub mod osc;
pub mod placeholder;
pub mod process;
pub mod scan;
pub mod session;
pub mod sixel;

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as AlacEvent;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle, Rgb};
use futures::StreamExt;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, Bounds, ClipboardItem, Context, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollDelta, ScrollWheelEvent, ShapedLine, Styled,
    Window, div,
};

use crate::config::schema::{BellMode, CursorStyleName};
use crate::config::{Config, Theme, theme::hsla_to_rgb8};
use crate::keymap::actions::{
    ClearScrollback, Copy, CopyLastBlock, CopyLastCommand, CopyLastOutput, CopyMode, Paste,
    PromptDown, PromptUp, Search, SearchToggleCase, SearchToggleRegex, SearchToggleWord, SelectAll,
};
use crate::prompt::integration::{Channel, write_channel};
use crate::startup::{OnExit, RestartDecision, RestartGate, StartupCommand};
pub use click::ClickTarget;
pub use commands::{Command, CommandLog};
use element::TerminalElement;
use graphics::GraphicsEvent;
use images::ImageStore;
pub use osc::{Marker, MarkerKind};
use placeholder::{PLACEHOLDER, strip_placeholders};
pub use process::ForegroundProcess;
use session::{SessionEvent, SessionOptions, TermSize, TerminalSession, resolve_shell};

#[cfg(target_os = "macos")]
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    fn NSBeep();
}

/// The modifier that turns a click into "open this": cmd on macOS, ctrl on
/// Linux — Super+click is the compositor's window-move on every major Linux
/// desktop, so it never reaches the app. Mouse handlers test this directly,
/// outside the keymap.
pub fn open_modifier(m: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        m.platform
    } else {
        m.control
    }
}

/// The system alert sound. Linux has no single "beep" API (XDG sound themes,
/// ALSA, PipeWire…), so it shells out to a sound tool; `bell = "sound"`
/// degrades to the visual flash there if none is installed.
fn system_beep() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { NSBeep() };
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        linux_beep()
    }
}

/// Linux has no single beep API: uses `canberra-gtk-play` (libcanberra),
/// falling back to `paplay` + freedesktop-sound-theme. Returns `false` if
/// unavailable; playback is backgrounded and serialized to drop BEL floods.
#[cfg(not(target_os = "macos"))]
fn linux_beep() -> bool {
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};

    const FALLBACK: &str = "/usr/share/sounds/freedesktop/stereo/bell.oga";
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    let canberra = on_path("canberra-gtk-play");
    let paplay = on_path("paplay") && Path::new(FALLBACK).is_file();
    if !canberra && !paplay {
        return false;
    }
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return true; // one is already playing; don't pile up
    }

    let spawned = std::thread::Builder::new()
        .name("omnipty-bell".into())
        .spawn(move || {
            let run = |cmd: &mut Command| {
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
            };
            let played = canberra
                && matches!(
                    run(Command::new("canberra-gtk-play").args(["-i", "bell"])),
                    Ok(s) if s.success()
                );
            if !played && paplay {
                let _ = run(Command::new("paplay").arg(FALLBACK));
            }
            IN_FLIGHT.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        IN_FLIGHT.store(false, Ordering::Release);
        return false;
    }
    true
}

/// Is `bin` a file in some `$PATH` directory?
#[cfg(not(target_os = "macos"))]
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

pub enum TerminalEvent {
    /// The shell exited, with its status. The owning window decides whether
    /// to close the pane.
    Exited(Option<i32>),
    TitleChanged,
    CwdChanged(PathBuf),
    /// Output arrived (coalesced with the repaint tick) — for the "unread"
    /// dot on background tabs.
    Output,
    /// The shell integration reported a command starting.
    CommandStarted,
    /// ...and finishing. `label` is the command's first line or a stand-in.
    CommandFinished {
        label: String,
        exit: Option<i32>,
        duration: Duration,
    },
    /// A program asked for a desktop notification (OSC 9 / OSC 777), already
    /// rate-limited and gated on the config.
    Notify {
        title: Option<String>,
        body: String,
    },
    /// cmd-click on a `path:line:col` that resolved to a file.
    OpenPath {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// cmd-click on a directory: point the tree there.
    RevealDir(PathBuf),
    /// Bytes the user typed or pasted while broadcast is on, for the owner
    /// to fan out to the tab's other panes. This pane has already written
    /// them to its own shell.
    Input(Vec<u8>),
    /// Something to say in the window banner ("copy mode needs the primary
    /// screen").
    Notice(String),
    /// The foreground process changed (a program started or ended, ssh
    /// connected or dropped).
    ForegroundChanged,
    /// The pane's startup command exited cleanly and its `on_exit` is
    /// `close`: the owner closes the pane with the usual semantics.
    StartupExited,
}

/// Where a pane's startup command is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupPhase {
    /// Nothing queued (no command, or it already ran).
    Idle,
    /// Waiting for the shell to be ready.
    Pending,
    /// Sent to the shell; waiting for its C marker.
    AwaitingStart,
    /// Running; waiting for its D marker.
    Running,
    /// Exited under `on_exit = restart`; the backoff timer is ticking.
    RestartWait,
    /// Ran (or was given up on). Nothing more happens automatically.
    Done,
}

/// Copy mode's sub-mode, for the indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViModeKind {
    Normal,
    Visual,
    VisualLine,
    VisualBlock,
}

impl ViModeKind {
    pub fn label(self) -> &'static str {
        match self {
            ViModeKind::Normal => "COPY",
            ViModeKind::Visual => "VISUAL",
            ViModeKind::VisualLine => "V-LINE",
            ViModeKind::VisualBlock => "V-BLOCK",
        }
    }
}

/// Copy-mode input state: a pending count (`5j`) and a pending first key
/// of a two-key sequence (`gg`, `yy`).
#[derive(Default)]
struct ViState {
    count: Option<usize>,
    pending: Option<char>,
}

/// The search bar's toggles. Kept on the pane so they survive closing and
/// reopening the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

impl SearchOptions {
    /// The pattern handed to the regex engine. Inline flags rather than the
    /// engine's smart-case default, so the toggle means what it says. The
    /// word boundary is the ASCII one: alacritty's lazy DFA can't do the
    /// Unicode-aware `\b`.
    pub fn pattern(&self, query: &str) -> String {
        let body = if self.regex {
            query.to_string()
        } else {
            regex_escape(query)
        };
        let body = if self.whole_word {
            format!("(?-u:\\b)(?:{body})(?-u:\\b)")
        } else {
            body
        };
        let flag = if self.case_sensitive { "(?-i)" } else { "(?i)" };
        format!("{flag}{body}")
    }
}

/// A cmd-hover underline: which screen row and columns to draw it under.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HoverSpan {
    pub row: usize,
    pub start: usize,
    pub end: usize,
}

/// How long a "does this path exist" answer is trusted. Hover resolution
/// runs on every mouse move with cmd held; stat on a network volume is slow.
const EXISTS_TTL: Duration = Duration::from_secs(2);

/// Geometry of the last painted frame, for mouse -> grid math.
#[derive(Clone, Copy)]
pub struct LastLayout {
    pub bounds: Bounds<Pixels>,
    pub cell_width: f32,
    pub cell_height: f32,
    pub display_offset: usize,
}

pub struct TerminalPane {
    pub session: Option<TerminalSession>,
    pub size: TermSize,
    pub title: String,
    pub cwd: Option<PathBuf>,
    pub child_exited: Option<Option<i32>>,
    focus_handle: FocusHandle,
    config: Rc<Config>,
    theme: Rc<Theme>,
    pub font_delta: f32,
    initial_dir: PathBuf,

    repaint_scheduled: bool,
    pub shape_cache: HashMap<u64, ShapedLine>,
    pub prev_shape_cache: HashMap<u64, ShapedLine>,
    pub last_layout: Option<LastLayout>,
    /// The pictures behind this pane's placeholder cells.
    pub images: ImageStore,
    last_cwd_poll: Instant,
    cwd_poll_scheduled: bool,

    pub blink_show: bool,
    last_input: Instant,
    selecting: bool,
    /// The cell last reported to a program tracking the mouse, while the
    /// button it was told about is still down: its drag, not ours.
    reporting: Option<(usize, usize)>,
    /// Where the press that began the selection landed.
    select_from_y: f32,
    /// Where a selection drag is being held, while that is at or past the
    /// pane's top or bottom edge: the view scrolls towards it until it
    /// comes away.
    autoscroll: Option<gpui::Point<Pixels>>,
    autoscrolling: bool,
    scroll_accum: f32,
    bell_until: Option<Instant>,
    search: Option<SearchState>,
    /// Prompt rows in absolute line coordinates (history_size + line). Stable
    /// until the scrollback cap rotates lines out; see prompt_up/down. Fed
    /// by OSC 133;A when the shell integration is live, else by the Enter
    /// heuristic in `on_key_down`.
    prompt_marks: Vec<usize>,
    /// What has run in this pane, from the OSC 133 markers.
    pub log: CommandLog,
    /// Where the last 133;B landed, so a command line can be read back off
    /// the grid when the shell didn't send it.
    last_input_start: Option<(usize, usize)>,
    /// OSC 7 is live for this session, so the cwd poll can stand down.
    osc7_seen: bool,
    /// Rate limit for program-posted notifications.
    last_program_notify: Option<Instant>,
    /// The file tree's root, for resolving relative paths in output.
    pub tree_root: Option<PathBuf>,
    /// Code blocks of the markdown preview this pane is paging; the "copy"
    /// links in it name them by index. Empty in every other pane.
    pub preview_code: Vec<String>,
    /// Re-renders the preview this pane is paging to a column count,
    /// rewriting the paged file; returns its code blocks. Called when the
    /// pane's width changes so the preview follows a resize.
    pub preview_render: Option<Box<dyn Fn(usize) -> Option<Vec<String>>>>,
    /// Repo root for the current cwd, looked up on the background pool.
    git_root: Option<PathBuf>,
    git_root_for: Option<PathBuf>,
    exists_cache: HashMap<PathBuf, (bool, Instant)>,
    /// The token under a cmd-hover, underlined to show it's clickable.
    pub hover: Option<HoverSpan>,
    /// The URL that hover would open, shown in the pane's corner so an
    /// OSC 8 link can't pass off one address as another.
    hover_url: Option<String>,
    /// Keystrokes and pastes are echoed to the tab's other panes. Set by
    /// the owner; the pane only reports its input.
    pub broadcast: bool,
    /// What's running on the PTY, refreshed on output.
    pub foreground: Option<ForegroundProcess>,
    last_foreground_poll: Instant,
    foreground_poll_scheduled: bool,
    /// Copy mode, when active.
    vi: Option<ViState>,
    /// The last accepted `/` or `?` search, for `n` / `N`.
    last_search: Option<(String, Direction)>,
    pub search_options: SearchOptions,
    /// What this pane runs when its workspace is restored. Saved with a
    /// pinned workspace; set from the terminal or the workspaces panel.
    pub startup: Option<StartupCommand>,
    startup_phase: StartupPhase,
    /// Bumped whenever the startup state machine restarts, so a timer from
    /// an earlier arm/restart can't act on a later one.
    startup_generation: usize,
    /// Set once the fallback (no markers) ready-detection has been kicked
    /// off, so the first Wakeup is the only one that schedules it.
    startup_wakeup_seen: bool,
    /// The C marker that just arrived is the startup command's: if the
    /// shell didn't send its text (`commands.emit_cmdline = false`), the
    /// log entry gets it from us rather than from the grid.
    startup_label_pending: bool,
    restart_gate: RestartGate,
    /// `-e` from the command line: this program (and its arguments) runs
    /// in place of the shell, with none of the shell integration, and a
    /// restart re-runs it.
    command: Option<Vec<String>>,
}

struct SearchState {
    query: String,
    current: Option<Match>,
    /// The query isn't a valid regex (only possible with the regex toggle).
    invalid: bool,
    /// Opened from copy mode with `/` (forward) or `?` (backward): enter
    /// moves the vi cursor to the match instead of walking older ones.
    vi_direction: Option<Direction>,
}

/// Escape a literal string for use as a regex pattern.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl EventEmitter<TerminalEvent> for TerminalPane {}

/// One cell past `p` in `direction`, wrapping at line ends and clamped to
/// the grid — the origin for "the next match after this one".
fn step_point<T>(
    term: &alacritty_terminal::term::Term<T>,
    p: GridPoint,
    direction: Direction,
) -> GridPoint {
    let last_column = term.last_column();
    let stepped = if direction == Direction::Left {
        if p.column.0 > 0 {
            GridPoint::new(p.line, Column(p.column.0 - 1))
        } else {
            GridPoint::new(p.line - 1, last_column)
        }
    } else if p.column < last_column {
        GridPoint::new(p.line, Column(p.column.0 + 1))
    } else {
        GridPoint::new(p.line + 1, Column(0))
    };
    stepped.grid_clamp(term, alacritty_terminal::index::Boundary::Grid)
}

/// `2340` → `2,340`.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Focusable for TerminalPane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalPane {
    /// `command` (`omnipty -e ...`) runs instead of the shell; `None` gives
    /// the ordinary shell pane.
    pub fn new(
        config: Rc<Config>,
        theme: Rc<Theme>,
        working_dir: PathBuf,
        command: Option<Vec<String>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let log = CommandLog::new(config.commands.max_entries.max(1));
        let mut this = Self {
            session: None,
            // Plausible bring-up size; real measurement happens on first layout.
            size: TermSize {
                columns: 80,
                screen_lines: 24,
                cell_width: 8.0,
                cell_height: 17.0,
                scale: 1.0,
            },
            title: "omnipty".into(),
            cwd: Some(working_dir.clone()),
            child_exited: None,
            focus_handle: cx.focus_handle(),
            config,
            theme,
            font_delta: 0.0,
            initial_dir: working_dir,
            repaint_scheduled: false,
            shape_cache: HashMap::new(),
            prev_shape_cache: HashMap::new(),
            last_layout: None,
            images: ImageStore::default(),
            last_cwd_poll: Instant::now(),
            cwd_poll_scheduled: false,
            blink_show: true,
            last_input: Instant::now(),
            selecting: false,
            reporting: None,
            select_from_y: 0.0,
            autoscroll: None,
            autoscrolling: false,
            scroll_accum: 0.0,
            bell_until: None,
            search: None,
            prompt_marks: Vec::new(),
            log,
            last_input_start: None,
            osc7_seen: false,
            last_program_notify: None,
            tree_root: None,
            preview_code: Vec::new(),
            preview_render: None,
            git_root: None,
            git_root_for: None,
            exists_cache: HashMap::new(),
            hover: None,
            hover_url: None,
            broadcast: false,
            foreground: None,
            last_foreground_poll: Instant::now(),
            foreground_poll_scheduled: false,
            vi: None,
            last_search: None,
            search_options: SearchOptions::default(),
            startup: None,
            startup_phase: StartupPhase::Idle,
            startup_generation: 0,
            startup_wakeup_seen: false,
            startup_label_pending: false,
            restart_gate: RestartGate::default(),
            command,
        };
        this.spawn_session(cx);
        this.refresh_git_root(cx);
        this.spawn_blink_task(cx);
        // A closed pane's images would otherwise stay in the GPU atlas.
        cx.on_release(|pane, cx| {
            for image in pane.images.clear() {
                cx.drop_image(image, None);
            }
        })
        .detach();
        this
    }

    pub fn set_config(&mut self, config: Rc<Config>, theme: Rc<Theme>, cx: &mut Context<Self>) {
        let term_changed = config.cursor != self.config.cursor
            || config.shell.scrollback != self.config.shell.scrollback;
        let images_changed = config.images != self.config.images;
        self.config = config;
        self.theme = theme;
        self.shape_cache.clear();
        self.prev_shape_cache.clear();
        if term_changed && let Some(session) = &self.session {
            session.set_term_options(self.term_config());
        }
        if images_changed {
            if let Some(session) = &self.session {
                session.set_images_enabled(self.config.images.enabled);
            }
            self.trim_images(false, cx);
        }
        cx.notify();
    }

    /// Alacritty's terminal options from the config: scrollback and the
    /// cursor the shell starts with. DECSCUSR from a program overrides the
    /// shape until it resets it; copy mode always shows a steady block.
    fn term_config(&self) -> TermConfig {
        let cursor = &self.config.cursor;
        let shape = match cursor.style {
            CursorStyleName::Block => CursorShape::Block,
            CursorStyleName::Bar => CursorShape::Beam,
            CursorStyleName::Underline => CursorShape::Underline,
        };
        TermConfig {
            scrolling_history: self.config.shell.scrollback,
            default_cursor_style: CursorStyle {
                shape,
                blinking: cursor.blink,
            },
            vi_mode_cursor_style: Some(CursorStyle {
                shape: CursorShape::Block,
                blinking: false,
            }),
            ..TermConfig::default()
        }
    }

    pub fn config(&self) -> &Rc<Config> {
        &self.config
    }

    pub fn theme(&self) -> &Rc<Theme> {
        &self.theme
    }

    pub fn adjust_font(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        match delta {
            Some(d) => self.font_delta = (self.font_delta + d).clamp(-8.0, 24.0),
            None => self.font_delta = 0.0,
        }
        self.shape_cache.clear();
        self.prev_shape_cache.clear();
        cx.notify();
    }

    fn spawn_session(&mut self, cx: &mut Context<Self>) {
        let shell = self.config.shell.clone();
        let cwd = self.cwd.clone().unwrap_or_else(|| self.initial_dir.clone());
        let options = match &self.command {
            // A program of the user's choosing, not a shell: no dotfile
            // shims or prompt hooks, just the terminal environment.
            Some(command) => SessionOptions {
                program: command[0].clone(),
                args: command[1..].to_vec(),
                working_directory: Some(cwd),
                scrollback: shell.scrollback,
                env: HashMap::new(),
                images: self.config.images.enabled,
            },
            None => {
                let program = resolve_shell(shell.program.as_deref());
                // Regenerated per session so a config reload applies to restarts.
                let integration = crate::prompt::integration::setup(&self.config, &program);
                SessionOptions {
                    program,
                    args: integration.args_override.unwrap_or(shell.args),
                    working_directory: Some(cwd),
                    scrollback: shell.scrollback,
                    env: integration.env,
                    images: self.config.images.enabled,
                }
            }
        };
        let shell_name = std::path::Path::new(&options.program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_owned();
        match TerminalSession::spawn(options, self.size) {
            Ok((session, mut rx)) => {
                if self.command.is_none() {
                    // Shell-integration bugs tend to belong to one shell.
                    crate::telemetry::tag("shell", &shell_name);
                }
                session.set_term_options(self.term_config());
                self.session = Some(session);
                self.child_exited = None;
                self.vi = None;
                self.foreground = None;
                cx.spawn(async move |this, cx| {
                    while let Some(event) = rx.next().await {
                        let mut batch = vec![event];
                        while batch.len() < 1024 {
                            match rx.try_recv() {
                                Ok(ev) => batch.push(ev),
                                Err(_) => break,
                            }
                        }
                        let alive = this
                            .update(cx, |pane, cx| {
                                for event in batch {
                                    match event {
                                        SessionEvent::Term(event) => {
                                            pane.handle_alac_event(event, cx)
                                        }
                                        SessionEvent::Marker(marker) => {
                                            pane.handle_marker(marker, cx)
                                        }
                                        SessionEvent::Graphics(event) => {
                                            pane.handle_graphics(event, cx)
                                        }
                                    }
                                }
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                    }
                })
                .detach();
            }
            Err(e) => {
                // NotFound is a `shell.program` typo, and the pane title already
                // says so. `-e` commands are the user's own programs.
                if self.command.is_none() && e.kind() != std::io::ErrorKind::NotFound {
                    crate::telemetry::report(
                        "shell failed to start",
                        format_args!("{shell_name}: {e}"),
                    );
                }
                self.title = format!("failed to spawn shell: {e}");
                self.child_exited = Some(None);
            }
        }
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.session = None;
        // The new shell starts from an empty grid; so does the store.
        let stale = self.images.clear();
        self.release_images(stale, cx);
        self.spawn_session(cx);
        cx.notify();
    }

    /// An image arrived, was placed, or was deleted. Always ahead of the
    /// `Wakeup` that repaints the cells involved.
    fn handle_graphics(&mut self, event: GraphicsEvent, cx: &mut Context<Self>) {
        match event {
            GraphicsEvent::Image {
                id,
                size,
                data,
                reservation,
            } => {
                let (generation, replaced) = self.images.begin(id, size);
                self.release_images(replaced, cx);
                // Never on this thread: a large photo takes a while, and
                // typing must not wait for it. Its cells draw blank until
                // it lands.
                let decode = cx.background_executor().spawn(async move {
                    // A crash in the decoder then says what it was fed:
                    // enough to build a test image, and nothing more.
                    let (kind, bytes) = match &data {
                        graphics::ImageData::Encoded { bytes, .. } => ("encoded", bytes.len()),
                        graphics::ImageData::Raw { bytes, .. } => ("raw", bytes.len()),
                    };
                    crate::telemetry::breadcrumb(
                        "image",
                        format!("decoding {bytes} {kind} bytes, {}x{}", size.0, size.1),
                    );
                    let image = images::decode(data, size);
                    // Decoded: its share of the PTY thread's budget is free.
                    drop(reservation);
                    image
                });
                cx.spawn(async move |this, cx| {
                    let image = decode.await;
                    this.update(cx, |pane, cx| {
                        pane.images.finish(id, generation, image);
                        pane.trim_images(false, cx);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            GraphicsEvent::Place {
                image,
                placement,
                spec,
            } => self.images.place(image, placement, spec),
            GraphicsEvent::Delete { image, placement } => self.images.delete(image, placement),
            GraphicsEvent::Free { image } => {
                let freed = self.images.free(image);
                self.release_images(freed, cx);
            }
        }
    }

    /// Take images that left the store out of the GPU atlas too, which
    /// otherwise only grows. Deferred to the end of the update, when no
    /// window is checked out and `drop_image` reaches all of them; the
    /// repaint keeps a frame that still drew one from being shown again.
    fn release_images(
        &mut self,
        images: impl IntoIterator<Item = std::sync::Arc<gpui::RenderImage>>,
        cx: &mut Context<Self>,
    ) {
        let images: Vec<_> = images.into_iter().collect();
        if images.is_empty() {
            return;
        }
        cx.notify();
        cx.defer(move |cx| {
            for image in images {
                cx.drop_image(image, None);
            }
        });
    }

    /// Hold decoded images to `images.memory_limit`. Nothing reports a
    /// picture whose cells scrolled out of history or were cleared, so
    /// when over the limit (or when `sweep` says the grid just lost a lot)
    /// the grid is read for the ids still in it; whatever else must go is
    /// the least recently drawn.
    fn trim_images(&mut self, sweep: bool, cx: &mut Context<Self>) {
        let limit = self.config.images.memory_limit.saturating_mul(1 << 20);
        if self.images.is_empty() || !(sweep || self.images.bytes() > limit) {
            return;
        }
        let live = self
            .session
            .as_ref()
            .and_then(|session| images::live_ids(&session.term.lock()));
        let (ids, pixels) = self.images.evict(limit, live.as_ref());
        self.release_images(pixels, cx);
        if !ids.is_empty()
            && let Some(session) = &self.session
        {
            session.forget_images(ids);
        }
    }

    fn handle_marker(&mut self, marker: Marker, cx: &mut Context<Self>) {
        let now = Instant::now();
        match &marker.kind {
            MarkerKind::Cwd(path) => {
                // A remote shell's OSC 7 names a directory on its machine,
                // and `file:///…` with no host would pass as local. Nothing
                // here should follow it: not the tree, not the git poll.
                if self.ssh_host().is_some() {
                    return;
                }
                self.osc7_seen = true;
                self.log.cwd = Some(path.clone());
                if self.cwd.as_ref() != Some(path) {
                    self.cwd = Some(path.clone());
                    cx.emit(TerminalEvent::CwdChanged(path.clone()));
                    self.refresh_git_root(cx);
                }
                return;
            }
            MarkerKind::Notify { title, body } => {
                // A remote host can spam these; one every few seconds is
                // plenty for anything legitimate.
                let recent = self
                    .last_program_notify
                    .is_some_and(|t| now.duration_since(t) < Duration::from_secs(3));
                if self.config.notifications.enabled
                    && self.config.notifications.passthrough_osc9
                    && !recent
                {
                    self.last_program_notify = Some(now);
                    cx.emit(TerminalEvent::Notify {
                        title: title.clone(),
                        body: body.clone(),
                    });
                }
                return;
            }
            MarkerKind::PromptStart if !marker.alt_screen => self.push_prompt_mark(marker.row),
            MarkerKind::InputStart if !marker.alt_screen => {
                self.last_input_start = Some((marker.row, marker.column));
            }
            _ => {}
        }
        self.track_startup_marker(&marker, cx);
        if !self.config.commands.track {
            // Still note that integration is live, so cmd-↑ trusts A markers.
            self.log.markers_seen = true;
            return;
        }
        match self.log.on_marker(&marker, now) {
            Some(commands::LogEvent::Started(id)) => {
                let known = std::mem::take(&mut self.startup_label_pending)
                    .then(|| self.startup.as_ref().map(|s| s.command.clone()))
                    .flatten();
                if self.log.get(id).is_some_and(|c| c.text.is_none())
                    && let Some(text) = known.or_else(|| self.read_command_line(marker.row))
                {
                    self.log.set_text(id, text);
                }
                self.last_input_start = None;
                cx.emit(TerminalEvent::CommandStarted);
                cx.notify();
            }
            Some(commands::LogEvent::Finished(id)) => {
                if let Some(cmd) = self.log.get(id) {
                    cx.emit(TerminalEvent::CommandFinished {
                        label: cmd.label().to_string(),
                        exit: cmd.exit,
                        duration: cmd.duration(),
                    });
                }
                cx.notify();
            }
            None => {}
        }
    }

    fn push_prompt_mark(&mut self, row: usize) {
        if self.prompt_marks.last() != Some(&row) {
            self.prompt_marks.push(row);
            if self.prompt_marks.len() > 500 {
                self.prompt_marks.remove(0);
            }
        }
    }

    /// The text between the last 133;B and the row before `command_row`:
    /// what the user typed, as the grid shows it. The fallback for shells
    /// that don't send `cmdline=`.
    fn read_command_line(&self, command_row: usize) -> Option<String> {
        let (row, column) = self.last_input_start?;
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let history = term.grid().history_size() as i32;
        let start = Line(row as i32 - history);
        let end = Line(command_row.checked_sub(1)? as i32 - history);
        if end < start || start < term.topmost_line() || end > term.bottommost_line() {
            return None;
        }
        let text = term.bounds_to_string(
            GridPoint::new(start, Column(column)),
            GridPoint::new(end, term.last_column()),
        );
        let text = strip_placeholders(&text).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// A command's output as text, if its rows are still in the buffer.
    pub fn output_text(&self, cmd: &Command) -> Option<String> {
        let rows = cmd.output_rows.clone()?;
        if rows.is_empty() {
            return None;
        }
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let history = term.grid().history_size() as i32;
        let start = Line(rows.start as i32 - history).max(term.topmost_line());
        let end = Line(rows.end as i32 - 1 - history).min(term.bottommost_line());
        if end < start {
            return None;
        }
        let text = term.bounds_to_string(
            GridPoint::new(start, Column(0)),
            GridPoint::new(end, term.last_column()),
        );
        let text = strip_placeholders(&text).trim_end().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Scroll so absolute `row` sits at the top of the viewport.
    pub fn scroll_to_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        let history = term.grid().history_size();
        let current = term.grid().display_offset() as i32;
        let target = history.saturating_sub(row).min(history) as i32;
        term.scroll_display(Scroll::Delta(target - current));
        drop(term);
        cx.notify();
    }

    fn copy_last_output(
        &mut self,
        _: &CopyLastOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cmd) = self.log.last_finished().cloned() else {
            return;
        };
        if let Some(text) = self.output_text(&cmd) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_last_command(
        &mut self,
        _: &CopyLastCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cmd) = self.log.last_finished().cloned() else {
            return;
        };
        if let Some(text) = cmd.text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn copy_last_block(&mut self, _: &CopyLastBlock, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(cmd) = self.log.last_finished().cloned() else {
            return;
        };
        let mut block = String::new();
        if let Some(text) = &cmd.text {
            block.push_str("$ ");
            block.push_str(text);
            block.push('\n');
        }
        if let Some(output) = self.output_text(&cmd) {
            block.push_str(&output);
            block.push('\n');
        }
        if !block.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(block));
        }
    }

    fn handle_alac_event(&mut self, event: AlacEvent, cx: &mut Context<Self>) {
        match event {
            AlacEvent::Wakeup => {
                self.schedule_repaint(cx);
                // With OSC 7 live, the shell tells us; polling would only
                // race it.
                if !self.osc7_seen {
                    self.poll_cwd(cx);
                }
                self.poll_foreground(cx);
                self.startup_on_first_output(cx);
            }
            AlacEvent::Title(title) => {
                self.title = title;
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            AlacEvent::ResetTitle => {
                self.title = "omnipty".into();
                cx.emit(TerminalEvent::TitleChanged);
                cx.notify();
            }
            AlacEvent::PtyWrite(text) => {
                // Device Attributes and cursor-position query responses; if
                // these are dropped, querying programs hang forever.
                if let Some(session) = &self.session {
                    session.write_input(self.device_attributes(text).into_bytes());
                }
            }
            AlacEvent::ClipboardStore(_, text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            AlacEvent::ClipboardLoad(_, formatter) => {
                if let Some(session) = &self.session {
                    let text = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    session.write_input(formatter(&text).into_bytes());
                }
            }
            AlacEvent::ColorRequest(index, formatter) => {
                if let Some(session) = &self.session {
                    let color = match index {
                        0..=255 => colors::resolve_indexed(index as u8, &self.theme),
                        256 => self.theme.foreground,
                        257 => self.theme.background,
                        258 => self.theme.cursor,
                        _ => self.theme.foreground,
                    };
                    let (r, g, b) = hsla_to_rgb8(color);
                    session.write_input(formatter(Rgb { r, g, b }).into_bytes());
                }
            }
            // `CSI 14 t` is answered on the PTY thread (`Reply::TextAreaSize`),
            // in the same device pixels as the cell size it's read against.
            AlacEvent::TextAreaSizeRequest(_) => {}
            AlacEvent::Bell => match self.config.bell {
                BellMode::None => {}
                BellMode::Sound if system_beep() => {}
                BellMode::Sound | BellMode::Visual => {
                    self.bell_until = Some(Instant::now() + Duration::from_millis(150));
                    let timer = cx.background_executor().timer(Duration::from_millis(160));
                    cx.spawn(async move |this, cx| {
                        timer.await;
                        this.update(cx, |pane, cx| {
                            pane.bell_until = None;
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                    cx.notify();
                }
            },
            AlacEvent::CursorBlinkingChange => {
                self.blink_show = true;
                cx.notify();
            }
            AlacEvent::MouseCursorDirty => {}
            AlacEvent::ChildExit(status) => {
                let code = status.code();
                self.child_exited = Some(code);
                cx.emit(TerminalEvent::Exited(code));
                cx.notify();
            }
            AlacEvent::Exit => {
                self.session = None;
                cx.notify();
            }
        }
    }

    /// alacritty answers DA1 as a VT102 (`?6c`). With images on, the answer
    /// is a VT220 with sixel (4) and ANSI colour (22): that `4` is how sixel
    /// programs, and tmux, learn they can draw here. Anything else alacritty
    /// writes passes through untouched.
    fn device_attributes(&self, reply: String) -> String {
        if reply == "\x1b[?6c" && self.config.images.enabled {
            "\x1b[?62;4;22c".into()
        } else {
            reply
        }
    }

    /// Coalesce repaints: however fast Wakeups arrive, we repaint at most once
    /// per ~8ms tick.
    fn schedule_repaint(&mut self, cx: &mut Context<Self>) {
        if self.repaint_scheduled {
            return;
        }
        self.repaint_scheduled = true;
        let timer = cx.background_executor().timer(Duration::from_millis(8));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |pane, cx| {
                pane.repaint_scheduled = false;
                cx.emit(TerminalEvent::Output);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn spawn_blink_task(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let timer = match this.update(cx, |pane, cx| {
                    let interval = pane.config.cursor.blink_interval.clamp(100, 5000);
                    cx.background_executor()
                        .timer(Duration::from_millis(interval))
                }) {
                    Ok(timer) => timer,
                    Err(_) => break,
                };
                timer.await;
                let alive = this
                    .update(cx, |pane, cx| {
                        let interval = Duration::from_millis(
                            pane.config.cursor.blink_interval.clamp(100, 5000),
                        );
                        // A cursor that blinks mid-typing is distracting.
                        if pane.last_input.elapsed() < interval {
                            if !pane.blink_show {
                                pane.blink_show = true;
                                cx.notify();
                            }
                        } else {
                            pane.blink_show = !pane.blink_show;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    const CWD_POLL_INTERVAL: Duration = Duration::from_millis(150);

    /// Throttled with a trailing edge: a burst of output always gets one more
    /// poll after it settles, otherwise the cd that produced the final prompt
    /// redraw is missed and the tree lags until the next keystroke.
    fn poll_cwd(&mut self, cx: &mut Context<Self>) {
        let elapsed = self.last_cwd_poll.elapsed();
        if elapsed < Self::CWD_POLL_INTERVAL {
            if !self.cwd_poll_scheduled {
                self.cwd_poll_scheduled = true;
                let timer = cx
                    .background_executor()
                    .timer(Self::CWD_POLL_INTERVAL.saturating_sub(elapsed));
                cx.spawn(async move |this, cx| {
                    timer.await;
                    this.update(cx, |pane, cx| {
                        pane.cwd_poll_scheduled = false;
                        pane.poll_cwd_now(cx);
                    })
                    .ok();
                })
                .detach();
            }
            return;
        }
        self.poll_cwd_now(cx);
    }

    fn poll_cwd_now(&mut self, cx: &mut Context<Self>) {
        self.last_cwd_poll = Instant::now();
        if let Some(session) = &self.session
            && let Some(cwd) = session.foreground_cwd()
            && self.cwd.as_ref() != Some(&cwd)
        {
            self.cwd = Some(cwd.clone());
            cx.emit(TerminalEvent::CwdChanged(cwd));
            self.refresh_git_root(cx);
        }
    }

    const FOREGROUND_POLL_INTERVAL: Duration = Duration::from_millis(400);

    /// Who's in the foreground, throttled with a trailing edge like the cwd
    /// poll: a program that prints once as it starts is still caught.
    fn poll_foreground(&mut self, cx: &mut Context<Self>) {
        let elapsed = self.last_foreground_poll.elapsed();
        if elapsed < Self::FOREGROUND_POLL_INTERVAL {
            if !self.foreground_poll_scheduled {
                self.foreground_poll_scheduled = true;
                let timer = cx
                    .background_executor()
                    .timer(Self::FOREGROUND_POLL_INTERVAL.saturating_sub(elapsed));
                cx.spawn(async move |this, cx| {
                    timer.await;
                    this.update(cx, |pane, cx| {
                        pane.foreground_poll_scheduled = false;
                        pane.poll_foreground_now(cx);
                    })
                    .ok();
                })
                .detach();
            }
            return;
        }
        self.poll_foreground_now(cx);
    }

    fn poll_foreground_now(&mut self, cx: &mut Context<Self>) {
        self.last_foreground_poll = Instant::now();
        let Some(session) = &self.session else { return };
        let current = session.foreground_process();
        if current != self.foreground {
            self.foreground = current;
            cx.emit(TerminalEvent::ForegroundChanged);
            cx.notify();
        }
    }

    /// The host this pane is ssh'd into, when its foreground process is ssh.
    pub fn ssh_host(&self) -> Option<&str> {
        self.foreground.as_ref().and_then(|f| f.ssh_host.as_deref())
    }

    /// Something other than the shell has the terminal (an editor, a build),
    /// so a command sent here wouldn't reach a prompt. Asked of the PTY
    /// directly: the polled `foreground` can be a moment behind.
    pub fn is_busy(&self) -> bool {
        self.log.is_running()
            || self
                .session
                .as_ref()
                .and_then(|s| s.foreground_process())
                .is_some_and(|f| !f.is_shell())
    }

    // --- Copy mode (vi scrollback navigation) ---

    /// Which copy sub-mode is active, if any.
    pub fn vi_mode(&self) -> Option<ViModeKind> {
        self.vi.as_ref()?;
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let kind = match term
            .selection
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|s| s.ty)
        {
            None => ViModeKind::Normal,
            Some(SelectionType::Lines) => ViModeKind::VisualLine,
            Some(SelectionType::Block) => ViModeKind::VisualBlock,
            Some(SelectionType::Simple | SelectionType::Semantic) => ViModeKind::Visual,
        };
        Some(kind)
    }

    /// The pending count, for the indicator.
    pub fn vi_count(&self) -> Option<usize> {
        self.vi.as_ref().and_then(|v| v.count)
    }

    fn copy_mode(&mut self, _: &CopyMode, _window: &mut Window, cx: &mut Context<Self>) {
        if self.vi.is_some() {
            self.exit_copy_mode(cx);
        } else if let Err(reason) = self.enter_copy_mode(cx) {
            cx.emit(TerminalEvent::Notice(reason.to_string()));
        }
    }

    /// Enter copy mode. Refused while a program holds the alternate
    /// screen — there's no scrollback there, and vim already has a vi mode.
    pub fn enter_copy_mode(&mut self, cx: &mut Context<Self>) -> Result<(), &'static str> {
        if self.child_exited.is_some() {
            return Err("the shell has exited — press ⏎ to restart it first");
        }
        let Some(session) = &self.session else {
            return Err("no session");
        };
        let mut term = session.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return Err("copy mode isn't available while a full-screen program has the terminal");
        }
        if !term.mode().contains(TermMode::VI) {
            term.toggle_vi_mode();
        }
        term.selection = None;
        drop(term);
        self.search = None;
        self.vi = Some(ViState::default());
        cx.notify();
        Ok(())
    }

    /// Leave copy mode: drop the selection and snap back to the live
    /// screen, the way tmux does.
    pub fn exit_copy_mode(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            if term.mode().contains(TermMode::VI) {
                term.toggle_vi_mode();
            }
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        self.vi = None;
        self.search = None;
        cx.notify();
    }

    /// The selected text, without the placeholder cells of any image the
    /// selection runs across.
    fn selection_text(&self) -> Option<String> {
        let text = self.session.as_ref()?.term.lock().selection_to_string()?;
        Some(strip_placeholders(&text).into_owned())
    }

    /// Copy the selection (if any) to the clipboard. Returns whether
    /// anything was copied.
    fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let text = self.selection_text();
        match text {
            Some(text) if !text.is_empty() => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                true
            }
            _ => false,
        }
    }

    /// Start (or, for the same type, end) a visual selection at the vi cursor.
    fn vi_toggle_selection(&mut self, ty: SelectionType) {
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        if term
            .selection
            .as_ref()
            .is_some_and(|s| s.ty == ty && !s.is_empty())
        {
            term.selection = None;
            return;
        }
        let point = term.vi_mode_cursor.point;
        let mut selection = Selection::new(ty, point, Side::Left);
        selection.include_all();
        term.selection = Some(selection);
    }

    /// The keystroke vocabulary of copy mode. Nothing here reaches the
    /// shell. Modelled on vim's normal/visual modes and tmux's copy mode.
    fn handle_vi_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let m = ks.modifiers;
        if m.platform || m.function {
            return; // cmd-keys are the app's; anything unbound is ignored
        }
        let Some(session) = self.session.as_ref().map(|s| s.term.clone()) else {
            return;
        };
        let (count, pending) = {
            let vi = self.vi.as_mut().expect("in copy mode");
            (vi.count.take(), vi.pending.take())
        };
        let n = count.unwrap_or(1).max(1);
        let motion = |term: &mut alacritty_terminal::term::Term<session::EventProxy>,
                      motion: ViMotion| {
            for _ in 0..n {
                term.vi_motion(motion);
            }
        };

        if m.control {
            let mut term = session.lock();
            let half = (term.screen_lines() / 2).max(1) as i32;
            let full = term.screen_lines() as i32;
            match ks.key.as_str() {
                "d" => {
                    term.scroll_display(Scroll::Delta(-half));
                    for _ in 0..half {
                        term.vi_motion(ViMotion::Down);
                    }
                }
                "u" => {
                    term.scroll_display(Scroll::Delta(half));
                    for _ in 0..half {
                        term.vi_motion(ViMotion::Up);
                    }
                }
                "f" => {
                    term.scroll_display(Scroll::Delta(-full));
                    for _ in 0..full {
                        term.vi_motion(ViMotion::Down);
                    }
                }
                "b" => {
                    term.scroll_display(Scroll::Delta(full));
                    for _ in 0..full {
                        term.vi_motion(ViMotion::Up);
                    }
                }
                "v" => {
                    drop(term);
                    self.vi_toggle_selection(SelectionType::Block);
                }
                "c" | "[" => {
                    drop(term);
                    self.exit_copy_mode(cx);
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        // Named keys first; everything else goes by the typed character so
        // shifted punctuation (`$`, `^`, `{`) arrives as itself.
        let key = match ks.key.as_str() {
            "escape" => Some('\x1b'),
            "enter" => Some('\r'),
            "up" => Some('k'),
            "down" => Some('j'),
            "left" => Some('h'),
            "right" => Some('l'),
            "home" => Some('0'),
            "end" => Some('$'),
            "pageup" => Some('\u{1}'),
            "pagedown" => Some('\u{2}'),
            "space" => Some(' '),
            "backspace" => Some('h'),
            _ => ks.key_char.as_deref().and_then(|c| {
                let mut it = c.chars();
                let ch = it.next()?;
                it.next().is_none().then_some(ch)
            }),
        };
        let Some(key) = key else { return };

        // Counts: digits accumulate, except a leading 0 which is a motion.
        if key.is_ascii_digit() && (key != '0' || count.is_some()) {
            let digit = key.to_digit(10).unwrap() as usize;
            let next = count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(99_999);
            if let Some(vi) = self.vi.as_mut() {
                vi.count = Some(next);
                vi.pending = pending;
            }
            cx.notify();
            return;
        }

        let mut term = session.lock();
        match (pending, key) {
            (Some('g'), 'g') => {
                let top = GridPoint::new(term.topmost_line(), Column(0));
                term.vi_goto_point(top);
            }
            (Some('y'), 'y') => {
                // yy: yank the cursor's line and leave.
                let point = term.vi_mode_cursor.point;
                let mut selection = Selection::new(SelectionType::Lines, point, Side::Left);
                selection.include_all();
                term.selection = Some(selection);
                drop(term);
                self.copy_selection(cx);
                self.exit_copy_mode(cx);
                return;
            }
            (Some(_), _) => {
                // An unfinished sequence followed by something else: drop
                // it and handle the key on its own.
                drop(term);
                if let Some(vi) = self.vi.as_mut() {
                    vi.count = count;
                }
                return self.handle_vi_key(event, cx);
            }
            (None, 'h') => motion(&mut term, ViMotion::Left),
            (None, 'j') => motion(&mut term, ViMotion::Down),
            (None, 'k') => motion(&mut term, ViMotion::Up),
            (None, 'l') => motion(&mut term, ViMotion::Right),
            (None, ' ') => motion(&mut term, ViMotion::Right),
            (None, 'w') => motion(&mut term, ViMotion::SemanticRight),
            (None, 'b') => motion(&mut term, ViMotion::SemanticLeft),
            (None, 'e') => motion(&mut term, ViMotion::SemanticRightEnd),
            (None, 'W') => motion(&mut term, ViMotion::WordRight),
            (None, 'B') => motion(&mut term, ViMotion::WordLeft),
            (None, 'E') => motion(&mut term, ViMotion::WordRightEnd),
            (None, '0') => term.vi_motion(ViMotion::First),
            (None, '^') => term.vi_motion(ViMotion::FirstOccupied),
            (None, '$') => term.vi_motion(ViMotion::Last),
            (None, 'H') => term.vi_motion(ViMotion::High),
            (None, 'M') => term.vi_motion(ViMotion::Middle),
            (None, 'L') => term.vi_motion(ViMotion::Low),
            (None, '%') => term.vi_motion(ViMotion::Bracket),
            (None, '{') => motion(&mut term, ViMotion::ParagraphUp),
            (None, '}') => motion(&mut term, ViMotion::ParagraphDown),
            (None, 'G') => {
                let bottom = GridPoint::new(term.bottommost_line(), Column(0));
                term.vi_goto_point(bottom);
            }
            (None, '\u{1}') => {
                let full = term.screen_lines() as i32;
                term.scroll_display(Scroll::Delta(full));
            }
            (None, '\u{2}') => {
                let full = term.screen_lines() as i32;
                term.scroll_display(Scroll::Delta(-full));
            }
            (None, 'g' | 'y')
                if key == 'g' || term.selection.as_ref().is_none_or(|s| s.is_empty()) =>
            {
                if let Some(vi) = self.vi.as_mut() {
                    vi.pending = Some(key);
                    vi.count = count;
                }
            }
            (None, 'y') | (None, '\r') => {
                drop(term);
                if self.copy_selection(cx) {
                    self.exit_copy_mode(cx);
                }
                return;
            }
            (None, 'v') => {
                drop(term);
                self.vi_toggle_selection(SelectionType::Simple);
            }
            (None, 'V') => {
                drop(term);
                self.vi_toggle_selection(SelectionType::Lines);
            }
            (None, '/') | (None, '?') => {
                drop(term);
                let direction = if key == '/' {
                    Direction::Right
                } else {
                    Direction::Left
                };
                self.search = Some(SearchState {
                    query: String::new(),
                    current: None,
                    invalid: false,
                    vi_direction: Some(direction),
                });
            }
            (None, 'n') | (None, 'N') => {
                drop(term);
                self.vi_search_again(key == 'N', cx);
            }
            (None, '\x1b') => {
                let had_selection = term.selection.take().is_some();
                drop(term);
                if !had_selection {
                    self.exit_copy_mode(cx);
                    return;
                }
            }
            (None, 'q') | (None, 'i') | (None, 'a') => {
                drop(term);
                self.exit_copy_mode(cx);
                return;
            }
            _ => {}
        }
        cx.notify();
    }

    /// `n` / `N`: repeat the last accepted copy-mode search from the vi
    /// cursor, in the same (or the opposite) direction.
    fn vi_search_again(&mut self, reverse: bool, cx: &mut Context<Self>) {
        let Some((pattern, direction)) = self.last_search.clone() else {
            return;
        };
        let direction = if reverse {
            direction.opposite()
        } else {
            direction
        };
        let Some(session) = &self.session else { return };
        let Ok(mut regex) = RegexSearch::new(&pattern) else {
            return;
        };
        let mut term = session.term.lock();
        let origin = step_point(&*term, term.vi_mode_cursor.point, direction);
        if let Some(m) = term.search_next(&mut regex, origin, direction, Side::Left, None) {
            let start = *m.start();
            term.vi_goto_point(start);
        }
        drop(term);
        cx.notify();
    }

    /// Bytes to the shell, bypassing the local key handling: how broadcast
    /// input from a sibling pane arrives.
    pub fn write_raw(&mut self, bytes: Vec<u8>) {
        if let Some(session) = &self.session {
            session.write_input(bytes);
            let mut term = session.term.lock();
            if term.grid().display_offset() != 0 && !term.mode().contains(TermMode::VI) {
                term.scroll_display(Scroll::Bottom);
            }
        }
    }

    /// Look up the repo root for the cwd, off the main thread, so
    /// repo-relative paths in output resolve even from a subdirectory.
    fn refresh_git_root(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.cwd.clone() else { return };
        if self.git_root_for.as_ref() == Some(&cwd) {
            return;
        }
        self.git_root_for = Some(cwd.clone());
        let bg = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let lookup = cwd.clone();
            let root = bg.spawn(async move { crate::git::toplevel(&lookup) }).await;
            this.update(cx, |pane, _| {
                if pane.git_root_for.as_ref() == Some(&cwd) {
                    pane.git_root = root;
                }
            })
            .ok();
        })
        .detach();
    }

    fn path_exists(&mut self, path: &std::path::Path) -> bool {
        let now = Instant::now();
        if let Some((exists, at)) = self.exists_cache.get(path)
            && now.duration_since(*at) < EXISTS_TTL
        {
            return *exists;
        }
        if self.exists_cache.len() > 512 {
            self.exists_cache.clear();
        }
        let exists = path.exists();
        self.exists_cache.insert(path.to_path_buf(), (exists, now));
        exists
    }

    /// What a cmd-click at `point` would open, and the token's span.
    fn target_at(&mut self, point: GridPoint) -> Option<(ClickTarget, click::Token)> {
        let session = self.session.as_ref()?;
        let term = session.term.lock();
        let grid = term.grid();
        // Explicit hyperlinks (OSC 8) win over textual detection. The target
        // is hidden behind arbitrary text, so only schemes that open a
        // browser or mail client are honoured: `file:///x.command` or an
        // app's custom scheme would run something on a click.
        if let Some(link) = grid[point.line][point.column].hyperlink() {
            let uri = link.uri();
            if !["http://", "https://", "mailto:"]
                .iter()
                .any(|s| uri.len() > s.len() && uri[..s.len()].eq_ignore_ascii_case(s))
            {
                return None;
            }
            // Underline the whole run of cells sharing the link.
            let line = &grid[point.line];
            let linked = |col: usize| line[Column(col)].hyperlink().as_ref() == Some(&link);
            let start = (0..point.column.0)
                .rev()
                .take_while(|&c| linked(c))
                .last()
                .unwrap_or(point.column.0);
            let end = (point.column.0..self.size.columns)
                .take_while(|&c| linked(c))
                .last()
                .unwrap_or(point.column.0);
            return Some((
                ClickTarget::Url(uri.to_string()),
                click::Token {
                    start,
                    end: end + 1,
                    text: uri.to_string(),
                },
            ));
        }
        let cols = self.size.columns;
        // An image beside a path is a gap, not part of the token.
        let chars: Vec<char> = (0..cols)
            .map(|c| match grid[point.line][Column(c)].c {
                PLACEHOLDER => ' ',
                c => c,
            })
            .collect();
        drop(term);
        let token = click::token_at(&chars, point.column.0)?;
        let home = crate::app::home_dir();
        let cwd = self.cwd.clone();
        let git_root = self.git_root.clone();
        let tree_root = self.tree_root.clone();
        let roots = click::Roots {
            cwd: cwd.as_deref(),
            git_root: git_root.as_deref(),
            tree_root: tree_root.as_deref(),
            home: home.as_deref(),
        };
        let target = click::classify(&token.text, &roots, &mut |p| self.path_exists(p))?;
        Some((target, token))
    }

    /// Type a path at the prompt, quoted. Relative to the cwd when it lies
    /// beneath it (and `absolute` is false), which is what a command wants.
    /// A name with control bytes is refused rather than typed (see
    /// `click::typeable`).
    pub fn insert_path(&mut self, path: &std::path::Path, absolute: bool, cx: &mut Context<Self>) {
        if !click::typeable(&path.to_string_lossy()) {
            cx.emit(TerminalEvent::Notice(
                "not inserted: the name contains control characters".into(),
            ));
            return;
        }
        let text = match (&self.cwd, absolute) {
            (Some(cwd), false) => match path.strip_prefix(cwd) {
                Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().to_string(),
                Ok(_) => ".".to_string(),
                Err(_) => path.to_string_lossy().to_string(),
            },
            _ => path.to_string_lossy().to_string(),
        };
        self.write_command(&format!("{} ", click::shell_quote(&text)));
    }

    /// Change the shell's directory. With shell integration installed this
    /// hands the path off through a file and triggers a zle/readline widget,
    /// so no `cd` command is echoed; otherwise it falls back to typing one.
    pub fn request_cd(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        use std::os::unix::ffi::OsStrExt as _;
        let Some(session) = &self.session else { return };
        if self.config.shell.integration
            && write_channel(Channel::Cd, session.id(), path.as_os_str().as_bytes())
        {
            // zsh's zle widget redraws the prompt in place. bash's readline
            // cannot, so submit an empty line to get a fresh correct prompt.
            let mut bytes = b"\x1b[9001~".to_vec();
            let program = resolve_shell(self.config.shell.program.as_deref());
            let is_bash = std::path::Path::new(&program)
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("bash"));
            if is_bash {
                bytes.push(b'\r');
            }
            session.write_input(bytes);
            session.term.lock().scroll_display(Scroll::Bottom);
            return;
        }
        let text = path.to_string_lossy();
        if !click::typeable(&text) {
            cx.emit(TerminalEvent::Notice(
                "can't cd there: the name contains control characters".into(),
            ));
            return;
        }
        self.write_command(&format!("cd {}\r", click::shell_quote(&text)));
    }

    /// Run a command in the shell without typing it at the prompt. With shell
    /// integration installed the command goes through a file and a widget, so
    /// the user sees only whatever the command itself draws — no echoed line,
    /// nothing in history. Otherwise it falls back to typing it visibly, which
    /// is at least honest about what just ran.
    ///
    /// `command` carries no trailing newline; the fallback path adds one.
    pub fn run_command(&mut self, command: &str) {
        let Some(session) = &self.session else { return };
        if self.config.shell.integration
            && self.shell_supports_widgets()
            && write_channel(Channel::Run, session.id(), command.as_bytes())
        {
            session.write_input(b"\x1b[9002~".to_vec());
            session.term.lock().scroll_display(Scroll::Bottom);
            return;
        }
        self.write_command(&format!("{command}\r"));
    }

    /// Widgets are only installed for the shells OmniPTY can inject into.
    fn shell_supports_widgets(&self) -> bool {
        let program = resolve_shell(self.config.shell.program.as_deref());
        let name = crate::terminal::session::shell_name(&program);
        name.starts_with("zsh") || name.starts_with("bash")
    }

    pub fn write_command(&mut self, command: &str) {
        if let Some(session) = &self.session {
            session.write_input(command.as_bytes().to_vec());
            session.term.lock().scroll_display(Scroll::Bottom);
        }
    }

    // --- Workspace startup commands ---

    /// Whether the shell will say when it's ready (OSC 133 A) and how a
    /// command it ran ended (133 D). Only the shells OmniPTY injects into.
    fn startup_uses_markers(&self) -> bool {
        self.config.shell.integration && self.shell_supports_widgets()
    }

    pub fn set_startup(&mut self, startup: Option<StartupCommand>, cx: &mut Context<Self>) {
        if self.startup != startup {
            self.startup = startup;
            cx.notify();
        }
    }

    /// Queue the startup command to run once the shell is ready. Called
    /// right after the pane is created on restore. With shell integration
    /// the first prompt marker is the signal; without it, a short delay
    /// after the first output. If neither arrives within `timeout` the
    /// command is dropped rather than fired into a void.
    pub fn arm_startup(&mut self, timeout: Duration, cx: &mut Context<Self>) {
        if self.startup.is_none() {
            return;
        }
        self.startup_phase = StartupPhase::Pending;
        self.startup_wakeup_seen = false;
        self.startup_generation += 1;
        let generation = self.startup_generation;
        let timer = cx.background_executor().timer(timeout);
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |pane, cx| {
                if pane.startup_generation == generation
                    && pane.startup_phase == StartupPhase::Pending
                {
                    pane.startup_phase = StartupPhase::Done;
                    cx.emit(TerminalEvent::Notice(format!(
                        "startup command skipped — the shell wasn't ready after {}",
                        commands::format_duration(timeout)
                    )));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Drop a queued startup command before it fires (shift held at launch,
    /// noticed only once the window has keyboard focus). Returns whether
    /// one was pending.
    pub fn cancel_startup(&mut self, cx: &mut Context<Self>) -> bool {
        if self.startup_phase != StartupPhase::Pending {
            return false;
        }
        self.startup_phase = StartupPhase::Done;
        self.startup_generation += 1;
        cx.notify();
        true
    }

    /// No markers to say the prompt is up: treat the first output as "the
    /// rc files are running" and fire shortly after. Racy by nature, which
    /// is why the marker path is preferred.
    fn startup_on_first_output(&mut self, cx: &mut Context<Self>) {
        if self.startup_phase != StartupPhase::Pending
            || self.startup_uses_markers()
            || self.startup_wakeup_seen
        {
            return;
        }
        self.startup_wakeup_seen = true;
        let generation = self.startup_generation;
        let timer = cx.background_executor().timer(Duration::from_millis(500));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |pane, cx| {
                if pane.startup_generation == generation {
                    pane.on_shell_ready(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_shell_ready(&mut self, cx: &mut Context<Self>) {
        if self.startup_phase == StartupPhase::Pending {
            self.fire_startup(cx);
        }
    }

    fn fire_startup(&mut self, cx: &mut Context<Self>) {
        let Some(startup) = self.startup.clone() else {
            self.startup_phase = StartupPhase::Idle;
            return;
        };
        // Without markers there is no way to see the command end, so
        // `on_exit` can't apply; the run is simply done.
        self.startup_phase = if self.startup_uses_markers() {
            StartupPhase::AwaitingStart
        } else {
            StartupPhase::Done
        };
        self.restart_gate.record_start(Instant::now());
        self.run_command(&startup.command);
        cx.notify();
    }

    /// Follow the startup command through the shell's markers: C says it
    /// started, D says it ended and how. Independent of the command log so
    /// `commands.track = false` doesn't disable `on_exit`.
    fn track_startup_marker(&mut self, marker: &Marker, cx: &mut Context<Self>) {
        match (&marker.kind, self.startup_phase) {
            (MarkerKind::PromptStart, StartupPhase::Pending) => self.on_shell_ready(cx),
            (MarkerKind::CommandStart { .. }, StartupPhase::AwaitingStart) => {
                self.startup_phase = StartupPhase::Running;
                self.startup_label_pending = true;
            }
            (MarkerKind::CommandEnd { exit }, StartupPhase::Running) => {
                let exit = *exit;
                self.startup_finished(exit, cx);
            }
            _ => {}
        }
    }

    fn startup_finished(&mut self, exit: Option<i32>, cx: &mut Context<Self>) {
        self.startup_phase = StartupPhase::Done;
        let Some(startup) = self.startup.clone() else {
            return;
        };
        match startup.on_exit {
            OnExit::Shell => {}
            OnExit::Close => {
                // Same rule as `exit` in a shell: a clean exit closes, a
                // failure stays on screen so it can be read.
                if exit == Some(0) {
                    cx.emit(TerminalEvent::StartupExited);
                } else {
                    let status = exit
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "unknown".into());
                    cx.emit(TerminalEvent::Notice(format!(
                        "startup command exited with status {status} — pane kept open"
                    )));
                }
            }
            OnExit::Restart => match self.restart_gate.record_exit(Instant::now()) {
                RestartDecision::After(delay) => {
                    self.startup_phase = StartupPhase::RestartWait;
                    self.startup_generation += 1;
                    let generation = self.startup_generation;
                    let timer = cx.background_executor().timer(delay);
                    cx.spawn(async move |this, cx| {
                        timer.await;
                        this.update(cx, |pane, cx| {
                            if pane.startup_generation == generation
                                && pane.startup_phase == StartupPhase::RestartWait
                                && pane.child_exited.is_none()
                            {
                                pane.fire_startup(cx);
                            }
                        })
                        .ok();
                    })
                    .detach();
                }
                RestartDecision::Stop => {
                    cx.emit(TerminalEvent::Notice(crate::startup::breaker_message(
                        self.restart_gate.exits_in_window(),
                    )));
                }
            },
        }
        cx.notify();
    }

    /// The restore-time affordance: what's about to run in this pane.
    fn startup_chip(&self) -> Option<String> {
        let startup = self.startup.as_ref()?;
        match self.startup_phase {
            StartupPhase::Pending => Some(format!("▸ {}", startup.command)),
            StartupPhase::RestartWait => Some(format!("↻ {}", startup.command)),
            _ => None,
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.handle_search_key(event, cx);
            cx.stop_propagation();
            return;
        }
        if self.vi.is_some() {
            self.handle_vi_key(event, cx);
            cx.stop_propagation();
            return;
        }
        if self.child_exited.is_some() {
            if event.keystroke.key == "enter" {
                self.restart(cx);
            }
            return;
        }
        let Some(session) = &self.session else { return };
        let mode = *session.term.lock().mode();
        let option_as_meta = self.config.shell.option_as_meta;
        if let Some(bytes) = keys::to_bytes(&event.keystroke, &mode, option_as_meta) {
            if self.broadcast {
                cx.emit(TerminalEvent::Input(bytes.clone()));
            }
            session.write_input(bytes);
            let mut term = session.term.lock();
            // Typing snaps you out of scrollback.
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
            }
            // Record a prompt mark: the row where Enter was pressed is (about
            // to be) a completed prompt line — the anchor cmd-up jumps to.
            // Once the shell integration's A markers are flowing they are
            // the better source, and this heuristic stands down.
            if event.keystroke.key == "enter"
                && !event.keystroke.modifiers.modified()
                && !term.mode().contains(TermMode::ALT_SCREEN)
                && !self.log.markers_seen
            {
                let abs = term.grid().history_size()
                    + term.renderable_content().cursor.point.line.0.max(0) as usize;
                drop(term);
                self.push_prompt_mark(abs);
            } else {
                drop(term);
            }
            self.last_input = Instant::now();
            self.blink_show = true;
            cx.stop_propagation();
            cx.notify();
        }
    }

    // --- Scrollback search ---

    fn toggle_search(&mut self, _: &Search, _window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            self.close_search(cx);
        } else {
            self.search = Some(SearchState {
                query: String::new(),
                current: None,
                invalid: false,
                vi_direction: None,
            });
        }
        cx.notify();
    }

    fn close_search(&mut self, cx: &mut Context<Self>) {
        self.search = None;
        if let Some(session) = &self.session {
            session.term.lock().selection = None;
        }
        cx.notify();
    }

    fn search_toggle_regex(
        &mut self,
        _: &SearchToggleRegex,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.regex = !self.search_options.regex;
        self.rerun_search(cx);
    }

    fn search_toggle_case(
        &mut self,
        _: &SearchToggleCase,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.case_sensitive = !self.search_options.case_sensitive;
        self.rerun_search(cx);
    }

    fn search_toggle_word(
        &mut self,
        _: &SearchToggleWord,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_options.whole_word = !self.search_options.whole_word;
        self.rerun_search(cx);
    }

    /// Flip a toggle by clicking its chip in the search bar.
    pub fn toggle_search_option(&mut self, which: u8, cx: &mut Context<Self>) {
        match which {
            0 => self.search_options.regex = !self.search_options.regex,
            1 => self.search_options.case_sensitive = !self.search_options.case_sensitive,
            _ => self.search_options.whole_word = !self.search_options.whole_word,
        }
        self.rerun_search(cx);
    }

    /// A toggle changed: search again from scratch with the same query.
    fn rerun_search(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = &mut self.search {
            state.current = None;
            let direction = state.vi_direction.unwrap_or(Direction::Left);
            self.run_search(direction, false, cx);
        }
        cx.notify();
    }

    fn handle_search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let vi_direction = self.search.as_ref().and_then(|s| s.vi_direction);
        match ks.key.as_str() {
            "escape" => {
                self.close_search(cx);
                return;
            }
            "enter" if vi_direction.is_some() => {
                // From copy mode: accept — the vi cursor lands on the match
                // and n / N take it from there.
                self.accept_vi_search(cx);
                return;
            }
            "enter" => {
                // enter walks older matches; shift-enter walks newer.
                let direction = if ks.modifiers.shift {
                    Direction::Right
                } else {
                    Direction::Left
                };
                self.run_search(direction, true, cx);
                return;
            }
            "backspace" => {
                if let Some(state) = &mut self.search {
                    if state.query.pop().is_none() {
                        self.close_search(cx);
                        return;
                    }
                    state.current = None;
                }
                self.run_search(vi_direction.unwrap_or(Direction::Left), false, cx);
                return;
            }
            _ => {}
        }
        if ks.modifiers.platform || ks.modifiers.control {
            return;
        }
        if let Some(key_char) = ks.key_char.clone()
            && let Some(state) = &mut self.search
        {
            state.query.push_str(&key_char);
            state.current = None;
            self.run_search(vi_direction.unwrap_or(Direction::Left), false, cx);
        }
    }

    /// Enter in a copy-mode search: park the vi cursor on the current match
    /// and close the bar, remembering the pattern for `n` / `N`.
    fn accept_vi_search(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.search.take() else {
            return;
        };
        let Some(direction) = state.vi_direction else {
            return;
        };
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            term.selection = None;
            if let Some(m) = &state.current {
                let start = *m.start();
                term.vi_goto_point(start);
            }
        }
        if !state.query.is_empty() && !state.invalid {
            self.last_search = Some((self.search_options.pattern(&state.query), direction));
        }
        cx.notify();
    }

    fn run_search(&mut self, direction: Direction, from_current: bool, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let Some(state) = &mut self.search else {
            return;
        };
        if state.query.is_empty() {
            state.current = None;
            state.invalid = false;
            session.term.lock().selection = None;
            cx.notify();
            return;
        }
        let pattern = self.search_options.pattern(&state.query);
        let Ok(mut regex) = RegexSearch::new(&pattern) else {
            // A half-typed regex like `foo(`: say so rather than silently
            // matching nothing.
            state.current = None;
            state.invalid = true;
            session.term.lock().selection = None;
            cx.notify();
            return;
        };
        state.invalid = false;
        let vi_direction = state.vi_direction;
        let mut term = session.term.lock();
        let screen_lines = term.screen_lines() as i32;
        let last_column = term.last_column();
        let origin = match (&state.current, from_current) {
            (Some(m), true) => {
                // Step one cell past the current match so we advance.
                let p = if direction == Direction::Left {
                    *m.start()
                } else {
                    *m.end()
                };
                step_point(&*term, p, direction)
            }
            // Copy mode searches from the vi cursor; the bar searches from
            // the bottom of the screen, i.e. "the nearest one behind me".
            _ if vi_direction.is_some() => term.vi_mode_cursor.point,
            _ => GridPoint::new(Line(screen_lines - 1), last_column),
        };
        let found = term.search_next(&mut regex, origin, direction, Side::Left, None);
        if let Some(m) = &found {
            let mut selection = Selection::new(SelectionType::Simple, *m.start(), Side::Left);
            selection.update(*m.end(), Side::Right);
            term.selection = Some(selection);
            // Scroll the match into the middle of the viewport.
            let history = term.grid().history_size() as i32;
            let offset = term.grid().display_offset() as i32;
            let line = m.start().line.0;
            if line < -offset || line >= screen_lines - offset {
                let target = (-line + screen_lines / 2).clamp(0, history);
                term.scroll_display(Scroll::Delta(target - offset));
            }
        }
        drop(term);
        if let Some(state) = &mut self.search {
            state.current = found;
        }
        cx.notify();
    }

    // --- Prompt jumping ---

    fn prompt_up(&mut self, _: &PromptUp, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        let history = term.grid().history_size();
        let offset = term.grid().display_offset();
        for &mark in self.prompt_marks.iter().rev() {
            let target = history.saturating_sub(mark).min(history);
            if target > offset {
                term.scroll_display(Scroll::Delta(target as i32 - offset as i32));
                drop(term);
                cx.notify();
                return;
            }
        }
    }

    fn prompt_down(&mut self, _: &PromptDown, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        let history = term.grid().history_size();
        let offset = term.grid().display_offset();
        if offset == 0 {
            return;
        }
        for &mark in self.prompt_marks.iter() {
            let target = history.saturating_sub(mark).min(history);
            if target < offset {
                term.scroll_display(Scroll::Delta(target as i32 - offset as i32));
                drop(term);
                cx.notify();
                return;
            }
        }
        term.scroll_display(Scroll::Bottom);
        drop(term);
        cx.notify();
    }

    fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.paste_text(text, cx);
    }

    /// Hand `text` to the shell as a paste (bracketed when the program
    /// asked for it).
    fn paste_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.vi.is_some() {
            return; // nothing reaches the shell in copy mode
        }
        let Some(session) = &self.session else { return };
        let bracketed = session
            .term
            .lock()
            .mode()
            .contains(TermMode::BRACKETED_PASTE);
        let bytes = keys::prepare_paste(&text, bracketed);
        if self.broadcast {
            cx.emit(TerminalEvent::Input(bytes.clone()));
        }
        session.write_input(bytes);
        session.term.lock().scroll_display(Scroll::Bottom);
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    fn select_all(&mut self, _: &SelectAll, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        let top = GridPoint::new(term.topmost_line(), Column(0));
        let bottom = GridPoint::new(term.bottommost_line(), term.last_column());
        let mut selection = Selection::new(SelectionType::Lines, top, Side::Left);
        selection.update(bottom, Side::Right);
        term.selection = Some(selection);
        drop(term);
        cx.notify();
    }

    fn clear_scrollback(
        &mut self,
        _: &ClearScrollback,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use alacritty_terminal::vte::ansi::{ClearMode, Handler};
        let Some(session) = &self.session else { return };
        let mut term = session.term.lock();
        term.clear_screen(ClearMode::Saved);
        drop(term);
        self.prompt_marks.clear();
        self.log.forget_rows();
        self.trim_images(true, cx);
        cx.notify();
    }

    /// Convert a window position to a grid point plus cell side.
    fn grid_point(&self, position: gpui::Point<Pixels>) -> Option<(GridPoint, Side, usize, usize)> {
        let layout = self.last_layout?;
        let x = f32::from(position.x - layout.bounds.origin.x) - self.config.window.padding.x;
        let y = f32::from(position.y - layout.bounds.origin.y) - self.config.window.padding.y;
        let col_f = (x / layout.cell_width).max(0.0);
        let col = (col_f as usize).min(self.size.columns.saturating_sub(1));
        let row = ((y / layout.cell_height).max(0.0) as usize)
            .min(self.size.screen_lines.saturating_sub(1));
        let side = if col_f.fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let line = row as i32 - layout.display_offset as i32;
        Some((GridPoint::new(Line(line), Column(col)), side, col, row))
    }

    /// The code block whose "copy" link is under `point`: the markdown
    /// renderer wraps that label in an `omnipty-copy:N` hyperlink. Only a pane
    /// that was handed preview code honours it, so other programs' output
    /// can't reach the clipboard this way.
    fn preview_code_at(&self, point: GridPoint) -> Option<String> {
        if self.preview_code.is_empty() {
            return None;
        }
        let link = self.session.as_ref()?.term.lock().grid()[point].hyperlink()?;
        let index = link.uri().strip_prefix(crate::markdown::COPY_URI)?;
        self.preview_code.get(index.parse::<usize>().ok()?).cloned()
    }

    /// The cells of the "copy" link under `point`, for the hover underline.
    fn copy_link_span(&self, point: GridPoint, row: usize) -> Option<HoverSpan> {
        self.preview_code_at(point)?;
        let term = self.session.as_ref()?.term.lock();
        let line = &term.grid()[point.line];
        let link = line[point.column].hyperlink();
        let linked = |col: usize| line[Column(col)].hyperlink() == link;
        let start = (0..point.column.0).rev().take_while(|&c| linked(c)).last();
        let end = (point.column.0..self.size.columns)
            .take_while(|&c| linked(c))
            .last()?;
        Some(HoverSpan {
            row,
            start: start.unwrap_or(point.column.0),
            end: end + 1,
        })
    }

    fn mouse_mode_active(&self, shift: bool) -> bool {
        if shift {
            return false; // shift bypasses reporting to force local selection
        }
        self.session
            .as_ref()
            .map(|s| s.term.lock().mode().intersects(TermMode::MOUSE_MODE))
            .unwrap_or(false)
    }

    fn send_mouse_report(
        &self,
        button: u8,
        col: usize,
        row: usize,
        pressed: bool,
        mods: &gpui::Modifiers,
    ) {
        let Some(session) = &self.session else { return };
        let mode = *session.term.lock().mode();
        let mut b = button;
        if mods.shift {
            b += 4;
        }
        if mods.alt {
            b += 8;
        }
        if mods.control {
            b += 16;
        }
        if mode.contains(TermMode::SGR_MOUSE) {
            let ch = if pressed { 'M' } else { 'm' };
            session.write_input(format!("\x1b[<{};{};{}{}", b, col + 1, row + 1, ch).into_bytes());
        } else if col < 223 && row < 223 {
            let b = if pressed { b } else { 3 };
            session.write_input(vec![
                0x1b,
                b'[',
                b'M',
                32 + b,
                33 + col as u8,
                33 + row as u8,
            ]);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let Some((point, side, col, row)) = self.grid_point(event.position) else {
            return;
        };
        if open_modifier(&event.modifiers) {
            match self.target_at(point).map(|(t, _)| t) {
                Some(ClickTarget::Url(url)) => cx.open_url(&url),
                Some(ClickTarget::Path { path, line, col }) => {
                    if path.is_dir() {
                        cx.emit(TerminalEvent::RevealDir(path));
                    } else {
                        cx.emit(TerminalEvent::OpenPath { path, line, col });
                    }
                }
                None => {}
            }
            return;
        }
        if let Some(code) = self.preview_code_at(point) {
            let lines = code.lines().count();
            cx.write_to_clipboard(ClipboardItem::new_string(code));
            let s = if lines == 1 { "" } else { "s" };
            cx.emit(TerminalEvent::Notice(format!("copied {lines} line{s}")));
            return;
        }
        if self.mouse_mode_active(event.modifiers.shift) {
            self.send_mouse_report(0, col, row, true, &event.modifiers);
            self.reporting = Some((col, row));
            cx.notify();
            return;
        }
        let ty = match event.click_count {
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        if let Some(session) = &self.session {
            let mut term = session.term.lock();
            // In copy mode a click also moves the vi cursor there.
            if self.vi.is_some() {
                term.vi_goto_point(point);
            }
            let mut selection = Selection::new(ty, point, side);
            if event.click_count > 1 {
                selection.include_all();
            }
            term.selection = Some(selection);
            drop(term);
            self.selecting = true;
            self.select_from_y = event.position.y.into();
            cx.notify();
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button.is_none() {
            // cmd-hover (ctrl-hover on Linux): underline whatever a click
            // would open.
            let mut url = None;
            let span = if open_modifier(&event.modifiers) {
                self.grid_point(event.position)
                    .and_then(|(point, _, _, row)| {
                        self.target_at(point).map(|(target, token)| {
                            if let ClickTarget::Url(u) = target {
                                url = Some(u);
                            }
                            HoverSpan {
                                row,
                                start: token.start,
                                end: token.end,
                            }
                        })
                    })
            } else {
                // A preview's "copy" link underlines on plain hover.
                self.grid_point(event.position)
                    .and_then(|(point, _, _, row)| self.copy_link_span(point, row))
            };
            if span != self.hover || url != self.hover_url {
                self.hover = span;
                self.hover_url = url;
                cx.notify();
            }
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some((_, _, col, row)) = self.grid_point(event.position) else {
            return;
        };
        if self.mouse_mode_active(event.modifiers.shift) {
            let drag = self
                .session
                .as_ref()
                .map(|s| {
                    s.term
                        .lock()
                        .mode()
                        .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
                })
                .unwrap_or(false);
            // Once per cell, as the protocol has it: macOS repeats a held
            // drag at 60Hz. Past the pane's edge (the element listens
            // window-wide during a drag) it's the nearest cell.
            if drag && self.reporting.is_some() && self.reporting != Some((col, row)) {
                self.send_mouse_report(32, col, row, true, &event.modifiers);
                self.reporting = Some((col, row));
            }
            return;
        }
        if self.selecting {
            self.drag_selection(event.position, cx);
        }
    }

    /// A selection drag, in the pane or out of it (the element listens
    /// window-wide while one is under way): the selection follows the
    /// pointer, and at the pane's top or bottom edge, or past it, the view
    /// scrolls towards it.
    fn drag_selection(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let lines = self.autoscroll_lines(position);
        self.autoscroll = (lines != 0).then_some(position);
        self.extend_selection(position, 0, cx);
        if lines == 0 || self.autoscrolling {
            return;
        }
        // A timer, not the mouse events: a pointer held still keeps scrolling.
        self.autoscrolling = true;
        cx.spawn(async move |this, cx| {
            loop {
                let Ok(timer) = this.update(cx, |_, cx| {
                    cx.background_executor().timer(Duration::from_millis(50))
                }) else {
                    break;
                };
                timer.await;
                if !this
                    .update(cx, |pane, cx| pane.autoscroll_tick(cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// One step of the selection scroll; false once the pointer has come
    /// away from the edge or the button is up.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if let (true, Some(position)) = (self.selecting, self.autoscroll) {
            self.extend_selection(position, self.autoscroll_lines(position), cx);
            return true;
        }
        self.autoscroll = None;
        self.autoscrolling = false;
        false
    }

    fn autoscroll_lines(&self, position: gpui::Point<Pixels>) -> i32 {
        let Some(layout) = self.last_layout else {
            return 0;
        };
        autoscroll_lines(
            position.y.into(),
            self.select_from_y,
            &layout,
            self.config.window.padding.y,
        )
    }

    /// Scroll the view by `scroll` lines, then stretch the selection to the
    /// cell nearest `position`.
    fn extend_selection(
        &mut self,
        position: gpui::Point<Pixels>,
        scroll: i32,
        cx: &mut Context<Self>,
    ) {
        let (Some((_, side, col, row)), Some(session)) = (self.grid_point(position), &self.session)
        else {
            return;
        };
        let mut term = session.term.lock();
        if scroll != 0 {
            term.scroll_display(Scroll::Delta(scroll));
        }
        // Not `grid_point`'s line: that one is from the last painted frame,
        // before this scroll.
        let line = row as i32 - term.grid().display_offset() as i32;
        if let Some(selection) = term.selection.as_mut() {
            selection.update(GridPoint::new(Line(line), Column(col)), side);
        }
        drop(term);
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.selecting = false;
            self.autoscroll = None;
            if let Some(text) = self.selection_text()
                && !text.is_empty()
            {
                // Linux: a selection is the primary selection, always —
                // middle-click pastes it. The clipboard proper only on
                // request (copy_on_select), as on macOS. GPUI only has the
                // primary-selection calls on the platforms that have one.
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                cx.write_to_primary(ClipboardItem::new_string(text.clone()));
                if self.config.copy_on_select {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            return;
        }
        if self.reporting.take().is_some() {
            cx.notify();
        }
        let Some((_, _, col, row)) = self.grid_point(event.position) else {
            return;
        };
        if self.mouse_mode_active(event.modifiers.shift) {
            self.send_mouse_report(0, col, row, false, &event.modifiers);
        }
    }

    /// Middle click: the X11/Wayland primary-selection paste. Programs
    /// tracking the mouse get the button instead.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "freebsd")),
        allow(unused_variables)
    )]
    fn on_middle_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if let Some((_, _, col, row)) = self.grid_point(event.position)
            && self.mouse_mode_active(event.modifiers.shift)
        {
            self.send_mouse_report(1, col, row, true, &event.modifiers);
        } else {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                self.paste_text(text, cx);
            }
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = &self.session else { return };
        let cell_height = self.last_layout.map(|l| l.cell_height).unwrap_or(17.0);
        let delta_lines = match event.delta {
            ScrollDelta::Lines(p) => p.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / cell_height,
        };
        self.scroll_accum += delta_lines;
        let lines = self.scroll_accum.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;

        let mode = *session.term.lock().mode();
        match wheel_target(mode, event.modifiers.shift) {
            WheelTarget::Report => {
                if let Some((_, _, col, row)) = self.grid_point(event.position) {
                    let button = if lines > 0 { 64 } else { 65 };
                    for _ in 0..lines.abs() {
                        self.send_mouse_report(button, col, row, true, &event.modifiers);
                    }
                }
            }
            WheelTarget::Arrows => {
                let seq: &[u8] = match (mode.contains(TermMode::APP_CURSOR), lines > 0) {
                    (true, true) => b"\x1bOA",
                    (true, false) => b"\x1bOB",
                    (false, true) => b"\x1b[A",
                    (false, false) => b"\x1b[B",
                };
                session.write_input(seq.repeat(lines.unsigned_abs() as usize));
            }
            WheelTarget::Scrollback => {
                session.term.lock().scroll_display(Scroll::Delta(lines));
                cx.notify();
            }
        }
    }
}

/// Lines a selection drag held at `y` scrolls the view each tick: up (into
/// the scrollback) on the pane's first row, down on its last, and more the
/// further past the pane it is. A drag that began at `from_y` on one of
/// those rows doesn't scroll there: selecting along the top row shouldn't
/// move it away. `padding` is the gap between the pane's edge and its rows.
fn autoscroll_lines(y: f32, from_y: f32, layout: &LastLayout, padding: f32) -> i32 {
    let (top, bottom) = (
        f32::from(layout.bounds.top()),
        f32::from(layout.bounds.bottom()),
    );
    let (first_row, last_row) = (
        top + padding + layout.cell_height,
        bottom - padding - layout.cell_height,
    );
    let rows_past = |distance: f32| (1 + (distance / layout.cell_height.max(1.0)) as i32).min(8);
    if y < top {
        rows_past(top - y)
    } else if y > bottom {
        -rows_past(y - bottom)
    } else if y < first_row && from_y >= first_row {
        1
    } else if y > last_row && from_y <= last_row {
        -1
    } else {
        0
    }
}

/// What the scroll wheel drives.
#[derive(Debug, PartialEq, Eq)]
enum WheelTarget {
    /// The program tracks the mouse: it gets wheel button reports.
    Report,
    /// An alt-screen program that doesn't (less, man): arrow keys, or the
    /// wheel would do nothing there.
    Arrows,
    Scrollback,
}

/// Mouse reporting is asked first. Alternate scroll is on by default, so
/// asking it first sent arrows to every alt-screen program — and the wheel
/// moved the cursor in neovim and lazygit instead of scrolling the view.
fn wheel_target(mode: TermMode, shift: bool) -> WheelTarget {
    if mode.intersects(TermMode::MOUSE_MODE) && !shift {
        WheelTarget::Report
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        WheelTarget::Arrows
    } else {
        WheelTarget::Scrollback
    }
}

impl TerminalPane {
    /// The failure gutter: a tick per command down the right edge, placed by
    /// its share of the whole buffer. Dim for success, red for failure,
    /// accent while running. Click to scroll there.
    fn render_gutter(&self, cx: &Context<Self>) -> Option<gpui::Div> {
        if self.log.is_empty() || self.child_exited.is_some() {
            return None;
        }
        let session = self.session.as_ref()?;
        let (total, alt) = {
            let term = session.term.lock();
            (
                term.grid().history_size() + term.screen_lines(),
                term.mode().contains(TermMode::ALT_SCREEN),
            )
        };
        if alt || total == 0 {
            return None;
        }
        let theme = &self.theme;
        let dim = colors::blend(theme.foreground, theme.background, 0.6);
        let mut strip = div()
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(gpui::px(6.0));
        for cmd in self.log.entries() {
            let Some(row) = cmd.prompt_row else { continue };
            let color = if cmd.finished.is_none() {
                theme.ansi[4]
            } else if cmd.failed() {
                theme.ansi[1]
            } else {
                dim
            };
            let frac = (row as f32 / total as f32).clamp(0.0, 1.0);
            let id = cmd.id;
            strip = strip.child(
                div()
                    .id(("gutter-mark", id as usize))
                    .absolute()
                    .right_0()
                    .top(gpui::relative(frac))
                    .w(gpui::px(4.0))
                    .h(gpui::px(3.0))
                    .rounded_sm()
                    .bg(color)
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            this.scroll_to_row(row, cx);
                        }),
                    ),
            );
        }
        Some(strip)
    }
}

impl Render for TerminalPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let theme = self.theme.clone();
        let vi_mode = self.vi_mode();
        let vi_count = self.vi_count();
        let scrolled_lines = self.last_layout.map(|l| l.display_offset).unwrap_or(0);
        let search_options = self.search_options;
        let dim = colors::blend(theme.foreground, theme.background, 0.45);

        div()
            .id("terminal-pane")
            .key_context(if vi_mode.is_some() {
                "TerminalVi"
            } else {
                "Terminal"
            })
            .track_focus(&self.focus_handle)
            .size_full()
            .relative()
            .overflow_hidden()
            // No background of its own: the window paints one behind every
            // pane, at `window.opacity`.
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::clear_scrollback))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::search_toggle_regex))
            .on_action(cx.listener(Self::search_toggle_case))
            .on_action(cx.listener(Self::search_toggle_word))
            .on_action(cx.listener(Self::copy_mode))
            .on_action(cx.listener(Self::prompt_up))
            .on_action(cx.listener(Self::prompt_down))
            .on_action(cx.listener(Self::copy_last_output))
            .on_action(cx.listener(Self::copy_last_command))
            .on_action(cx.listener(Self::copy_last_block))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_modifiers_changed(
                cx.listener(|this, ev: &gpui::ModifiersChangedEvent, _w, cx| {
                    if !open_modifier(&ev.modifiers) && this.hover.is_some() {
                        this.hover = None;
                        this.hover_url = None;
                        cx.notify();
                    }
                }),
            )
            .when(self.hover.is_some(), |d| d.cursor_pointer())
            // Files dropped from Finder, or dragged out of the tree, land at
            // the prompt as quoted paths.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                window.focus(&this.focus_handle);
                for path in paths.paths() {
                    this.insert_path(path, true, cx);
                }
                cx.notify();
            }))
            .on_drop(
                cx.listener(|this, drag: &crate::tree::TreeDrag, window, cx| {
                    window.focus(&this.focus_handle);
                    this.insert_path(&drag.path, false, cx);
                    cx.notify();
                }),
            )
            .child(TerminalElement::new(cx.entity(), focused))
            .when_some(self.render_gutter(cx), |this, gutter| this.child(gutter))
            // Where a cmd-click would go, browser-style: the one place an
            // OSC 8 link's real address is visible.
            .when_some(self.hover_url.clone(), |this, url| {
                this.child(
                    div()
                        .absolute()
                        .bottom_1()
                        .left_1()
                        .max_w_full()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.ansi[0])
                        .text_size(gpui::px(11.0))
                        .text_color(dim)
                        .overflow_hidden()
                        .child(url),
                )
            })
            // "N lines above": you're looking at history, and how far back.
            .when(
                scrolled_lines > 0 && self.search.is_none() && self.child_exited.is_none(),
                |this| {
                    this.child(
                        div()
                            .absolute()
                            .top_2()
                            .right_2()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.ansi[0])
                            .text_size(gpui::px(11.0))
                            .text_color(dim)
                            .child(format!("▲ {} lines above", group_thousands(scrolled_lines))),
                    )
                },
            )
            .when_some(self.search.as_ref(), |this, search| {
                let hint = if search.invalid {
                    "invalid pattern"
                } else if search.current.is_some() && search.vi_direction.is_some() {
                    "⏎ go  esc"
                } else if search.current.is_some() {
                    "⏎ older  ⇧⏎ newer  esc"
                } else if search.query.is_empty() {
                    "type to search"
                } else {
                    "no match"
                };
                let ok = (search.current.is_some() || search.query.is_empty()) && !search.invalid;
                let prefix = match search.vi_direction {
                    Some(Direction::Left) => "?",
                    _ => "/",
                };
                let chip = |ix: u8, label: &'static str, on: bool| {
                    let (fg, bg) = if on {
                        (theme.background, theme.ansi[4])
                    } else {
                        (dim, theme.ansi[0])
                    };
                    div()
                        .id(("search-toggle", ix as usize))
                        .px_1()
                        .rounded_sm()
                        .bg(bg)
                        .text_color(fg)
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.toggle_search_option(ix, cx);
                            }),
                        )
                        .child(label)
                };
                this.child(
                    div()
                        .absolute()
                        .top_2()
                        .right_2()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme.ansi[0])
                        .border_1()
                        .border_color(if ok { theme.ansi[4] } else { theme.ansi[1] })
                        .flex()
                        .flex_row()
                        .gap_2()
                        .items_center()
                        .text_size(gpui::px(12.0))
                        .child(format!("{prefix}{}", search.query))
                        .child(div().text_color(dim).child(hint))
                        // The toggles, as chips: lit when on, clickable.
                        .child(chip(0, ".*", search_options.regex))
                        .child(chip(1, "Aa", search_options.case_sensitive))
                        .child(chip(2, "ab|", search_options.whole_word)),
                )
            })
            // Copy mode indicator — where vim puts its `-- VISUAL --`.
            .when_some(vi_mode, |this, kind| {
                let label = match vi_count {
                    Some(n) => format!("-- {} -- {n}", kind.label()),
                    None => format!("-- {} --", kind.label()),
                };
                this.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .right_2()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.ansi[3])
                        .text_color(theme.background)
                        .text_size(gpui::px(11.0))
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(label),
                )
            })
            // A startup command queued for this pane: visible before it
            // runs, then replaced by the command's own output.
            .when_some(self.startup_chip(), |this, label| {
                this.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_2()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.ansi[0])
                        .text_size(gpui::px(11.0))
                        .text_color(dim)
                        .child(label),
                )
            })
            .when(
                self.bell_until.is_some_and(|t| Instant::now() < t),
                |this| {
                    let mut flash = theme.foreground;
                    flash.a = 0.12;
                    this.child(div().absolute().inset_0().bg(flash))
                },
            )
            .when_some(self.child_exited, |this, code| {
                let message = match code {
                    Some(code) => format!("[process exited with code {code}]"),
                    None => "[process exited]".to_string(),
                };
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .bg(theme.ansi[0])
                                .text_color(theme.foreground)
                                .child(format!("{message} — press ⏎ to restart")),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    #[test]
    fn options_build_the_expected_pattern() {
        let plain = SearchOptions::default();
        assert_eq!(plain.pattern("a.b"), "(?i)a\\.b");
        let re = SearchOptions {
            regex: true,
            ..Default::default()
        };
        assert_eq!(re.pattern("a.b"), "(?i)a.b");
        let word = SearchOptions {
            whole_word: true,
            case_sensitive: true,
            ..Default::default()
        };
        assert_eq!(word.pattern("Err"), "(?-i)(?-u:\\b)(?:Err)(?-u:\\b)");
        // Every combination compiles, and a broken regex is reported.
        for regex in [false, true] {
            for case in [false, true] {
                for word in [false, true] {
                    let o = SearchOptions {
                        regex,
                        case_sensitive: case,
                        whole_word: word,
                    };
                    assert!(
                        RegexSearch::new(&o.pattern("foo(bar)")).is_ok() || regex,
                        "{o:?}"
                    );
                }
            }
        }
        assert!(RegexSearch::new(&re.pattern("foo(")).is_err());
    }

    #[test]
    fn a_selection_dragged_past_the_pane_scrolls_towards_the_pointer() {
        let layout = LastLayout {
            bounds: gpui::Bounds::new(
                gpui::point(gpui::px(0.0), gpui::px(100.0)),
                gpui::size(gpui::px(800.0), gpui::px(400.0)),
            ),
            cell_width: 8.0,
            cell_height: 20.0,
            display_offset: 0,
        };
        // A drag that began mid-pane (y 300), with 8px of padding.
        let lines = |y: f32| autoscroll_lines(y, 300.0, &layout, 8.0);
        assert_eq!(lines(300.0), 0);
        // Above: into the scrollback, faster the further out, up to a cap.
        assert_eq!(lines(95.0), 1);
        assert_eq!(lines(55.0), 3);
        assert_eq!(lines(-5000.0), 8);
        assert_eq!(lines(505.0), -1);
        // Reaching the first or last row is enough; the one next to it isn't.
        assert_eq!(lines(120.0), 1);
        assert_eq!(lines(480.0), -1);
        assert_eq!(lines(135.0), 0);
        assert_eq!(lines(465.0), 0);
        // Unless the drag began on that row.
        assert_eq!(autoscroll_lines(485.0, 480.0, &layout, 8.0), 0);
        assert_eq!(autoscroll_lines(505.0, 480.0, &layout, 8.0), -1);
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(2340), "2,340");
        assert_eq!(group_thousands(1234567), "1,234,567");
    }
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    #[test]
    fn a_program_tracking_the_mouse_gets_the_wheel_not_arrows() {
        let alt = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        // neovim, lazygit: alt screen with mouse reporting on.
        let tracking = alt | TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(wheel_target(tracking, false), WheelTarget::Report);
        // shift takes the wheel back from the program.
        assert_eq!(wheel_target(tracking, true), WheelTarget::Arrows);
        // less, man: alt screen, no mouse.
        assert_eq!(wheel_target(alt, false), WheelTarget::Arrows);
        assert_eq!(
            wheel_target(TermMode::ALTERNATE_SCROLL, false),
            WheelTarget::Scrollback
        );
    }
}
