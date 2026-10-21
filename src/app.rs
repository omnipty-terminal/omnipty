use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::AppContext as _;
use gpui::prelude::FluentBuilder;
use gpui::{
    Action, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, MenuItem,
    ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};

use crate::config::schema::{
    CloseLastTab, ColorsConfig, OpenIn, StatusBarPosition, StatusBarTab, TitlebarMode,
};
use crate::config::theme::parse_hex;
use crate::config::{self, Config, Theme};
use crate::git::{GitStatus, read_git_status};
use crate::keymap::actions::*;
use crate::keymap::registry::{self, ActionContext, ActionMeta};
use crate::keymap::resolve::pretty_keys;
use crate::keymap::{self, ResolvedKeymap};
use crate::line_edit::{self, LineEdit};
use crate::notifications;
use crate::palette::{self, PaletteItem};
use crate::panes::{Axis, Direction, Node, NodePath};
use crate::startup::{OnExit, StartupCommand};
use crate::terminal::colors::{blend, translucent};
use crate::terminal::commands::format_duration;
use crate::terminal::{LastLayout, TerminalEvent, TerminalPane};
use crate::tree::{FileTree, TreeEvent, TreePrompt};
use crate::workspaces::{SavedPane, SavedTab, SavedWorkspace};

pub type PaneId = u64;

/// One tab: a split tree of panes and which of them is focused.
struct TabState {
    layout: Node<PaneId>,
    active: PaneId,
    /// Output arrived while this tab was in the background.
    unread: bool,
    /// A pane shown at full tab size, hiding its siblings. Transient: not
    /// saved with the workspace.
    zoomed: Option<PaneId>,
    /// Keystrokes and pastes go to every pane in the tab.
    broadcast: bool,
    /// A user-set name, overriding the automatic title.
    title: Option<String>,
    /// The pane that had focus when this tab was opened on top of it (a
    /// markdown preview, What's New). Closing the tab goes back to it
    /// instead of to a neighbour. Transient: not saved with the workspace.
    opener: Option<PaneId>,
}

impl TabState {
    fn new(layout: Node<PaneId>, active: PaneId) -> Self {
        Self {
            layout,
            active,
            unread: false,
            zoomed: None,
            broadcast: false,
            title: None,
            opener: None,
        }
    }
}

/// A tab being dragged along the bar, by its index.
#[derive(Clone)]
struct TabDrag {
    ix: usize,
}

/// A workspace being dragged up or down the panel, by its index.
#[derive(Clone)]
struct WsDrag {
    ix: usize,
}

/// A workspace row's gap from the drawer's edges (`mx_2`), and its height.
const WS_ROW_INSET: f32 = 8.0;
const WS_ROW_HEIGHT: f32 = 26.0;

/// What follows the pointer while a workspace is dragged: the row itself,
/// lifted, kept in the panel's column so it only travels up and down.
struct WsDragCard {
    name: gpui::SharedString,
    pinned: bool,
    width: f32,
    /// Where along the row it was grabbed.
    grab_x: f32,
    /// The preview is drawn outside the window's root, so it inherits
    /// neither the font nor the colours.
    font: gpui::SharedString,
    theme: Rc<Theme>,
}

impl Render for WsDragCard {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let mut border = accent;
        border.a = 0.5;
        // GPUI draws the preview at the pointer, less where the row was
        // grabbed; shift it back to the row's own left edge.
        let drawn_at = f32::from(window.mouse_position().x) - self.grab_x;
        div().w(px(self.width)).h(px(WS_ROW_HEIGHT)).child(
            div()
                .absolute()
                .top_0()
                .left(px(WS_ROW_INSET - drawn_at))
                .w(px(self.width))
                .h(px(WS_ROW_HEIGHT))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(blend(theme.background, accent, 0.2))
                .shadow_lg()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .font_family(self.font.clone())
                .text_size(px(13.0))
                .text_color(theme.foreground)
                .child(div().flex_1().truncate().child(self.name.clone()))
                .when(self.pinned, |d| {
                    d.child(div().flex_none().text_color(accent).child("\u{f08d}"))
                }),
        )
    }
}

/// The label that follows the pointer while a tab is dragged.
struct TabDragLabel(gpui::SharedString);

impl Render for TabDragLabel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(gpui::black())
            .text_color(gpui::white())
            .text_size(px(12.0))
            .child(self.0.clone())
    }
}

/// How many closed tabs cmd-shift-t can bring back.
const CLOSED_TAB_RING: usize = 10;

/// A named collection of tabs — the tmux-session analogue. Temporary by
/// default; `persist` opts it into surviving restarts (layout + directories,
/// fresh shells).
struct Workspace {
    name: String,
    persist: bool,
    tabs: Vec<TabState>,
    active_tab: usize,
}

/// A right-click menu on a workspace row: which row, and where to draw it.
struct WsContextMenu {
    ix: usize,
    position: gpui::Point<gpui::Pixels>,
}

/// Where the Linux ☰ menu button sits: inset from the window's top-left
/// corner, small enough to fit inside any of the bars that can be there.
const APP_MENU_BUTTON_LEFT: f32 = 6.0;
const APP_MENU_BUTTON_TOP: f32 = 4.0;
const APP_MENU_BUTTON_SIZE: f32 = 22.0;
/// Left padding the bar under the ☰ button needs so its own content starts
/// clear of it.
pub const APP_MENU_BUTTON_CLEARANCE: f32 = APP_MENU_BUTTON_LEFT + APP_MENU_BUTTON_SIZE + 6.0;

/// Which bar the ☰ menu button floats over, so that bar can pad for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppMenuCorner {
    StatusBar,
    Tree,
    TabBar,
}

/// The Linux stand-in for the macOS menu bar: a popover under the ☰ button
/// in the top-left corner. `open` is which top-level menu (OmniPTY, File, …)
/// is expanded in the right-hand column; hovering a heading switches it.
struct AppMenu {
    open: usize,
}

pub struct OmniPTY {
    config: Rc<Config>,
    theme: Rc<Theme>,
    tree: gpui::Entity<FileTree>,
    workspaces: Vec<Workspace>,
    active_ws: usize,
    /// Cursor row in the workspaces panel (may differ from `active_ws`).
    ws_selected: usize,
    ws_focus: FocusHandle,
    /// The panel's footer is asking "delete? (y/n)" about the selected row.
    ws_confirm_delete: bool,
    ws_context_menu: Option<WsContextMenu>,
    app_menu: Option<AppMenu>,
    /// The bundled icon for the About panel, decoded once per window.
    app_icon: std::sync::Arc<gpui::Image>,
    /// Every live pane across all workspaces and tabs.
    panes: HashMap<PaneId, gpui::Entity<TerminalPane>>,
    next_pane_id: PaneId,
    pane_subscriptions: HashMap<PaneId, Subscription>,
    drawer_visible: bool,
    /// The width the drawer was dragged to; `tree.width` until it has been.
    drawer_width: Option<f32>,
    /// The drawer's edge is being dragged.
    drawer_drag: bool,
    /// `window.opacity < 1` and `window.blur` as the window last had them
    /// applied, so a config reload can change either.
    translucency: Option<(bool, bool)>,
    toasts: Vec<Toast>,
    next_toast_id: usize,
    git_status: GitStatus,
    last_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    bounds_save_scheduled: bool,
    overlay: Option<Overlay>,
    /// Focus handle for whichever overlay is open; only one can be.
    picker_focus: FocusHandle,
    keymap: Rc<ResolvedKeymap>,
    /// Most recently run palette commands, newest first.
    palette_recent: VecDeque<&'static str>,
    divider_drag: Option<DividerDrag>,
    /// Panes whose focus ring is flashing red after a background failure.
    fail_flash: HashMap<PaneId, Instant>,
    /// A once-a-second repaint is running for the status bar's elapsed time.
    ticking: bool,
    /// Notification route keys → the pane a click should focus.
    notification_routes: HashMap<notifications::RouteKey, PaneId>,
    finder_index: Option<FinderIndex>,
    finder_indexing: bool,
    /// Files opened through the finder or cmd-click, newest first.
    recent_files: VecDeque<PathBuf>,
    status_bar_override: Option<bool>,
    tab_bar_override: Option<bool>,
    update: UpdateState,
    /// Recently closed tabs, newest first, for cmd-shift-t.
    closed_tabs: VecDeque<SavedTab>,
    /// The macOS appearance is dark. Drives `colors.follow_system`.
    dark_appearance: bool,
    /// Another app is frontmost, for `window.inactive_window_opacity`.
    window_active: bool,
    /// Saved startup commands run on restore. `workspaces.run_startup_commands`
    /// unless this launch opted out (`--no-startup-commands`, or shift held).
    run_startup_commands: bool,
    startup_skipped_at_launch: bool,
    /// When the window opened. Wayland (and X11) only report modifier
    /// state through events, which start once the window has keyboard
    /// focus — after `new` has already asked. The first modifier event
    /// inside `SHIFT_AT_LAUNCH_WINDOW` stands in for "was shift held".
    opened_at: Instant,
    _config_watcher: Option<
        notify_debouncer_full::Debouncer<
            notify::RecommendedWatcher,
            notify_debouncer_full::FileIdMap,
        >,
    >,
    _subscriptions: Vec<Subscription>,
}

/// A modal over the window. At most one is open at a time; both share the
/// `Overlay` keybinding context and `picker_focus`.
enum Overlay {
    Palette(PaletteState),
    ThemePicker(ThemePicker),
    History(HistoryState),
    FileFinder(FinderState),
    /// One line of text for whoever asked: a file's new name, a workspace's.
    Prompt(PromptState),
    Confirm(ConfirmState),
    /// One pane's startup command, from the terminal.
    StartupCommand(StartupCommandState),
    /// Every pane's startup command in one workspace, from the drawer.
    StartupEditor(StartupEditorState),
    /// Icon, version, and website: the About panel.
    About(AboutState),
}

struct AboutState {
    return_focus: FocusTarget,
}

struct StartupCommandState {
    pane: PaneId,
    buffer: LineEdit,
    on_exit: OnExit,
    /// Where the pane lives, for the header.
    cwd: Option<PathBuf>,
    return_focus: FocusTarget,
}

/// One row of the workspace-level editor.
struct StartupRow {
    pane: PaneId,
    /// `tab 1 · pane 2 — ~/dev/api`
    label: String,
    command: LineEdit,
    on_exit: OnExit,
}

struct StartupEditorState {
    /// Index of the workspace being edited.
    ws: usize,
    name: String,
    rows: Vec<StartupRow>,
    selected: usize,
    return_focus: FocusTarget,
}

struct PromptState {
    /// The action being asked about: "Rename main.rs".
    title: String,
    hint: &'static str,
    buffer: LineEdit,
    action: PromptAction,
    return_focus: FocusTarget,
}

/// What a prompt's text is for. Indices are into the active workspace's
/// tabs, or the workspace list.
enum PromptAction {
    Tree(TreePrompt),
    WsAdd,
    WsRename(usize),
    TabRename(usize),
}

/// A yes/no question before something destructive.
struct ConfirmState {
    message: String,
    return_focus: FocusTarget,
}

fn appearance_is_dark(appearance: gpui::WindowAppearance) -> bool {
    matches!(
        appearance,
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

/// Every file under the tree root, as root-relative strings, walked once
/// on the background pool and kept until the root changes or it goes stale.
struct FinderIndex {
    root: PathBuf,
    entries: Rc<Vec<String>>,
    truncated: bool,
    built: Instant,
}

/// The index is rebuilt when opened this long after it was walked.
const FINDER_INDEX_TTL: Duration = Duration::from_secs(30);
/// Entries beyond this are dropped, with a note in the overlay.
const FINDER_INDEX_CAP: usize = 100_000;

struct FinderMatch {
    entry: usize,
    highlights: Vec<usize>,
    score: i32,
}

struct FinderState {
    query: LineEdit,
    matches: Vec<FinderMatch>,
    selected: usize,
    scroll: usize,
    return_focus: FocusTarget,
}

/// What confirming a file-finder row does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinderAction {
    Open,
    Insert,
    Reveal,
}

/// One row of command history, gathered across every pane.
#[derive(Clone)]
struct HistoryItem {
    text: String,
    cwd: Option<PathBuf>,
    exit: Option<i32>,
    finished: Option<Instant>,
}

struct HistoryMatch {
    item: usize,
    highlights: Vec<usize>,
    score: i32,
}

struct HistoryState {
    query: LineEdit,
    items: Vec<HistoryItem>,
    /// Indices into `items`, best first.
    matches: Vec<HistoryMatch>,
    selected: usize,
    scroll: usize,
    return_focus: FocusTarget,
}

static NEXT_ROUTE: AtomicU64 = AtomicU64::new(1);

/// What had focus when an overlay opened, so closing it hands focus back.
/// Stored by identity rather than as a `FocusHandle`, so a pane that exits
/// meanwhile falls back to the active one instead of a dead element.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    Pane(PaneId),
    Tree,
    Workspaces,
}

struct ThemePicker {
    selected: usize,
    scroll: usize,
    /// Theme to restore on cancel.
    original: Rc<Theme>,
    return_focus: FocusTarget,
}

/// Rows visible in the palette list before it scrolls.
const PALETTE_ROWS: usize = 12;

struct PaletteState {
    query: LineEdit,
    /// The filtered, ranked list; recomputed on every keystroke.
    matches: Vec<PaletteItem>,
    selected: usize,
    /// Index of the first visible row.
    scroll: usize,
    return_focus: FocusTarget,
}

/// The smallest a pane may be dragged or resized to, in cells. Narrower
/// than this and TUI programs start misbehaving.
const MIN_PANE_COLS: f32 = 20.0;
const MIN_PANE_ROWS: f32 = 3.0;

/// An in-progress divider drag. Pixel deltas are converted to ratio deltas
/// against the split's measured extent, and only the two panes either side
/// of the divider move.
struct DividerDrag {
    path: NodePath,
    divider: usize,
    axis: Axis,
    start_ratios: Vec<f32>,
    /// Pointer position along the axis at mouse-down.
    start_pos: f32,
    /// The split's size along the axis, in pixels.
    extent: f32,
    min_ratio: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum ToastKind {
    Info,
    Error,
}

/// A corner notice. Transient ones time out; `sticky` ones (config and
/// keymap errors) stay until the next clean reload. Any toast goes on click;
/// the post-update one also opens the changelog tab.
#[derive(Clone)]
struct Toast {
    id: usize,
    kind: ToastKind,
    message: String,
    opens_changelog: bool,
    sticky: bool,
    /// The one-time crash-reports question: two buttons, and clicking the
    /// body does nothing. × leaves it unanswered, so it's asked again.
    asks_crash_reports: bool,
}

#[derive(Clone, PartialEq)]
// Each platform reaches only its own states: Available on Linux, the
// download/install pair on macOS.
#[allow(dead_code)]
enum UpdateState {
    Idle,
    Checking,
    /// Linux: a newer release exists; `url` is its page. No in-place
    /// install — packages come from the AUR or a tarball.
    Available {
        version: String,
        url: String,
    },
    /// macOS: downloading the DMG.
    Downloading(String),
    /// macOS: DMG on disk, one click from a swap-and-relaunch.
    Ready {
        version: String,
        dmg: PathBuf,
    },
}

pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

fn window_state_path() -> Option<PathBuf> {
    Some(crate::paths::cache_dir().join("window.txt"))
}

pub fn load_window_bounds() -> Option<gpui::Bounds<gpui::Pixels>> {
    let text = std::fs::read_to_string(window_state_path()?).ok()?;
    let mut parts = text
        .split_whitespace()
        .filter_map(|p| p.parse::<f32>().ok());
    let (x, y, w, h) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if w < 200.0 || h < 200.0 {
        return None;
    }
    Some(gpui::Bounds {
        origin: gpui::point(px(x), px(y)),
        size: gpui::size(px(w), px(h)),
    })
}

/// The width the drawer was last dragged to: a fifth number after the
/// window's bounds, there only once it has been dragged.
fn load_drawer_width() -> Option<f32> {
    let text = std::fs::read_to_string(window_state_path()?).ok()?;
    text.split_whitespace().nth(4)?.parse().ok()
}

/// No narrower than its rows are useful, no wider than leaves the terminal
/// room.
fn clamp_drawer_width(width: f32, window: f32) -> f32 {
    width.clamp(160.0, (window - 240.0).max(160.0))
}

/// The name a workspace gets until it's given one: `workspace N`, with the
/// lowest number no workspace in `names` is using.
fn default_ws_name<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let taken: Vec<&str> = names.collect();
    (1..)
        .map(|n| format!("workspace {n}"))
        .find(|name| !taken.contains(&name.as_str()))
        .unwrap()
}

/// Where the item at `ix` sits after the one at `from` is moved to `to`.
fn index_after_move(ix: usize, from: usize, to: usize) -> usize {
    if ix == from {
        to
    } else if from < ix && to >= ix {
        ix - 1
    } else if from > ix && to <= ix {
        ix + 1
    } else {
        ix
    }
}

/// Quote a path for the shell: single-quoted, embedded quotes escaped.
fn shell_quote(path: &Path) -> String {
    single_quote(&path.to_string_lossy())
}

/// Show an image file and wait for a key: what an image preview pane runs.
/// Nothing but `/bin/sh` and `base64` is needed, because the terminal it
/// prints to is OmniPTY. `fit` scales the picture down to the pane; without
/// it the picture is drawn at its own size, never blown up.
fn image_preview_command(path: &Path, fit: bool) -> String {
    let sizing = if fit { ";width=100%%;height=100%%" } else { "" };
    // Clear, hide the cursor, send the file inline (iTerm2's OSC 1337), then
    // read one key with the terminal's line editing off and put it back.
    let script = format!(
        r#"s=$(stty -g); stty -icanon -echo; printf "[2J[H[?25l]1337;File=inline=1{sizing}:"; base64 < "$1"; printf ""; dd bs=1 count=1 >/dev/null 2>&1; printf "[?25h"; stty "$s""#
    );
    format!(
        "/bin/sh -c {} sh {}",
        single_quote(&script),
        shell_quote(path)
    )
}

/// Whether `less` can wrap at spaces instead of mid-word. An older less
/// stops on an unknown flag with a "press RETURN" prompt, so ask first.
/// Probes the app's PATH; a shell with a different less is assumed newer.
fn less_wraps_words() -> bool {
    static WRAPS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WRAPS.get_or_init(|| {
        std::process::Command::new("less")
            .arg("--help")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("--wordwrap"))
    })
}

/// Wrap in single quotes for any Bourne-family shell.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Open a file in the user's editor, or hand it to the OS default text
/// editor when no $EDITOR is set — a fresh Mac has no nvim, and "command
/// not found: nvim" is a rough first impression.
///
/// Only the shell knows what $EDITOR is (a GUI-launched app does not inherit
/// it), so the choice has to be made there. `shell` is the program that will
/// run this: under fish, csh, or nushell the Bourne syntax below is a parse
/// error, so it goes to /bin/sh instead. That costs a non-exported $EDITOR —
/// worth it only where the direct form cannot run at all.
fn edit_file_command(path: &Path, shell: &str) -> String {
    editor_command(path, None, shell, None)
}

/// The Bourne snippet that opens `path_expr` (an already-quoted path, or a
/// `"$1"`-style reference) in `$EDITOR`, jumping to a line when the editor
/// is one whose flag we know. Only the shell knows what `$EDITOR` is, so the
/// mapping happens there, on the program name.
fn editor_snippet(
    path_expr: &str,
    at: Option<(u32, Option<u32>)>,
    override_cmd: Option<&str>,
) -> String {
    // No $EDITOR: the desktop's default text editor. macOS `open -t`;
    // Linux `xdg-open`, which picks the MIME handler.
    let fallback = if cfg!(target_os = "macos") {
        "open -t"
    } else {
        "xdg-open"
    };
    let Some((line, col)) = at else {
        return format!(
            "if [ -n \"${{EDITOR:-}}\" ]; then $EDITOR {path_expr}; else {fallback} {path_expr}; fi"
        );
    };
    if let Some(template) = override_cmd {
        return template
            .replace("{path}", path_expr)
            .replace("{line}", &line.to_string())
            .replace("{col}", &col.unwrap_or(1).to_string());
    }
    let colspec = col.map(|c| format!(":{c}")).unwrap_or_default();
    let vim = match col {
        Some(c) => format!("\"+call cursor({line},{c})\" {path_expr}"),
        None => format!("+{line} {path_expr}"),
    };
    format!(
        "if [ -n \"${{EDITOR:-}}\" ]; then case \"${{EDITOR##*/}}\" in \
         vim*|nvim*|vi|view) $EDITOR {vim};; \
         code*|cursor*|zed*|codium*) $EDITOR --goto {path_expr}:{line}{colspec};; \
         emacs*) $EDITOR +{line}{colspec} {path_expr};; \
         subl*|hx*|micro*) $EDITOR {path_expr}:{line}{colspec};; \
         *) $EDITOR {path_expr};; esac; else {fallback} {path_expr}; fi"
    )
}

/// Open a file in the user's editor, optionally at a line and column.
fn editor_command(
    path: &Path,
    at: Option<(u32, Option<u32>)>,
    shell: &str,
    override_cmd: Option<&str>,
) -> String {
    let quoted = shell_quote(path);
    if crate::terminal::session::is_posix_shell(shell) {
        return editor_snippet(&quoted, at, override_cmd);
    }
    // A path no shell can quote goes through a file instead, so the line typed
    // at the prompt holds no path text at all. Slower and less legible, so it
    // is only for the paths that need it.
    if path_needs_indirection(path)
        && let Some(name) = crate::prompt::integration::write_edit_target(path)
    {
        let body = editor_snippet("\"$p\"", at, override_cmd).replace('\'', "'\\''");
        return format!(
            "/bin/sh -c 'f=\"$HOME/.cache/omnipty/edit/{name}\"; p=$(cat \"$f\"); rm -f \"$f\"; {body}'"
        );
    }
    // Otherwise the path goes to /bin/sh as an argument rather than
    // interpolated into the script, so the script itself contains no single
    // quotes and the outer token stays a plain single-quoted string — which
    // fish, csh, and nushell all agree on.
    let body = editor_snippet("\"$1\"", at, override_cmd).replace('\'', "'\\''");
    format!("/bin/sh -c '{body}' omnipty {quoted}")
}

/// Characters that no single quoting style survives across every shell at
/// once: nushell has no escape for `'` inside a literal, fish reads `\` there
/// as an escape, and csh expands `!` even inside single quotes. Rare enough in
/// real paths to be worth an uglier route rather than a per-shell escaping
/// table for every shell someone might install.
fn path_needs_indirection(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str()
        .as_bytes()
        .iter()
        .any(|b| matches!(b, b'\'' | b'\\' | b'!'))
}

impl OmniPTY {
    pub fn new(
        config: Config,
        config_error: Option<String>,
        cwd_override: Option<PathBuf>,
        command: Option<Vec<String>>,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = Rc::new(config);
        let dark_appearance = appearance_is_dark(window.appearance());
        let theme = Rc::new(Theme::resolve(&config.colors, dark_appearance));
        // `omnipty <dir>` (or the CLI shim) starts rooted at that directory,
        // else where it was run from (what `xdg-terminal-exec --dir` sets);
        // otherwise home. See `Cli::working_directory`.
        let cwd = cwd_override
            .or_else(|| crate::cli::cli().working_directory())
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));

        let tree = cx.new(|cx| FileTree::new(cwd.clone(), config.clone(), theme.clone(), cx));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(&tree, window, Self::on_tree_event));
        // Light/dark auto-switching and window-level dimming both need to
        // know when the OS changes its mind.
        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            this.on_appearance_changed(window, cx);
        }));
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if this.window_active != active {
                this.window_active = active;
                cx.notify();
            }
        }));

        // Live config reload.
        let mut config_watcher = None;
        if let Some((watcher, mut rx)) = config::watch() {
            config_watcher = Some(watcher);
            cx.spawn(async move |this, cx| {
                while let Some(()) = rx.next().await {
                    let alive = this.update(cx, |this, cx| this.reload_config(cx)).is_ok();
                    if !alive {
                        break;
                    }
                }
            })
            .detach();
        }

        // Periodic git refresh for the status bar.
        cx.spawn(async move |this, cx| {
            loop {
                let timer = match this.update(cx, |_, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(3))
                }) {
                    Ok(timer) => timer,
                    Err(_) => break,
                };
                timer.await;
                if this
                    .update(cx, |this, cx| this.refresh_git_status(cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        // Bad [keymap] entries are skipped by `resolve`; report them here so
        // the user learns why a binding didn't take.
        let resolved = Rc::new(keymap::resolve(&config.keymap));
        let config_error = config_error.or_else(|| resolved.error_banner());

        let drawer_visible = config.tree.open_on_startup;
        let mut this = Self {
            config,
            theme,
            tree,
            workspaces: Vec::new(),
            active_ws: 0,
            ws_selected: 0,
            ws_focus: cx.focus_handle(),
            ws_confirm_delete: false,
            ws_context_menu: None,
            app_menu: None,
            app_icon: std::sync::Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!("../assets/linux/icons/hicolor/256x256/apps/omnipty.png").to_vec(),
            )),
            panes: HashMap::new(),
            next_pane_id: 0,
            pane_subscriptions: HashMap::new(),
            drawer_visible,
            drawer_width: load_drawer_width(),
            drawer_drag: false,
            translucency: None,
            toasts: Vec::new(),
            next_toast_id: 0,
            git_status: GitStatus::default(),
            last_bounds: None,
            bounds_save_scheduled: false,
            overlay: None,
            picker_focus: cx.focus_handle(),
            keymap: resolved,
            palette_recent: VecDeque::new(),
            divider_drag: None,
            fail_flash: HashMap::new(),
            ticking: false,
            notification_routes: HashMap::new(),
            finder_index: None,
            finder_indexing: false,
            recent_files: VecDeque::new(),
            status_bar_override: None,
            tab_bar_override: None,
            update: UpdateState::Idle,
            closed_tabs: VecDeque::new(),
            dark_appearance,
            window_active: window.is_window_active(),
            run_startup_commands: false,
            startup_skipped_at_launch: false,
            opened_at: Instant::now(),
            _config_watcher: config_watcher,
            _subscriptions: subscriptions,
        };
        // The escape hatches for a startup command that wedges the app:
        // a flag on the command line, or shift held while it launches.
        // Neither depends on any file the app writes.
        this.startup_skipped_at_launch =
            crate::cli::cli().no_startup_commands || window.modifiers().shift;
        this.run_startup_commands =
            this.config.workspaces.run_startup_commands && !this.startup_skipped_at_launch;
        this.bootstrap_workspaces(cwd, command, restore, window, cx);
        if let Some(message) = config_error {
            this.sticky_toast(message);
        }
        if this.startup_skipped_at_launch && this.any_startup_commands(cx) {
            this.toast(
                ToastKind::Info,
                "startup commands skipped for this launch".into(),
                cx,
            );
        }
        if let Some(previous) = crate::update::note_launch_version() {
            let current = env!("CARGO_PKG_VERSION");
            crate::telemetry::updated(&previous, current);
            // Long enough to read after the window settles, then gone:
            // What's New stays in the Help menu.
            let id = this.push_toast(
                ToastKind::Info,
                format!("updated v{previous} → v{current} — click for what's new"),
                true,
                false,
            );
            this.expire_toast(id, 20, cx);
        }
        this.ask_about_crash_reports();
        if let Some(report) = crate::telemetry::crash_notice() {
            let id = this.push_toast(
                ToastKind::Info,
                format!(
                    "OmniPTY quit unexpectedly last time. Crash report {} was sent; \
                     Help → Report an Issue will include it.",
                    report.get(..8).unwrap_or(report)
                ),
                false,
                false,
            );
            this.expire_toast(id, 30, cx);
        }
        this.refresh_git_status(cx);

        // Auto-check for updates: installed copies only (not cargo run),
        // shortly after launch and then every 6 hours.
        if crate::update::is_installed() && !cfg!(debug_assertions) {
            cx.spawn(async move |this, cx| {
                loop {
                    let timer = match this.update(cx, |_, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(15))
                    }) {
                        Ok(timer) => timer,
                        Err(_) => break,
                    };
                    timer.await;
                    if this
                        .update(cx, |this, cx| this.check_for_updates(false, cx))
                        .is_err()
                    {
                        break;
                    }
                    let timer = match this.update(cx, |_, cx| {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(6 * 3600))
                    }) {
                        Ok(timer) => timer,
                        Err(_) => break,
                    };
                    timer.await;
                }
            })
            .detach();
        }
        this
    }

    fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if matches!(
            self.update,
            UpdateState::Checking | UpdateState::Downloading(_)
        ) {
            return;
        }
        if let UpdateState::Ready { .. } = self.update {
            if manual {
                self.toast(
                    ToastKind::Info,
                    "update already downloaded — click the button to install".into(),
                    cx,
                );
            }
            return;
        }
        self.update = UpdateState::Checking;
        let bg = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let latest = bg.spawn(async move { crate::update::fetch_latest() }).await;
            let info = match latest {
                Ok(Some(info))
                    if crate::update::is_newer(&info.version, env!("CARGO_PKG_VERSION")) =>
                {
                    info
                }
                Ok(_) => {
                    this.update(cx, |this, cx| {
                        this.update = UpdateState::Idle;
                        if manual {
                            this.toast(
                                ToastKind::Info,
                                format!("OmniPTY is up to date (v{})", env!("CARGO_PKG_VERSION")),
                                cx,
                            );
                        }
                    })
                    .ok();
                    return;
                }
                Err(e) => {
                    this.update(cx, |this, cx| {
                        this.update = UpdateState::Idle;
                        if manual {
                            this.toast(ToastKind::Error, e, cx);
                        }
                    })
                    .ok();
                    return;
                }
            };
            let version = info.version.clone();
            // Linux: announce and point at the release; the package manager
            // (or the tarball) does the install.
            #[cfg(not(target_os = "macos"))]
            {
                this.update(cx, |this, cx| {
                    this.update = UpdateState::Available {
                        version,
                        url: info.url,
                    };
                    cx.notify();
                })
                .ok();
            }
            #[cfg(target_os = "macos")]
            {
                if this
                    .update(cx, |this, cx| {
                        this.update = UpdateState::Downloading(version.clone());
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                let bg2 = cx.background_executor().clone();
                let downloaded = bg2
                    .spawn(async move { crate::update::download(&info) })
                    .await;
                this.update(cx, |this, cx| {
                    match downloaded {
                        Ok(dmg) => {
                            this.update = UpdateState::Ready {
                                version: version.clone(),
                                dmg,
                            };
                        }
                        Err(e) => {
                            // The check just succeeded, so this isn't "offline":
                            // worth saying even on the automatic path.
                            this.update = UpdateState::Idle;
                            this.toast(ToastKind::Error, e, cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn install_update(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::Available { url, .. } = &self.update {
            cx.open_url(url);
        }
        #[cfg(target_os = "macos")]
        if let UpdateState::Ready { dmg, version } = &self.update {
            match crate::update::install_and_restart(dmg) {
                Ok(()) => {
                    if crate::update::installed_bundle().is_some() {
                        crate::update::note_install_attempt(version);
                        cx.quit();
                    } else {
                        self.toast(
                            ToastKind::Info,
                            "not running from an installed app — opened the DMG instead".into(),
                            cx,
                        );
                    }
                }
                Err(e) => self.toast(ToastKind::Error, e, cx),
            }
        }
    }

    fn for_each_pane(
        &mut self,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&mut TerminalPane, &mut Context<TerminalPane>),
    ) {
        for pane in self.panes.values().cloned().collect::<Vec<_>>() {
            pane.update(cx, |t, cx| f(t, cx));
        }
    }

    fn ws(&self) -> &Workspace {
        &self.workspaces[self.active_ws]
    }

    fn ws_mut(&mut self) -> &mut Workspace {
        let ix = self.active_ws;
        &mut self.workspaces[ix]
    }

    fn tab(&self) -> &TabState {
        let ws = self.ws();
        &ws.tabs[ws.active_tab]
    }

    fn tab_mut(&mut self) -> &mut TabState {
        let ws = self.ws_mut();
        let ix = ws.active_tab;
        &mut ws.tabs[ix]
    }

    fn active_id(&self) -> PaneId {
        self.tab().active
    }

    fn active_pane(&self) -> gpui::Entity<TerminalPane> {
        self.panes
            .get(&self.active_id())
            .cloned()
            .expect("active pane id is always present in the pane map")
    }

    fn create_pane(&mut self, cwd: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> PaneId {
        self.create_pane_running(cwd, None, window, cx)
    }

    /// A pane running `command` (`-e` from the command line) instead of the
    /// shell; `None` is the ordinary shell pane.
    fn create_pane_running(
        &mut self,
        cwd: PathBuf,
        command: Option<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let (config, theme) = (self.config.clone(), self.theme.clone());
        let pane = cx.new(|cx| TerminalPane::new(config, theme, cwd, command, cx));
        let tree_root = self.tree.read(cx).root.clone();
        pane.update(cx, |t, _| t.tree_root = Some(tree_root));
        let subscription = cx.subscribe_in(&pane, window, Self::on_terminal_event);
        self.panes.insert(id, pane);
        self.pane_subscriptions.insert(id, subscription);
        id
    }

    fn drop_pane(&mut self, id: PaneId) {
        self.panes.remove(&id);
        self.pane_subscriptions.remove(&id);
    }

    /// Which (workspace, tab) owns this pane — panes in background tabs can
    /// still exit and need to be removed from wherever they live.
    fn locate_pane(&self, id: PaneId) -> Option<(usize, usize)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                if tab.layout.leaves().contains(&id) {
                    return Some((wi, ti));
                }
            }
        }
        None
    }

    fn pane_bounds(&self, id: PaneId, cx: &Context<Self>) -> Option<gpui::Bounds<gpui::Pixels>> {
        self.panes.get(&id)?.read(cx).last_layout.map(|l| l.bounds)
    }

    /// Nearest pane in `direction`, chosen geometrically so navigation follows
    /// what is on screen rather than the shape of the split tree.
    fn pane_in_direction(&self, direction: Direction, cx: &Context<Self>) -> Option<PaneId> {
        // Zoomed: the siblings aren't on screen, so walk the tree order
        // instead — like tmux, which moves and stays zoomed.
        if self.tab().zoomed.is_some() {
            let leaves = self.tab().layout.leaves();
            let ix = leaves.iter().position(|id| *id == self.active_id())?;
            return match direction {
                Direction::Left | Direction::Up => ix.checked_sub(1).map(|i| leaves[i]),
                Direction::Right | Direction::Down => leaves.get(ix + 1).copied(),
            };
        }
        let current = self.pane_bounds(self.active_id(), cx)?;
        let (cx0, cy0) = (
            f32::from(current.origin.x) + f32::from(current.size.width) / 2.0,
            f32::from(current.origin.y) + f32::from(current.size.height) / 2.0,
        );
        let mut best: Option<(f32, PaneId)> = None;
        for id in self.tab().layout.leaves() {
            if id == self.active_id() {
                continue;
            }
            let Some(b) = self.pane_bounds(id, cx) else {
                continue;
            };
            let (left, top) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (right, bottom) = (
                left + f32::from(b.size.width),
                top + f32::from(b.size.height),
            );
            let (bx, by) = ((left + right) / 2.0, (top + bottom) / 2.0);
            let (cur_left, cur_top) = (f32::from(current.origin.x), f32::from(current.origin.y));
            let (cur_right, cur_bottom) = (
                cur_left + f32::from(current.size.width),
                cur_top + f32::from(current.size.height),
            );
            // Require overlap on the perpendicular axis so we do not jump to a
            // pane that merely happens to sit in that half of the window.
            let (ok, distance) = match direction {
                Direction::Left => (bx < cx0 && top < cur_bottom && bottom > cur_top, cx0 - bx),
                Direction::Right => (bx > cx0 && top < cur_bottom && bottom > cur_top, bx - cx0),
                Direction::Up => (by < cy0 && left < cur_right && right > cur_left, cy0 - by),
                Direction::Down => (by > cy0 && left < cur_right && right > cur_left, by - cy0),
            };
            if ok && best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Point the drawer at the focused pane's directory. Called whenever the
    /// active pane changes so the tree tracks whichever split you're in.
    fn sync_tree_to_active(&mut self, cx: &mut Context<Self>) {
        if !self.config.tree.follow_cwd {
            return;
        }
        let Some(cwd) = self.active_pane().read(cx).cwd.clone() else {
            return;
        };
        let tree = self.tree.clone();
        // Deferred: this can run from render, where re-entrant entity updates
        // are not allowed.
        cx.defer(move |cx| {
            tree.update(cx, |tree, cx| tree.set_root(cwd, cx));
        });
        self.refresh_git_status(cx);
    }

    fn focus_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(pane) = self.panes.get(&id).cloned() {
            let tab = self.tab_mut();
            tab.active = id;
            if tab.zoomed.is_some() && tab.layout.leaves().contains(&id) {
                tab.zoomed = Some(id);
            }
            window.focus(&pane.focus_handle(cx));
            self.sync_tree_to_active(cx);
            cx.notify();
        }
    }

    fn focus_in_direction(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_focus(cx).is_focused(window) || self.ws_focus.is_focused(window) {
            if direction == Direction::Right {
                let id = self.active_id();
                self.focus_pane(id, window, cx);
            }
            return;
        }
        match self.pane_in_direction(direction, cx) {
            Some(id) => self.focus_pane(id, window, cx),
            // Off the left edge of the panes: the drawer is what is over there.
            None if direction == Direction::Left => self.focus_tree(Some(window), cx),
            None => {}
        }
    }

    fn split_active(&mut self, direction: Direction, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self
            .active_pane()
            .read(cx)
            .cwd
            .clone()
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let id = self.create_pane(cwd, window, cx);
        self.insert_split(id, direction, window, cx);
    }

    /// Put a pane that already exists beside the focused one, and focus it.
    fn insert_split(
        &mut self,
        id: PaneId,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.active_id();
        let tab = self.tab_mut();
        tab.layout.split(&target, direction, id);
        // A split while zoomed is a request to see both.
        tab.zoomed = None;
        if tab.broadcast
            && let Some(pane) = self.panes.get(&id)
        {
            pane.update(cx, |t, _| t.broadcast = true);
        }
        self.focus_pane(id, window, cx);
        self.save_workspaces(cx);
    }

    // --- Zoom, broadcast, and the other tmux reflexes ---

    /// Show the active pane at full tab size, or restore the layout.
    fn toggle_zoom(&mut self, cx: &mut Context<Self>) {
        let tab = self.tab_mut();
        if tab.zoomed.is_some() {
            tab.zoomed = None;
        } else if tab.layout.len() > 1 {
            tab.zoomed = Some(tab.active);
        } else {
            self.toast(
                ToastKind::Info,
                "zoom needs more than one pane in the tab".into(),
                cx,
            );
            return;
        }
        cx.notify();
    }

    fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        if self.tab().layout.len() < 2 {
            self.toast(
                ToastKind::Info,
                "broadcast needs more than one pane in the tab".into(),
                cx,
            );
            return;
        }
        let on = !self.tab().broadcast;
        let (wix, tix) = (self.active_ws, self.ws().active_tab);
        self.workspaces[wix].tabs[tix].broadcast = on;
        self.sync_tab_broadcast(wix, tix, cx);
        cx.notify();
    }

    /// Push the tab's broadcast flag down to its panes, switching it off
    /// first if the tab is down to one pane — nothing to broadcast to.
    fn sync_tab_broadcast(&mut self, wix: usize, tix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self
            .workspaces
            .get_mut(wix)
            .and_then(|w| w.tabs.get_mut(tix))
        else {
            return;
        };
        if tab.layout.len() < 2 {
            tab.broadcast = false;
        }
        let on = tab.broadcast;
        for id in tab.layout.leaves() {
            if let Some(pane) = self.panes.get(&id) {
                pane.update(cx, |t, _| t.broadcast = on);
            }
        }
    }

    /// Input from one pane, echoed to the others in its tab.
    fn broadcast_input(&mut self, from: PaneId, bytes: &[u8], cx: &mut Context<Self>) {
        let Some((wix, tix)) = self.locate_pane(from) else {
            return;
        };
        let tab = &self.workspaces[wix].tabs[tix];
        if !tab.broadcast {
            return;
        }
        for id in tab.layout.leaves() {
            if id != from
                && let Some(pane) = self.panes.get(&id)
            {
                pane.update(cx, |t, _| t.write_raw(bytes.to_vec()));
            }
        }
    }

    /// ctrl-w o: close every other pane in the tab, asking first when
    /// more than one would go.
    fn close_other_panes_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let others = self.tab().layout.len().saturating_sub(1);
        match others {
            0 => {}
            1 => self.close_other_panes(window, cx),
            n => self.open_confirm(
                format!("Close the other {n} panes in this tab?"),
                window,
                cx,
            ),
        }
    }

    fn close_other_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let keep = self.active_id();
        let others: Vec<PaneId> = self
            .tab()
            .layout
            .leaves()
            .into_iter()
            .filter(|id| *id != keep)
            .collect();
        if others.is_empty() {
            return;
        }
        for id in others {
            self.tab_mut().layout.remove(&id);
            self.drop_pane(id);
        }
        let tab = self.tab_mut();
        tab.zoomed = None;
        let (wix, tix) = (self.active_ws, self.ws().active_tab);
        self.sync_tab_broadcast(wix, tix, cx);
        self.focus_pane(keep, window, cx);
        self.save_workspaces(cx);
    }

    /// ctrl-w x: exchange the active pane with its neighbour in the split.
    fn swap_active_pane(&mut self, cx: &mut Context<Self>) {
        let id = self.active_id();
        if self.tab_mut().layout.swap_with_neighbour(&id) {
            self.save_workspaces(cx);
            cx.notify();
        }
    }

    /// Close a pane wherever it lives, cascading upward: the last pane closes
    /// its tab, the last tab closes its workspace. Returns false only when
    /// this was the last pane of the last tab of the last workspace — the
    /// caller decides whether that closes the window.
    fn close_pane(&mut self, id: PaneId, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((wix, tix)) = self.locate_pane(id) else {
            return true;
        };
        let tab = &mut self.workspaces[wix].tabs[tix];

        if tab.layout.len() > 1 {
            let before = tab.layout.leaves();
            tab.layout.remove(&id);
            if tab.active == id {
                let ix = before.iter().position(|l| *l == id).unwrap_or(0);
                let remaining = tab.layout.leaves();
                tab.active = remaining[ix.min(remaining.len() - 1)];
            }
            if tab.zoomed == Some(id) || tab.layout.len() < 2 {
                tab.zoomed = None;
            }
            self.drop_pane(id);
            self.sync_tab_broadcast(wix, tix, cx);
            if wix == self.active_ws && tix == self.ws().active_tab {
                let next = self.active_id();
                self.focus_pane(next, window, cx);
            }
            self.save_workspaces(cx);
            cx.notify();
            return true;
        }

        // Last pane in its tab: the tab goes with it.
        if self.workspaces[wix].tabs.len() > 1 {
            self.close_tab_at(wix, tix, window, cx);
            return true;
        }
        self.close_last_tab(wix, window, cx)
    }

    /// Close a workspace's only tab. `tabs.close_last` decides what is
    /// left: a fresh tab in the home directory, or nothing — the workspace
    /// goes too, and false says it was the last one (so the window should).
    fn close_last_tab(&mut self, wix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.config.tabs.close_last == CloseLastTab::NewTab {
            let cwd = home_dir().unwrap_or_else(|| PathBuf::from("/"));
            let id = self.create_pane(cwd, window, cx);
            self.workspaces[wix]
                .tabs
                .push(TabState::new(Node::Leaf(id), id));
            self.close_tab_at(wix, 0, window, cx);
            return true;
        }
        if self.workspaces.len() > 1 {
            self.remove_workspace_at(wix, window, cx);
            return true;
        }
        false
    }

    /// Remove a whole tab (all its panes). Callers guarantee the workspace
    /// keeps at least one tab.
    fn close_tab_at(
        &mut self,
        wix: usize,
        tix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active_tab = wix == self.active_ws && tix == self.workspaces[wix].active_tab;
        let tab = self.workspaces[wix].tabs.remove(tix);
        self.remember_closed_tab(&tab, cx);
        for pid in tab.layout.leaves() {
            self.drop_pane(pid);
        }
        let ws = &mut self.workspaces[wix];
        if ws.active_tab >= ws.tabs.len() {
            ws.active_tab = ws.tabs.len() - 1;
        } else if tix < ws.active_tab {
            ws.active_tab -= 1;
        }
        // A tab opened on top of another hands focus back to it. Found by
        // pane, so it survives tabs being moved or closed in the meantime.
        if was_active_tab
            && let Some(opener) = tab.opener
            && let Some(ix) = ws
                .tabs
                .iter()
                .position(|t| t.layout.leaves().contains(&opener))
        {
            ws.active_tab = ix;
        }
        if was_active_tab {
            let next = self.active_id();
            self.focus_pane(next, window, cx);
        }
        self.save_workspaces(cx);
        cx.notify();
    }

    /// Remove a workspace and everything in it. Deleting the only workspace
    /// swaps in a fresh default so the window never ends up empty.
    fn remove_workspace_at(&mut self, wix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let was_active = wix == self.active_ws;
        if self.workspaces.len() == 1 {
            let cwd = home_dir().unwrap_or_else(|| PathBuf::from("/"));
            let id = self.create_pane(cwd, window, cx);
            // The one it replaces doesn't hold its number.
            let name = default_ws_name(std::iter::empty());
            let fresh = Workspace {
                name,
                persist: false,
                tabs: vec![TabState::new(Node::Leaf(id), id)],
                active_tab: 0,
            };
            let old = std::mem::replace(&mut self.workspaces[0], fresh);
            for tab in &old.tabs {
                self.remember_closed_tab(tab, cx);
            }
            for pid in old.tabs.iter().flat_map(|t| t.layout.leaves()) {
                self.drop_pane(pid);
            }
            self.active_ws = 0;
            self.ws_selected = 0;
            self.focus_pane(id, window, cx);
        } else {
            let old = self.workspaces.remove(wix);
            for tab in &old.tabs {
                self.remember_closed_tab(tab, cx);
            }
            for pid in old.tabs.iter().flat_map(|t| t.layout.leaves()) {
                self.drop_pane(pid);
            }
            if self.active_ws >= self.workspaces.len() {
                self.active_ws = self.workspaces.len() - 1;
            } else if wix < self.active_ws {
                self.active_ws -= 1;
            }
            self.ws_selected = self.ws_selected.min(self.workspaces.len() - 1);
            if was_active {
                let next = self.active_id();
                self.focus_pane(next, window, cx);
            }
        }
        self.save_workspaces(cx);
        cx.notify();
    }

    fn render_pane_node(
        &self,
        node: &Node<PaneId>,
        path: &NodePath,
        accent: gpui::Hsla,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        match node {
            Node::Leaf(id) => {
                let Some(pane) = self.panes.get(id) else {
                    return div().into_any_element();
                };
                let focused = pane.focus_handle(cx).is_focused(window);
                let tab = self.tab();
                let many = tab.layout.len() > 1;
                // Only mark the active pane when there is a choice to make.
                let show_ring = focused && (many || self.drawer_visible);
                let flashing = self
                    .fail_flash
                    .get(id)
                    .is_some_and(|until| Instant::now() < *until);
                // A pane ssh'd into a host with a configured accent wears it
                // on its border, focused or not: "am I on prod?" at a glance.
                let ssh_accent = pane
                    .read(cx)
                    .ssh_host()
                    .and_then(|host| self.config.ssh.accent_for(host))
                    .and_then(parse_hex);
                let ring = if flashing {
                    self.theme.ansi[1]
                } else if tab.broadcast {
                    // Impossible to miss, on every pane that will receive input.
                    if focused {
                        self.theme.ansi[1]
                    } else {
                        blend(self.theme.ansi[1], self.theme.background, 0.4)
                    }
                } else if let Some(color) = ssh_accent {
                    if focused {
                        color
                    } else {
                        blend(color, self.theme.background, 0.35)
                    }
                } else if show_ring {
                    accent
                } else {
                    gpui::transparent_black()
                };
                // Dim the panes you aren't in by fading what they draw. The
                // background is a layer of its own underneath, so a dimmed
                // pane in a translucent window is no more solid than the
                // rest. (A shade laid over the pane would be.)
                let dim = self.config.window.inactive_pane_opacity.clamp(0.05, 1.0);
                let dimmed = many && tab.zoomed.is_none() && *id != tab.active && dim < 1.0;
                let background = translucent(self.theme.background, self.config.window.opacity);
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    .bg(background)
                    .border_1()
                    .border_color(ring)
                    .child(
                        div()
                            .size_full()
                            .when(dimmed, |d| d.opacity(dim))
                            .child(pane.clone()),
                    )
                    .into_any_element()
            }
            Node::Split {
                axis,
                children,
                ratios,
            } => {
                let horizontal = *axis == Axis::Horizontal;
                let mut container = div().size_full().flex().min_w_0().min_h_0();
                container = if horizontal {
                    container.flex_row()
                } else {
                    container.flex_col()
                };
                // A hairline between siblings; the focus ring stays the only
                // coloured edge, so the active pane still reads at a glance.
                let divider = blend(self.theme.foreground, self.theme.background, 0.72);
                for (ix, child) in children.iter().enumerate() {
                    if ix > 0 {
                        container =
                            container.child(self.render_divider(path, ix - 1, *axis, divider, cx));
                    }
                    let ratio = ratios
                        .get(ix)
                        .copied()
                        .unwrap_or(1.0 / children.len() as f32);
                    let mut child_path = path.clone();
                    child_path.push(ix);
                    // Flex weights rather than percentages: taffy shares out
                    // whatever is left after the 1px dividers in whole pixels,
                    // so N children never overflow by N-1 px or leave a gap.
                    let mut cell = div().min_w_0().min_h_0().overflow_hidden();
                    {
                        let style = cell.style();
                        style.flex_grow = Some(ratio.max(0.0001));
                        style.flex_shrink = Some(1.0);
                        style.flex_basis = Some(px(0.0).into());
                    }
                    container = container.child(cell.child(self.render_pane_node(
                        child,
                        &child_path,
                        accent,
                        window,
                        cx,
                    )));
                }
                container.into_any_element()
            }
        }
    }

    /// The 1px line between two siblings, with a wider invisible grab area
    /// drawn on top of both neighbours. `deferred` paints it after the panes
    /// so their hitboxes don't swallow the edge that overlaps them.
    fn render_divider(
        &self,
        path: &NodePath,
        divider: usize,
        axis: Axis,
        color: gpui::Hsla,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let horizontal = axis == Axis::Horizontal;
        // While a modal or a drag is up, the deferred grab areas would paint
        // above it; they aren't needed then anyway.
        let interactive = self.overlay.is_none()
            && self.divider_drag.is_none()
            && self.ws_context_menu.is_none()
            && self.app_menu.is_none();
        let path = path.clone();
        let line = div().flex_none().relative().bg(color);
        let line = if horizontal {
            line.w(px(1.0)).h_full()
        } else {
            line.h(px(1.0)).w_full()
        };
        line.when(interactive, |line| {
            let hit = div()
                .absolute()
                .cursor(if horizontal {
                    gpui::CursorStyle::ResizeLeftRight
                } else {
                    gpui::CursorStyle::ResizeUpDown
                })
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        cx.stop_propagation();
                        this.start_divider_drag(path.clone(), divider, axis, ev.position, cx);
                    }),
                );
            let hit = if horizontal {
                hit.top_0().bottom_0().left(px(-3.0)).w(px(7.0))
            } else {
                hit.left_0().right_0().top(px(-3.0)).h(px(7.0))
            };
            line.child(gpui::deferred(hit))
        })
    }

    fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        if !self.config.status_bar.enabled {
            return;
        }
        let Some(cwd) = self.active_pane().read(cx).cwd.clone() else {
            return;
        };
        let bg = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let status = bg.spawn(async move { read_git_status(&cwd) }).await;
            this.update(cx, |this, cx| {
                if this.git_status != status {
                    this.git_status = status;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_bounds_debounced(
        &mut self,
        bounds: gpui::Bounds<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.last_bounds = Some(bounds);
        if self.bounds_save_scheduled {
            return;
        }
        self.bounds_save_scheduled = true;
        let timer = cx
            .background_executor()
            .timer(std::time::Duration::from_millis(1000));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, _| {
                this.bounds_save_scheduled = false;
                this.write_window_state();
            })
            .ok();
        })
        .detach();
    }

    /// The width it was dragged to, as far as this window allows; else
    /// `tree.width`.
    fn drawer_width(&self, window: &Window) -> f32 {
        match self.drawer_width {
            Some(w) => clamp_drawer_width(w, f32::from(window.viewport_size().width)),
            None => self.config.tree.width,
        }
    }

    /// The window's bounds, then the drawer's width once it has been dragged.
    fn write_window_state(&self) {
        let (Some(b), Some(path)) = (self.last_bounds, window_state_path()) else {
            return;
        };
        let mut state = format!(
            "{} {} {} {}",
            f32::from(b.origin.x),
            f32::from(b.origin.y),
            f32::from(b.size.width),
            f32::from(b.size.height)
        );
        if let Some(width) = self.drawer_width {
            state.push_str(&format!(" {width}"));
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let _ = std::fs::write(path, state);
    }

    fn reload_config(&mut self, cx: &mut Context<Self>) {
        match config::reload() {
            Ok(new_config) => {
                if new_config == *self.config {
                    return;
                }
                let shell_or_prompt_changed = new_config.shell != self.config.shell
                    || new_config.prompt != self.config.prompt;
                let keymap_changed = new_config.keymap != self.config.keymap;
                // A new `tree.width` is a request for that width, dragged
                // or not.
                if new_config.tree.width != self.config.tree.width {
                    self.drawer_width = None;
                    self.write_window_state();
                }
                if self.config.crash_reports.enabled && !new_config.crash_reports.enabled {
                    crate::telemetry::disable();
                }
                self.config = Rc::new(new_config);
                self.run_startup_commands =
                    self.config.workspaces.run_startup_commands && !self.startup_skipped_at_launch;
                self.theme = Rc::new(Theme::resolve(&self.config.colors, self.dark_appearance));
                let config = self.config.clone();
                let theme = self.theme.clone();
                self.for_each_pane(cx, |t, cx| t.set_config(config.clone(), theme.clone(), cx));
                self.tree
                    .update(cx, |t, cx| t.set_config(config, theme, cx));
                let keymap_banner = if keymap_changed {
                    self.rebind_keys(cx)
                } else {
                    None
                };
                if let Some(message) = keymap_banner {
                    // Like a parse error: stays up until the next clean reload.
                    self.sticky_toast(message);
                } else {
                    self.toasts.retain(|t| !t.sticky);
                    if shell_or_prompt_changed {
                        self.toast(
                            ToastKind::Info,
                            "config reloaded — shell/prompt changes apply to new sessions".into(),
                            cx,
                        );
                    }
                }
            }
            Err(message) => {
                // Keep the previous config, show the error, keep running.
                // Sticky errors persist until the next successful reload.
                self.sticky_toast(message);
            }
        }
        cx.notify();
    }

    /// The system switched between light and dark. With `follow_system`
    /// on, re-resolve the theme the same way a config reload would. A theme
    /// preview in progress is left alone; it reverts or commits on its own.
    fn on_appearance_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = appearance_is_dark(window.appearance());
        if dark == self.dark_appearance {
            return;
        }
        self.dark_appearance = dark;
        if !self.config.colors.follow_system
            || matches!(self.overlay, Some(Overlay::ThemePicker(_)))
        {
            return;
        }
        let theme = Rc::new(Theme::resolve(&self.config.colors, dark));
        self.apply_theme(theme, cx);
    }

    /// Re-resolve the keymap and swap it in live. GPUI's keymap is
    /// app-global, so this also refreshes the menu bar's key equivalents.
    /// Returns a banner describing any entries that were skipped.
    fn rebind_keys(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let resolved = Rc::new(keymap::resolve(&self.config.keymap));
        cx.clear_key_bindings();
        cx.bind_keys(resolved.bindings());
        if cfg!(target_os = "macos") {
            cx.set_menus(crate::menus());
        }
        let banner = resolved.error_banner();
        self.keymap = resolved;
        banner
    }

    fn push_toast(
        &mut self,
        kind: ToastKind,
        message: String,
        opens_changelog: bool,
        sticky: bool,
    ) -> usize {
        let id = self.next_toast_id;
        self.next_toast_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            message,
            opens_changelog,
            sticky,
            asks_crash_reports: false,
        });
        id
    }

    /// Ask about crash reports, once per process (one window, not every
    /// window opened at startup) and only while the config hasn't answered.
    fn ask_about_crash_reports(&mut self) {
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let text = std::fs::read_to_string(config::config_path()).unwrap_or_default();
        if config::crash_reports_answered(&text) {
            return;
        }
        let id = self.push_toast(
            ToastKind::Info,
            "Send crash reports to the OmniPTY developer? A report is the stack trace, \
             version and OS, never anything shown in or typed into a terminal. Yes \
             also sends a start and stop ping each launch, so crash rates mean \
             something. Change your answer any time under [crash_reports] in the config."
                .into(),
            false,
            true,
        );
        if let Some(toast) = self.toasts.iter_mut().find(|t| t.id == id) {
            toast.asks_crash_reports = true;
        }
    }

    fn answer_crash_reports(&mut self, id: usize, enabled: bool, cx: &mut Context<Self>) {
        self.dismiss_toast(id, cx);
        match persist_crash_reports(enabled) {
            Ok(()) if enabled => self.toast(
                ToastKind::Info,
                "thanks — crash reports start at the next launch".into(),
                cx,
            ),
            Ok(()) => {}
            Err(e) => self.toast(ToastKind::Error, e, cx),
        }
    }

    /// A notice that times out on its own: 4s for info, 8s for errors.
    fn toast(&mut self, kind: ToastKind, message: String, cx: &mut Context<Self>) {
        let id = self.push_toast(kind, message, false, false);
        let secs = if kind == ToastKind::Error { 8 } else { 4 };
        self.expire_toast(id, secs, cx);
    }

    fn expire_toast(&mut self, id: usize, secs: u64, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(Duration::from_secs(secs));
        cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| this.dismiss_toast(id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// The one sticky slot: a config/keymap error that stays until the next
    /// clean reload replaces or clears it.
    fn sticky_toast(&mut self, message: String) {
        self.toasts.retain(|t| !t.sticky);
        self.push_toast(ToastKind::Error, message, false, true);
    }

    fn dismiss_toast(&mut self, id: usize, cx: &mut Context<Self>) {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.id != id);
        if self.toasts.len() != before {
            cx.notify();
        }
    }

    fn click_toast(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .toasts
            .iter()
            .any(|t| t.id == id && t.asks_crash_reports)
        {
            return;
        }
        let opens_changelog = self.toasts.iter().any(|t| t.id == id && t.opens_changelog);
        self.dismiss_toast(id, cx);
        if opens_changelog {
            self.open_changelog_tab(window, cx);
        }
    }

    /// The bundled changelog, rendered to ANSI and paged in a new tab.
    fn open_changelog_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, code)) = crate::changelog::write_rendered(self.tab_columns(cx)) else {
            self.toast(
                ToastKind::Error,
                "couldn't write the changelog to ~/.cache/omnipty".into(),
                cx,
            );
            return;
        };
        let cwd = self.new_tab_cwd(cx);
        self.open_pager(
            &path,
            code,
            crate::changelog::write_rendered,
            cwd,
            "what's new".into(),
            OpenIn::Tab,
            window,
            cx,
        );
    }

    /// Render a markdown file the way the changelog is and page it in a new
    /// tab or split (`markdown.preview_in`). The pane's cwd is the file's
    /// directory so relative links in it resolve on cmd-click.
    fn open_markdown_preview(
        &mut self,
        source: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let place = self.config.markdown.preview_in;
        let columns = match place {
            OpenIn::Tab => self.tab_columns(cx),
            OpenIn::Split => self.split_columns(cx),
        };
        let Some((path, code)) = crate::markdown::write_preview(source, columns) else {
            self.toast(
                ToastKind::Error,
                format!("couldn't render {} for preview", source.display()),
                cx,
            );
            return;
        };
        let cwd = source
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.new_tab_cwd(cx));
        let name = source.file_name().unwrap_or_default().to_string_lossy();
        let title = format!("preview: {name}");
        let source = source.to_path_buf();
        let render = move |cols| crate::markdown::write_preview(&source, cols);
        self.open_pager(&path, code, render, cwd, title, place, window, cx);
    }

    /// Show an image file in a pane of its own, a new tab or a split
    /// (`images.preview_in`), drawn by the pane itself; any key closes it.
    /// `size` is the picture's, in pixels.
    fn open_image_preview(
        &mut self,
        path: &Path,
        size: (u32, u32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let place = self.config.images.preview_in;
        // Whether it needs scaling down, judged by the focused pane: a new
        // tab's pane is that size or larger, a split's half as wide.
        let padding = self.config.window.padding;
        let scale = window.scale_factor();
        let share = match place {
            OpenIn::Tab => 1.0,
            OpenIn::Split => 0.5,
        };
        let fit = self.active_pane().read(cx).last_layout.is_none_or(|l| {
            let room = |extent: f32, pad: f32, cell: f32| (extent - pad * 2.0 - cell) * scale;
            let width = f32::from(l.bounds.size.width) * share;
            size.0 as f32 > room(width, padding.x, l.cell_width)
                || size.1 as f32 > room(l.bounds.size.height.into(), padding.y, l.cell_height)
        });
        let cwd = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.new_tab_cwd(cx));
        let title = path.file_name().unwrap_or_default().to_string_lossy();
        self.open_command_pane(
            image_preview_command(path, fit),
            cwd,
            Some(title.into_owned()),
            place,
            window,
            cx,
        );
    }

    /// Columns a terminal filling the current tab has: what a new tab's pager
    /// gets, measured across this tab's panes. 80 before the first layout.
    fn tab_columns(&self, cx: &Context<Self>) -> usize {
        let mut span: Option<(f32, f32, f32)> = None;
        for id in self.tab().layout.leaves() {
            if let Some(l) = self.panes[&id].read(cx).last_layout {
                let (left, right) = (f32::from(l.bounds.left()), f32::from(l.bounds.right()));
                span = Some(match span {
                    Some((a, b, cell)) => (a.min(left), b.max(right), cell),
                    None => (left, right, l.cell_width),
                });
            }
        }
        let pad = self.config.window.padding.x * 2.0;
        span.map_or(80, |(left, right, cell)| {
            ((right - left - pad) / cell) as usize
        })
    }

    /// Columns a pane split off the focused one will have: half of it, less
    /// a column for the divider. 40 before the first layout.
    fn split_columns(&self, cx: &Context<Self>) -> usize {
        let pad = self.config.window.padding.x * 2.0;
        self.active_pane().read(cx).last_layout.map_or(40, |l| {
            let half = f32::from(l.bounds.size.width) / 2.0;
            (((half - pad) / l.cell_width) as usize).saturating_sub(1)
        })
    }

    /// Page a rendered (ANSI) file with `less`. `code` is what the
    /// rendering's "copy" links copy; `render` redoes the rendering at a
    /// new column count when the pane is resized.
    #[allow(clippy::too_many_arguments)]
    fn open_pager(
        &mut self,
        path: &Path,
        code: Vec<String>,
        render: impl Fn(usize) -> Option<(PathBuf, Vec<String>)> + 'static,
        cwd: PathBuf,
        title: String,
        place: OpenIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let wrap = if less_wraps_words() {
            " --wordwrap"
        } else {
            ""
        };
        // -c: draw from the top. Left to itself less draws upward from the
        // bottom row, so a file shorter than the pane opens at the bottom.
        // --tilde: leave the rows past the end blank, not marked with `~`.
        let command = format!("less -Rc --tilde{wrap} {}", shell_quote(path));
        let id = self.open_command_pane(command, cwd, Some(title), place, window, cx);
        self.panes[&id].update(cx, |pane, _| {
            pane.preview_code = code;
            pane.preview_render = Some(Box::new(move |cols| render(cols).map(|(_, code)| code)));
        });
    }

    /// A pane of its own for `command`: a new tab in the current workspace,
    /// or a split to the right of the focused pane. It closes when the
    /// command exits. The title is a tab's; a split has nowhere to show one.
    fn open_command_pane(
        &mut self,
        command: String,
        cwd: PathBuf,
        title: Option<String>,
        place: OpenIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let opener = self.active_id();
        let id = self.create_pane(cwd, window, cx);
        let timeout = self.config.workspaces.startup_timeout.0;
        self.panes[&id].update(cx, |pane, cx| {
            pane.set_startup(
                Some(StartupCommand {
                    command,
                    on_exit: OnExit::Close,
                }),
                cx,
            );
            pane.arm_startup(timeout, cx);
        });
        match place {
            OpenIn::Tab => {
                let ws = self.ws_mut();
                ws.tabs.push(TabState {
                    title,
                    opener: Some(opener),
                    ..TabState::new(Node::Leaf(id), id)
                });
                ws.active_tab = ws.tabs.len() - 1;
                self.focus_pane(id, window, cx);
            }
            OpenIn::Split => self.insert_split(id, Direction::Right, window, cx),
        }
        id
    }

    /// Run `command` at the focused pane's prompt. A pane busy with a
    /// program of its own — the editor still showing the last file, a build
    /// — has no prompt to take it, so the command gets a pane of its own
    /// (`editor.open_in`).
    fn run_at_prompt(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let (busy, cwd) = {
            let pane = self.active_pane().read(cx);
            (pane.is_busy(), pane.cwd.clone())
        };
        if busy {
            let cwd = cwd.or_else(home_dir).unwrap_or_else(|| PathBuf::from("/"));
            let place = self.config.editor.open_in;
            self.open_command_pane(command, cwd, None, place, window, cx);
            return;
        }
        self.active_pane()
            .update(cx, |t, _| t.run_command(&command));
        self.focus_terminal(Some(window), cx);
    }

    /// Bottom-right stack, newest at the bottom, lifted above the status bar
    /// when that sits at the bottom. Clicking a toast dismisses it.
    fn render_toasts(&self, above_status_bar: bool, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        div()
            .absolute()
            .right(px(8.0))
            .bottom(px(if above_status_bar { 34.0 } else { 8.0 }))
            .flex()
            .flex_col()
            .items_end()
            .gap_1()
            .children(self.toasts.iter().map(|t| {
                let (bg, fg) = match t.kind {
                    ToastKind::Info => (
                        blend(theme.background, theme.foreground, 0.12),
                        theme.foreground,
                    ),
                    ToastKind::Error => (theme.ansi[1], theme.background),
                };
                let id = t.id;
                div()
                    .id(("toast", id))
                    .max_w(px(480.0))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .bg(bg)
                    .text_size(px(12.0))
                    .text_color(fg)
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            this.click_toast(id, window, cx)
                        }),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(t.message.clone())
                            .when(t.asks_crash_reports, |body| {
                                let button = |label: &'static str, enabled: bool| {
                                    div()
                                        .id((label, id))
                                        .px_2()
                                        .rounded_sm()
                                        .bg(blend(bg, fg, 0.15))
                                        .hover(|s| s.bg(blend(bg, fg, 0.25)))
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(
                                                move |this, _: &gpui::MouseDownEvent, _w, cx| {
                                                    cx.stop_propagation();
                                                    this.answer_crash_reports(id, enabled, cx);
                                                },
                                            ),
                                        )
                                        .child(label)
                                };
                                body.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap_2()
                                        .child(button("Send reports", true))
                                        .child(button("No thanks", false)),
                                )
                            }),
                    )
                    // Dismiss without following the toast's click action.
                    .child(
                        div()
                            .id(("toast-close", id))
                            .flex_none()
                            .text_color(blend(fg, bg, 0.4))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.dismiss_toast(id, cx);
                                }),
                            )
                            .child("×"),
                    )
            }))
    }

    fn on_tree_event(
        &mut self,
        _: &gpui::Entity<FileTree>,
        event: &TreeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TreeEvent::OpenFile(path) => {
                let path = path.clone();
                self.open_in_editor(&path, None, window, cx);
            }
            TreeEvent::PreviewMarkdown(path) => {
                let path = path.clone();
                self.open_markdown_preview(&path, window, cx);
            }
            TreeEvent::ChangedRoot(path) | TreeEvent::CdShell(path) => {
                let path = path.clone();
                self.active_pane()
                    .update(cx, |t, cx| t.request_cd(&path, cx));
            }
            TreeEvent::RootChanged(root) => {
                let root = root.clone();
                self.for_each_pane(cx, |t, _| t.tree_root = Some(root.clone()));
            }
            TreeEvent::InsertPath { path, absolute } => {
                let (path, absolute) = (path.clone(), *absolute);
                self.active_pane()
                    .update(cx, |t, cx| t.insert_path(&path, absolute, cx));
                self.focus_terminal(Some(window), cx);
            }
            TreeEvent::FocusTerminal => self.focus_terminal(Some(window), cx),
            TreeEvent::Prompt {
                prompt,
                title,
                hint,
                initial,
            } => {
                let action = PromptAction::Tree(prompt.clone());
                self.open_prompt(title.clone(), hint, initial.clone(), action, window, cx);
            }
        }
    }

    fn on_terminal_event(
        &mut self,
        emitter: &gpui::Entity<TerminalPane>,
        event: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalEvent::Exited(code) => {
                // Follow Terminal.app: a clean `exit` closes the pane, and the
                // window along with the last pane. A crash or non-zero status
                // keeps the overlay so the failure stays readable.
                if *code != Some(0) {
                    return;
                }
                let id = self
                    .panes
                    .iter()
                    .find(|(_, p)| p.entity_id() == emitter.entity_id())
                    .map(|(id, _)| *id);
                if let Some(id) = id
                    && !self.close_pane(id, window, cx)
                {
                    window.remove_window();
                }
            }
            TerminalEvent::StartupExited => {
                // `on_exit = close`: the same path a clean `exit` takes.
                if let Some(id) = self.pane_id_of(emitter)
                    && !self.close_pane(id, window, cx)
                {
                    window.remove_window();
                }
            }
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::Output => {
                // A background tab gets a dot; the active one is being
                // watched already.
                let Some(id) = self.pane_id_of(emitter) else {
                    return;
                };
                if let Some((wix, tix)) = self.locate_pane(id)
                    && !(wix == self.active_ws && tix == self.ws().active_tab)
                {
                    let tab = &mut self.workspaces[wix].tabs[tix];
                    if !tab.unread {
                        tab.unread = true;
                        cx.notify();
                    }
                }
            }
            TerminalEvent::CommandStarted => {
                self.start_ticker(cx);
                cx.notify();
            }
            TerminalEvent::CommandFinished {
                label,
                exit,
                duration,
            } => {
                let Some(id) = self.pane_id_of(emitter) else {
                    return;
                };
                let pane_focused =
                    window.is_window_active() && emitter.focus_handle(cx).is_focused(window);
                let finished = notifications::Finished {
                    duration: *duration,
                    exit: *exit,
                    pane_focused,
                };
                if notifications::should_notify(&self.config.notifications, finished) {
                    let route = self.route_for(id);
                    notifications::post(
                        "OmniPTY",
                        &notifications::command_summary(label, *exit, *duration),
                        Some(route),
                    );
                }
                // A failure somewhere you weren't looking: flash that pane's
                // ring so the eye lands on the right split.
                if exit.is_some_and(|e| e != 0) && !emitter.focus_handle(cx).is_focused(window) {
                    self.fail_flash
                        .insert(id, Instant::now() + Duration::from_millis(1500));
                    let timer = cx.background_executor().timer(Duration::from_millis(1600));
                    cx.spawn(async move |this, cx| {
                        timer.await;
                        this.update(cx, |this, cx| {
                            this.fail_flash.retain(|_, until| Instant::now() < *until);
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
                cx.notify();
            }
            TerminalEvent::Notify { title, body } => {
                let route = self.pane_id_of(emitter).map(|id| self.route_for(id));
                notifications::post(title.as_deref().unwrap_or("OmniPTY"), body, route);
            }
            TerminalEvent::OpenPath { path, line, col } => {
                let (path, at) = (path.clone(), line.map(|l| (l, *col)));
                self.open_in_editor(&path, at, window, cx);
            }
            TerminalEvent::RevealDir(dir) => {
                let dir = dir.clone();
                self.drawer_visible = true;
                self.tree.update(cx, |tree, cx| tree.set_root(dir, cx));
                cx.notify();
            }
            TerminalEvent::Input(bytes) => {
                if let Some(id) = self.pane_id_of(emitter) {
                    self.broadcast_input(id, bytes, cx);
                }
            }
            TerminalEvent::Notice(message) => {
                self.toast(ToastKind::Info, message.clone(), cx);
            }
            // Tab titles and the ssh chip follow the foreground process.
            TerminalEvent::ForegroundChanged => cx.notify(),
            TerminalEvent::CwdChanged(cwd) => {
                // Background panes change directory too; only the focused one
                // should move the tree or the status bar.
                let is_active = self
                    .panes
                    .get(&self.active_id())
                    .is_some_and(|p| p.entity_id() == emitter.entity_id());
                if is_active {
                    if self.config.tree.follow_cwd {
                        let cwd = cwd.clone();
                        self.tree.update(cx, |tree, cx| tree.set_root(cwd, cx));
                    }
                    self.refresh_git_status(cx);
                }
                // Any pane's cd changes what a persisted workspace should
                // restore to, focused or not.
                self.save_workspaces(cx);
            }
        }
    }

    fn pane_id_of(&self, pane: &gpui::Entity<TerminalPane>) -> Option<PaneId> {
        self.panes
            .iter()
            .find(|(_, p)| p.entity_id() == pane.entity_id())
            .map(|(id, _)| *id)
    }

    /// Allocate a click-routing key for a notification about `pane`.
    fn route_for(&mut self, pane: PaneId) -> notifications::RouteKey {
        let key = NEXT_ROUTE.fetch_add(1, Ordering::Relaxed);
        self.notification_routes.insert(key, pane);
        // Keys are cheap; keep the map from growing forever.
        if self.notification_routes.len() > 200 {
            let oldest = self.notification_routes.keys().copied().min();
            if let Some(k) = oldest {
                self.notification_routes.remove(&k);
            }
        }
        key
    }

    /// A notification was clicked: bring its pane to the front. Returns
    /// false when the key isn't ours (another window's, or long gone).
    pub fn on_notification_click(
        &mut self,
        key: notifications::RouteKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.notification_routes.remove(&key) else {
            return false;
        };
        let Some((wix, tix)) = self.locate_pane(id) else {
            return true;
        };
        self.active_ws = wix;
        self.ws_selected = wix;
        self.workspaces[wix].active_tab = tix;
        self.workspaces[wix].tabs[tix].active = id;
        cx.activate(true);
        window.activate_window();
        self.focus_pane(id, window, cx);
        true
    }

    /// Repaint once a second while any pane has a running command, so the
    /// status bar's elapsed time ticks. Stops itself when nothing runs.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                let timer = match this.update(cx, |_, cx| {
                    cx.background_executor().timer(Duration::from_secs(1))
                }) {
                    Ok(timer) => timer,
                    Err(_) => break,
                };
                timer.await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let running = this.panes.values().any(|p| p.read(cx).log.is_running());
                        if !running {
                            this.ticking = false;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    fn tree_focus(&self, cx: &Context<Self>) -> FocusHandle {
        self.tree.focus_handle(cx)
    }

    fn term_focus(&self, cx: &Context<Self>) -> FocusHandle {
        self.active_pane().focus_handle(cx)
    }

    fn focus_tree(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.drawer_visible = true;
        if let Some(window) = window {
            window.focus(&self.tree_focus(cx));
        }
        cx.notify();
    }

    fn focus_terminal(&mut self, window: Option<&mut Window>, cx: &mut Context<Self>) {
        if let Some(window) = window {
            window.focus(&self.term_focus(cx));
        }
        cx.notify();
    }

    /// The shell that will run anything OmniPTY types at a prompt.
    fn shell_program(&self) -> String {
        crate::terminal::session::resolve_shell(self.config.shell.program.as_deref())
    }

    // --- Overlays: theme picker and command palette ---

    fn current_focus_target(&self, window: &Window, cx: &Context<Self>) -> FocusTarget {
        if self.tree_focus(cx).is_focused(window) {
            FocusTarget::Tree
        } else if self.ws_focus.is_focused(window) {
            FocusTarget::Workspaces
        } else {
            FocusTarget::Pane(self.active_id())
        }
    }

    fn restore_focus(&mut self, target: FocusTarget, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            FocusTarget::Tree if self.drawer_visible => self.focus_tree(Some(window), cx),
            FocusTarget::Workspaces if self.drawer_visible => {
                self.focus_workspaces_panel(window, cx)
            }
            FocusTarget::Pane(id) if self.panes.contains_key(&id) => {
                if let Some(pane) = self.panes.get(&id) {
                    window.focus(&pane.focus_handle(cx));
                }
                cx.notify();
            }
            _ => self.focus_terminal(Some(window), cx),
        }
    }

    /// Dismiss whichever overlay is open and hand focus back to where it was.
    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(overlay) = self.overlay.take() else {
            return;
        };
        let target = match &overlay {
            Overlay::Palette(p) => p.return_focus,
            Overlay::ThemePicker(t) => t.return_focus,
            Overlay::History(h) => h.return_focus,
            Overlay::FileFinder(f) => f.return_focus,
            Overlay::Prompt(p) => p.return_focus,
            Overlay::Confirm(c) => c.return_focus,
            Overlay::StartupCommand(s) => s.return_focus,
            Overlay::StartupEditor(e) => e.return_focus,
            Overlay::About(a) => a.return_focus,
        };
        self.restore_focus(target, window, cx);
        cx.notify();
    }

    fn overlay_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::ThemePicker(_)) => self.picker_move(delta, cx),
            Some(Overlay::Palette(_)) => self.palette_move(delta, cx),
            Some(Overlay::History(_)) => self.history_move(delta, cx),
            Some(Overlay::FileFinder(_)) => self.finder_move(delta, cx),
            Some(Overlay::StartupEditor(_)) => self.startup_editor_move(delta, cx),
            Some(
                Overlay::Prompt(_)
                | Overlay::Confirm(_)
                | Overlay::StartupCommand(_)
                | Overlay::About(_),
            )
            | None => {}
        }
    }

    fn overlay_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::ThemePicker(_)) => self.picker_confirm(window, cx),
            Some(Overlay::Palette(_)) => self.palette_confirm(window, cx),
            Some(Overlay::History(_)) => self.history_confirm(false, window, cx),
            Some(Overlay::FileFinder(_)) => self.finder_confirm(FinderAction::Open, window, cx),
            Some(Overlay::Prompt(_)) => self.prompt_confirm(window, cx),
            Some(Overlay::Confirm(_)) => self.confirm_run(window, cx),
            Some(Overlay::StartupCommand(_)) => self.startup_command_confirm(window, cx),
            Some(Overlay::StartupEditor(_)) => self.startup_editor_confirm(window, cx),
            Some(Overlay::About(_)) => self.close_overlay(window, cx),
            None => {}
        }
    }

    /// cmd-enter: the overlay's second verb. The history overlay runs the
    /// command instead of inserting it; elsewhere it's a plain confirm.
    fn overlay_confirm_alt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::History(_)) => self.history_confirm(true, window, cx),
            Some(Overlay::FileFinder(_)) => self.finder_confirm(FinderAction::Insert, window, cx),
            _ => self.overlay_confirm(window, cx),
        }
    }

    /// "⏎ open · ⌘⏎ insert path · " for an overlay's footer, from the keys
    /// the actions are bound to right now — a global hotkey elsewhere on the
    /// system can make a default unusable, and the hint should follow the
    /// rebind. An unbound action drops out.
    fn overlay_hints(&self, actions: &[(&str, &str)]) -> String {
        actions
            .iter()
            .filter_map(|(id, label)| {
                let keys = pretty_keys(&self.keymap.display_for(id)?.keys);
                Some(format!("{keys} {label} · "))
            })
            .collect()
    }

    /// alt-enter: reveal in the tree, where that means something.
    fn overlay_confirm_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::FileFinder(_)) => self.finder_confirm(FinderAction::Reveal, window, cx),
            _ => self.overlay_confirm(window, cx),
        }
    }

    fn overlay_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::ThemePicker(_)) => self.picker_cancel(window, cx),
            Some(
                Overlay::Palette(_)
                | Overlay::History(_)
                | Overlay::FileFinder(_)
                | Overlay::Prompt(_)
                | Overlay::Confirm(_)
                | Overlay::StartupCommand(_)
                | Overlay::StartupEditor(_)
                | Overlay::About(_),
            ) => self.close_overlay(window, cx),
            None => {}
        }
    }

    /// Text input shared by the overlays that have one.
    fn overlay_query_mut(&mut self) -> Option<&mut LineEdit> {
        match &mut self.overlay {
            Some(Overlay::Palette(p)) => Some(&mut p.query),
            Some(Overlay::History(h)) => Some(&mut h.query),
            Some(Overlay::FileFinder(f)) => Some(&mut f.query),
            Some(Overlay::Prompt(p)) => Some(&mut p.buffer),
            Some(Overlay::StartupCommand(s)) => Some(&mut s.buffer),
            Some(Overlay::StartupEditor(e)) => e.rows.get_mut(e.selected).map(|r| &mut r.command),
            _ => None,
        }
    }

    // --- Startup commands ---

    /// Ask what one pane should run when its workspace is restored.
    /// Prefilled with the current command, or — the part that makes this
    /// feel like remembering rather than form-filling — the last command
    /// that ran in the pane.
    fn open_startup_command(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entity) = self.panes.get(&pane) else {
            return;
        };
        let (current, cwd, last) = {
            let p = entity.read(cx);
            let last = p.log.entries().rev().find_map(|c| c.text.clone());
            (p.startup.clone(), p.cwd.clone(), last)
        };
        let (buffer, on_exit) = match current {
            Some(s) => (s.command, s.on_exit),
            None => (last.unwrap_or_default(), OnExit::default()),
        };
        let buffer = LineEdit::new(buffer);
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::StartupCommand(StartupCommandState {
            pane,
            buffer,
            on_exit,
            cwd,
            return_focus,
        }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn startup_command_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::StartupCommand(s)) = &self.overlay else {
            return;
        };
        let (pane, command, on_exit) = (s.pane, s.buffer.text.trim().to_string(), s.on_exit);
        let startup = (!command.is_empty()).then_some(StartupCommand { command, on_exit });
        if let Some(entity) = self.panes.get(&pane) {
            entity.update(cx, |p, cx| p.set_startup(startup.clone(), cx));
        }
        self.close_overlay(window, cx);
        self.after_startup_edit(
            self.locate_pane(pane).map(|(w, _)| w),
            startup.is_some(),
            cx,
        );
    }

    /// Save, and nudge if the workspace isn't pinned — a startup command on
    /// a temporary workspace is forgotten at quit, which is rarely the
    /// intent.
    fn after_startup_edit(&mut self, wix: Option<usize>, any_set: bool, cx: &mut Context<Self>) {
        self.save_workspaces(cx);
        if any_set
            && let Some(ws) = wix.and_then(|w| self.workspaces.get(w))
            && !ws.persist
        {
            self.toast(ToastKind::Info,
                format!("startup command set — pin \"{}\" (p in the workspaces panel) to keep it across restarts", ws.name),
                cx,
            );
        }
        cx.notify();
    }

    /// Every pane in a workspace with its command, editable in one place.
    fn open_startup_editor(&mut self, wix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspaces.get(wix) else {
            return;
        };
        let home = home_dir();
        let mut rows = Vec::new();
        for (tix, tab) in ws.tabs.iter().enumerate() {
            for (pix, id) in tab.layout.leaves().into_iter().enumerate() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let p = pane.read(cx);
                let dir = p
                    .cwd
                    .as_deref()
                    .map(|d| pretty_path(d, home.as_deref()))
                    .unwrap_or_default();
                let tab_name = tab
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("tab {}", tix + 1));
                let label = if tab.layout.len() > 1 {
                    format!("{tab_name} · pane {} — {dir}", pix + 1)
                } else {
                    format!("{tab_name} — {dir}")
                };
                let (command, on_exit) = match &p.startup {
                    Some(s) => (s.command.clone(), s.on_exit),
                    None => (String::new(), OnExit::default()),
                };
                rows.push(StartupRow {
                    pane: id,
                    label,
                    command: LineEdit::new(command),
                    on_exit,
                });
            }
        }
        if rows.is_empty() {
            return;
        }
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::StartupEditor(StartupEditorState {
            ws: wix,
            name: ws.name.clone(),
            rows,
            selected: 0,
            return_focus,
        }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn startup_editor_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Overlay::StartupEditor(e)) = &mut self.overlay else {
            return;
        };
        let n = e.rows.len() as isize;
        if n > 0 {
            e.selected = (e.selected as isize + delta).rem_euclid(n) as usize;
            cx.notify();
        }
    }

    fn startup_editor_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::StartupEditor(e)) = &self.overlay else {
            return;
        };
        let wix = e.ws;
        let edits: Vec<(PaneId, Option<StartupCommand>)> = e
            .rows
            .iter()
            .map(|r| {
                let command = r.command.text.trim().to_string();
                (
                    r.pane,
                    (!command.is_empty()).then_some(StartupCommand {
                        command,
                        on_exit: r.on_exit,
                    }),
                )
            })
            .collect();
        let any_set = edits.iter().any(|(_, s)| s.is_some());
        for (id, startup) in edits {
            if let Some(entity) = self.panes.get(&id) {
                entity.update(cx, |p, cx| p.set_startup(startup, cx));
            }
        }
        self.close_overlay(window, cx);
        self.after_startup_edit(Some(wix), any_set, cx);
    }

    /// `tab` in either startup overlay cycles the selected row's on-exit
    /// behaviour.
    fn startup_cycle_on_exit(&mut self, cx: &mut Context<Self>) {
        match &mut self.overlay {
            Some(Overlay::StartupCommand(s)) => s.on_exit = s.on_exit.next(),
            Some(Overlay::StartupEditor(e)) => {
                if let Some(row) = e.rows.get_mut(e.selected) {
                    row.on_exit = row.on_exit.next();
                }
            }
            _ => return,
        }
        cx.notify();
    }

    /// The `on exit: shell` chip shared by both startup overlays.
    fn on_exit_chip(&self, on_exit: OnExit, lit: bool) -> gpui::Div {
        let theme = &self.theme;
        let dim = blend(theme.foreground, theme.background, 0.45);
        div()
            .flex_none()
            .px_1p5()
            .rounded_sm()
            .text_size(px(11.0))
            .bg(theme.ansi[0])
            .text_color(if lit { theme.foreground } else { dim })
            .child(format!("on exit: {}", on_exit.label()))
    }

    fn render_startup_command_body(
        &self,
        s: &StartupCommandState,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let home = home_dir();
        let where_ = s
            .cwd
            .as_deref()
            .map(|d| pretty_path(d, home.as_deref()))
            .unwrap_or_default();
        let header = div()
            .flex_none()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(blend(theme.foreground, theme.background, 0.3))
                    .child("startup command"),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child(where_),
            );
        let input = div()
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(accent).child("▸"))
            .child(self.render_line_edit(&s.buffer, "nothing — just a shell", cx))
            .child(self.on_exit_chip(s.on_exit, true));
        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .text_size(px(11.0))
            .text_color(dim)
            .child(
                "⏎ save · empty clears · tab cycles on-exit (shell / close / restart) · esc cancel",
            );
        div()
            .flex()
            .flex_col()
            .child(header)
            .child(input)
            .child(footer)
    }

    fn render_startup_editor_body(&self, e: &StartupEditorState, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let header = div()
            .flex_none()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(border)
            .text_color(blend(theme.foreground, theme.background, 0.3))
            .child(format!("startup commands — {}", e.name));
        let mut list = div().flex().flex_col().p_1().gap(px(1.0));
        for (ix, row) in e.rows.iter().enumerate() {
            let is_selected = ix == e.selected;
            list = list.child(
                div()
                    .id(("startup-row", ix))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    // Fainter than elsewhere: this row holds the input, and
                    // selected text in it is drawn in the full colour.
                    .when(is_selected, |d| d.bg(theme.selection_bg.opacity(0.4)))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, _w, cx| {
                            if let Some(Overlay::StartupEditor(e)) = &mut this.overlay {
                                e.selected = ix;
                                cx.notify();
                            }
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(row.label.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(if row.command.text.is_empty() {
                                        dim
                                    } else {
                                        accent
                                    })
                                    .child("▸"),
                            )
                            .child(match (row.command.text.is_empty(), is_selected) {
                                (_, true) => self.render_line_edit(
                                    &row.command,
                                    "nothing — just a shell",
                                    cx,
                                ),
                                (true, false) => {
                                    div().flex_1().text_color(dim).child("just a shell")
                                }
                                (false, false) => div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_normal()
                                    .child(row.command.text.clone()),
                            })
                            .when(!row.command.text.is_empty() || is_selected, |d| {
                                d.child(self.on_exit_chip(row.on_exit, is_selected))
                            }),
                    ),
            );
        }
        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(border)
            .text_size(px(11.0))
            .text_color(dim)
            .child("↑↓ pane · type to edit · tab cycles on-exit · ⏎ save all · esc cancel");
        div()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .child(footer)
    }

    // --- Tab rename and confirmations ---

    fn open_tab_rename(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.ws().tabs.get(tab) else {
            return;
        };
        let buffer = LineEdit::new(t.title.clone().unwrap_or_default());
        self.open_prompt(
            "Rename tab".into(),
            "empty name restores the automatic title",
            buffer,
            PromptAction::TabRename(tab),
            window,
            cx,
        );
    }

    fn open_ws_rename(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspaces.get(ix) else {
            return;
        };
        let (title, buffer) = (
            format!("Rename {}", ws.name),
            LineEdit::new(ws.name.clone()),
        );
        self.open_prompt(title, "", buffer, PromptAction::WsRename(ix), window, cx);
    }

    /// The prompt modal: one line of text, titled with what it's for.
    fn open_prompt(
        &mut self,
        title: String,
        hint: &'static str,
        buffer: LineEdit,
        action: PromptAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::Prompt(PromptState {
            title,
            hint,
            buffer,
            action,
            return_focus,
        }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    /// Hand the text to whatever asked for it. Only a tab's name may be
    /// empty: that goes back to the automatic title.
    fn prompt_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.overlay, Some(Overlay::Prompt(_))) {
            return;
        }
        let Some(Overlay::Prompt(p)) = self.overlay.take() else {
            return;
        };
        self.restore_focus(p.return_focus, window, cx);
        let text = p.buffer.text.trim().to_string();
        match p.action {
            PromptAction::TabRename(tab) => {
                if let Some(t) = self.ws_mut().tabs.get_mut(tab) {
                    t.title = (!text.is_empty()).then_some(text);
                }
                self.save_workspaces(cx);
            }
            _ if text.is_empty() => {}
            PromptAction::Tree(prompt) => {
                self.tree
                    .update(cx, |tree, cx| tree.apply_prompt(prompt, text, cx));
            }
            PromptAction::WsAdd => {
                self.new_workspace(window, cx);
                self.ws_mut().name = text;
                self.save_workspaces(cx);
            }
            PromptAction::WsRename(ix) => {
                if let Some(ws) = self.workspaces.get_mut(ix) {
                    ws.name = text;
                    self.save_workspaces(cx);
                }
            }
        }
        cx.notify();
    }

    /// The only confirmable action today is close-other-panes; `confirm_run` calls it directly.
    fn open_confirm(&mut self, message: String, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::Confirm(ConfirmState {
            message,
            return_focus,
        }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    // --- About ---

    /// Icon, version, website. The window-level answer to `About`; with no
    /// window open (macOS menu bar) the app-level handler opens the site.
    fn open_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay.is_some() {
            self.close_overlay(window, cx);
        }
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::About(AboutState { return_focus }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn render_about_body(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let site = crate::WEBSITE_URL.trim_start_matches("https://");
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_6()
                    .pt_6()
                    .pb_5()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .child(gpui::img(self.app_icon.clone()).size(px(96.0)).mb_3())
                    .child(
                        div()
                            .text_size(px(20.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("OmniPTY"),
                    )
                    .child(
                        div()
                            .text_color(dim)
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        div()
                            .mt_2()
                            .text_size(px(12.0))
                            .text_color(dim)
                            .child("A native terminal for macOS and Linux"),
                    )
                    .child(
                        div()
                            .id("about-website")
                            .mt_1()
                            .text_size(px(12.0))
                            .text_color(accent)
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.close_overlay(window, cx);
                                    cx.open_url(crate::WEBSITE_URL);
                                }),
                            )
                            .child(site),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(border)
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("⏎ / esc close"),
            )
    }

    fn confirm_run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.overlay, Some(Overlay::Confirm(_))) {
            return;
        }
        self.close_overlay(window, cx);
        self.close_other_panes(window, cx);
    }

    /// A one-line input for the overlay's text field.
    fn render_line_edit(
        &self,
        edit: &LineEdit,
        placeholder: &'static str,
        cx: &Context<Self>,
    ) -> gpui::Div {
        line_edit::render(edit, placeholder, &self.theme, cx, Self::overlay_query_mut)
    }

    fn render_prompt_body(&self, p: &PromptState, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let title = div()
            .flex_none()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(border)
            .text_color(blend(theme.foreground, theme.background, 0.3))
            .child(p.title.clone());
        let input = div()
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(theme.ansi[4]).child("▸"))
            .child(self.render_line_edit(&p.buffer, "", cx));
        let mut keys = String::from("⏎ confirm · esc cancel");
        if !p.hint.is_empty() {
            keys = format!("{keys} · {}", p.hint);
        }
        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .text_size(px(11.0))
            .text_color(dim)
            .child(keys);
        div()
            .flex()
            .flex_col()
            .child(title)
            .child(input)
            .child(footer)
    }

    fn render_confirm_body(&self, c: &ConfirmState) -> gpui::Div {
        let theme = &self.theme;
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(border)
                    .child(c.message.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child("y / ⏎ confirm · n / esc cancel"),
            )
    }

    // --- Theme picker ---

    fn current_preset(&self) -> String {
        self.theme.preset.to_string()
    }

    fn open_theme_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Overlay::Palette(_)) = &self.overlay {
            self.close_overlay(window, cx);
        }
        let current = self.current_preset();
        let selected = config::theme::PRESET_NAMES
            .iter()
            .position(|n| *n == current)
            .unwrap_or(0);
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::ThemePicker(ThemePicker {
            selected,
            scroll: (selected + 1).saturating_sub(PALETTE_ROWS),
            original: self.theme.clone(),
            return_focus,
        }));
        window.focus(&self.picker_focus);
        cx.notify();
    }

    /// Swap the live theme everywhere without touching the config.
    fn apply_theme(&mut self, theme: Rc<Theme>, cx: &mut Context<Self>) {
        self.theme = theme.clone();
        let config = self.config.clone();
        self.for_each_pane(cx, |t, cx| t.set_config(config.clone(), theme.clone(), cx));
        self.tree
            .update(cx, |t, cx| t.set_config(config, theme, cx));
        cx.notify();
    }

    /// Preview the selected preset. Pure preset palette: picking a theme
    /// means "I want this theme", so explicit color overrides (including the
    /// fully-pinned [colors] block older generated configs carry) don't apply.
    fn preview_selected(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::ThemePicker(picker)) = &self.overlay else {
            return;
        };
        let name = config::theme::PRESET_NAMES[picker.selected];
        let colors = crate::config::schema::ColorsConfig {
            preset: Some(name.to_string()),
            ..Default::default()
        };
        self.apply_theme(Rc::new(Theme::from_config(&colors)), cx);
    }

    fn picker_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = config::theme::PRESET_NAMES.len() as isize;
        if let Some(Overlay::ThemePicker(picker)) = &mut self.overlay {
            picker.selected = (picker.selected as isize + delta).rem_euclid(count) as usize;
            if picker.selected < picker.scroll {
                picker.scroll = picker.selected;
            } else if picker.selected >= picker.scroll + PALETTE_ROWS {
                picker.scroll = picker.selected + 1 - PALETTE_ROWS;
            }
            self.preview_selected(cx);
        }
    }

    fn picker_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::ThemePicker(picker)) = &self.overlay else {
            return;
        };
        let name = config::theme::PRESET_NAMES[picker.selected].to_string();
        // Update in-memory config first so the file-watcher reload no-ops.
        // Committing a preset replaces the whole [colors] block — explicit
        // overrides would silently defeat the theme switch otherwise. With
        // follow_system on, the pick is for the current appearance only:
        // set that variant and keep the rest of the block.
        let mut config = (*self.config).clone();
        let variant = if config.colors.follow_system {
            if self.dark_appearance {
                config.colors.preset_dark = Some(name.clone());
                Some("preset_dark")
            } else {
                config.colors.preset_light = Some(name.clone());
                Some("preset_light")
            }
        } else {
            config.colors = ColorsConfig {
                preset: Some(name.clone()),
                ..Default::default()
            };
            None
        };
        self.config = Rc::new(config);
        if let Err(e) = persist_preset(&name, variant) {
            self.sticky_toast(e);
        }
        self.preview_selected(cx);
        self.close_overlay(window, cx);
    }

    fn picker_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Overlay::ThemePicker(picker)) = &self.overlay {
            let original = picker.original.clone();
            self.apply_theme(original, cx);
        }
        self.close_overlay(window, cx);
    }

    fn render_theme_list(&self, picker: &ThemePicker, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let mut list = div().flex().flex_col().p_1().gap(px(1.0));
        let end = (picker.scroll + PALETTE_ROWS).min(config::theme::PRESET_NAMES.len());
        for (ix, name) in config::theme::PRESET_NAMES
            .iter()
            .enumerate()
            .take(end)
            .skip(picker.scroll)
        {
            let preset_theme = Theme::from_config(&crate::config::schema::ColorsConfig {
                preset: Some(name.to_string()),
                ..Default::default()
            });
            let is_selected = ix == picker.selected;
            let mut swatches = div().flex().flex_row().gap(px(2.0)).items_center();
            swatches = swatches.child(
                div()
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded_sm()
                    .bg(preset_theme.background)
                    .border_1()
                    .border_color(preset_theme.ansi[8]),
            );
            for i in 1..7 {
                swatches = swatches.child(
                    div()
                        .w(px(8.0))
                        .h(px(14.0))
                        .rounded_sm()
                        .bg(preset_theme.ansi[i]),
                );
            }
            list = list.child(
                div()
                    .id(ix)
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .when(is_selected, |d| d.bg(theme.selection_bg))
                    .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _w, cx| {
                        if let Some(Overlay::ThemePicker(p)) = &mut this.overlay
                            && p.selected != ix
                        {
                            p.selected = ix;
                            this.preview_selected(cx);
                        }
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            if let Some(Overlay::ThemePicker(p)) = &mut this.overlay {
                                p.selected = ix;
                            }
                            this.picker_confirm(window, cx);
                        }),
                    )
                    .child(div().flex_1().child(*name))
                    .child(swatches),
            );
        }
        list
    }

    // --- Command palette ---

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::Palette(_)) => self.close_overlay(window, cx),
            Some(Overlay::ThemePicker(_)) => {
                self.picker_cancel(window, cx);
                self.open_palette(window, cx);
            }
            Some(_) => {
                self.close_overlay(window, cx);
                self.open_palette(window, cx);
            }
            None => self.open_palette(window, cx),
        }
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::Palette(PaletteState {
            query: LineEdit::default(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            return_focus,
        }));
        self.palette_refresh();
        window.focus(&self.picker_focus);
        cx.notify();
    }

    /// Actions the palette offers right now. Tree actions need the drawer
    /// on screen to have anything to act on. Workspace actions don't: running
    /// one focuses the panel, which opens the drawer with the active
    /// workspace selected. Overlay navigation is meaningless from inside an
    /// overlay.
    fn palette_candidates(&self) -> impl Iterator<Item = &'static ActionMeta> {
        let drawer = self.drawer_visible;
        registry::all().iter().filter(move |m| {
            m.id != "app::palette"
                // The ☰ menu is the Linux stand-in for the menu bar; macOS
                // has the real thing.
                && (m.id != "app::menu" || cfg!(target_os = "linux"))
                && match m.context {
                    ActionContext::Root | ActionContext::Workspaces => true,
                    ActionContext::FileTree => drawer,
                    ActionContext::Overlay => false,
                }
        })
    }

    fn palette_refresh(&mut self) {
        let Some(Overlay::Palette(p)) = &self.overlay else {
            return;
        };
        let query = p.query.text.clone();
        let recent: Vec<&str> = self.palette_recent.iter().copied().collect();
        let keymap = self.keymap.clone();
        let items = palette::build_items(&query, self.palette_candidates(), &recent, |id| {
            keymap.display_for(id).map(|e| pretty_keys(&e.keys))
        });
        let Some(Overlay::Palette(p)) = &mut self.overlay else {
            return;
        };
        p.matches = items;
        p.selected = 0;
        p.scroll = 0;
    }

    fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Overlay::Palette(p)) = &mut self.overlay else {
            return;
        };
        let n = p.matches.len();
        if n == 0 {
            return;
        }
        p.selected = (p.selected as isize + delta).rem_euclid(n as isize) as usize;
        if p.selected < p.scroll {
            p.scroll = p.selected;
        } else if p.selected >= p.scroll + PALETTE_ROWS {
            p.scroll = p.selected + 1 - PALETTE_ROWS;
        }
        cx.notify();
    }

    /// Run the selected command. Focus goes back first — to the context the
    /// action belongs to, or to wherever it was — so the action dispatches
    /// into a live element rather than the overlay we're tearing down.
    fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Palette(p)) = &self.overlay else {
            return;
        };
        let Some(item) = p.matches.get(p.selected) else {
            return;
        };
        let Some(meta) = registry::by_id(item.action_id) else {
            return;
        };
        let return_focus = p.return_focus;
        self.overlay = None;

        self.palette_recent.retain(|id| *id != meta.id);
        self.palette_recent.push_front(meta.id);
        self.palette_recent.truncate(20);

        match meta.context {
            ActionContext::FileTree => self.focus_tree(Some(window), cx),
            ActionContext::Workspaces => self.focus_workspaces_panel(window, cx),
            _ => self.restore_focus(return_focus, window, cx),
        }
        window.dispatch_action((meta.build)(), cx);
        cx.notify();
    }

    fn on_overlay_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &event.keystroke;
        if let Some(Overlay::Confirm(_)) = &self.overlay {
            match ks.key.as_str() {
                "y" => self.confirm_run(window, cx),
                "n" => self.close_overlay(window, cx),
                _ => {}
            }
            cx.stop_propagation();
            return;
        }
        if ks.key == "tab"
            && matches!(
                self.overlay,
                Some(Overlay::StartupCommand(_) | Overlay::StartupEditor(_))
            )
        {
            self.startup_cycle_on_exit(cx);
            cx.stop_propagation();
            return;
        }
        // Cut has no action of its own; copy, paste and select-all arrive as
        // the terminal's actions (see `render_overlay`).
        let m = ks.modifiers;
        if ks.key == "x" && (m.platform || (m.control && m.shift)) {
            self.overlay_copy(cx);
            if let Some(q) = self.overlay_query_mut() {
                q.insert("");
            }
        } else if !self.overlay_query_mut().is_some_and(|q| q.handle(ks)) {
            return;
        }
        cx.stop_propagation();
        self.overlay_query_changed(cx);
    }

    /// Re-run whatever the overlay's text field filters, after an edit.
    fn overlay_query_changed(&mut self, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::Palette(_)) => self.palette_refresh(),
            Some(Overlay::History(_)) => self.history_refresh(cx),
            Some(Overlay::FileFinder(_)) => self.finder_refresh(),
            _ => {}
        }
        cx.notify();
    }

    fn overlay_copy(&mut self, cx: &mut Context<Self>) {
        let selected = self.overlay_query_mut().and_then(|q| q.selected_text());
        if let Some(text) = selected.map(str::to_string) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    fn overlay_paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Some(q) = self.overlay_query_mut() {
            q.insert(&text);
            self.overlay_query_changed(cx);
        }
    }

    // --- File finder (cmd-p) ---

    fn toggle_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::FileFinder(_)) => self.close_overlay(window, cx),
            Some(Overlay::ThemePicker(_)) => {
                self.picker_cancel(window, cx);
                self.open_finder(window, cx);
            }
            Some(_) => {
                self.close_overlay(window, cx);
                self.open_finder(window, cx);
            }
            None => self.open_finder(window, cx),
        }
    }

    fn open_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.current_focus_target(window, cx);
        self.overlay = Some(Overlay::FileFinder(FinderState {
            query: LineEdit::default(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            return_focus,
        }));
        self.ensure_finder_index(cx);
        self.finder_refresh();
        window.focus(&self.picker_focus);
        cx.notify();
    }

    /// Walk the tree root on the background pool unless a fresh index for
    /// it already exists. Uses the tree's own gitignore/hidden settings so
    /// results match what the drawer shows.
    fn ensure_finder_index(&mut self, cx: &mut Context<Self>) {
        let root = self.tree.read(cx).root.clone();
        let fresh = self
            .finder_index
            .as_ref()
            .is_some_and(|ix| ix.root == root && ix.built.elapsed() < FINDER_INDEX_TTL);
        if fresh || self.finder_indexing {
            return;
        }
        self.finder_indexing = true;
        let respect_gitignore = self.config.tree.respect_gitignore;
        let show_hidden = self.config.tree.show_hidden;
        let bg = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let walk_root = root.clone();
            let (entries, truncated) = bg
                .spawn(async move { walk_files(&walk_root, respect_gitignore, show_hidden) })
                .await;
            this.update(cx, |this, cx| {
                this.finder_indexing = false;
                this.finder_index = Some(FinderIndex {
                    root,
                    entries: Rc::new(entries),
                    truncated,
                    built: Instant::now(),
                });
                this.finder_refresh();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finder_refresh(&mut self) {
        let Some(index) = &self.finder_index else {
            if let Some(Overlay::FileFinder(f)) = &mut self.overlay {
                f.matches.clear();
            }
            return;
        };
        let entries = index.entries.clone();
        // Recent files as the index spells them, mapped to their rank — once
        // per refresh. Doing this per match meant re-parsing 30 paths for each
        // of up to 100k entries: a quarter of a second on an empty query.
        let recent: HashMap<String, usize> = self
            .recent_files
            .iter()
            .enumerate()
            .filter_map(|(pos, path)| {
                let rel = path.strip_prefix(&index.root).ok()?;
                Some((rel.to_string_lossy().into_owned(), pos))
            })
            .collect();
        let Some(Overlay::FileFinder(f)) = &mut self.overlay else {
            return;
        };
        let query = f.query.text.trim().to_string();
        let mut matcher = palette::Matcher::new(&query);
        let mut matches: Vec<FinderMatch> = entries
            .iter()
            .enumerate()
            .filter_map(|(ix, rel)| {
                let m = matcher.score(rel)?;
                let mut score = m.score;
                if !query.is_empty() {
                    // Matches inside the file name beat matches in the
                    // directories above it; shallow paths beat deep ones.
                    let base_start = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
                    if m.positions.iter().all(|&p| p >= base_start) {
                        score += 8;
                    }
                    score -= rel.matches('/').count() as i32;
                }
                if let Some(&pos) = recent.get(rel.as_str()) {
                    score += 20 - pos.min(10) as i32;
                }
                Some(FinderMatch {
                    entry: ix,
                    highlights: m.positions,
                    score,
                })
            })
            .collect();
        matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.entry.cmp(&b.entry)));
        matches.truncate(500);
        f.matches = matches;
        f.selected = 0;
        f.scroll = 0;
    }

    fn finder_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Overlay::FileFinder(f)) = &mut self.overlay else {
            return;
        };
        let n = f.matches.len();
        if n == 0 {
            return;
        }
        f.selected = (f.selected as isize + delta).rem_euclid(n as isize) as usize;
        if f.selected < f.scroll {
            f.scroll = f.selected;
        } else if f.selected >= f.scroll + PALETTE_ROWS {
            f.scroll = f.selected + 1 - PALETTE_ROWS;
        }
        cx.notify();
    }

    fn finder_confirm(
        &mut self,
        action: FinderAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Overlay::FileFinder(f)) = &self.overlay else {
            return;
        };
        let Some(index) = &self.finder_index else {
            return;
        };
        let Some(m) = f.matches.get(f.selected) else {
            return;
        };
        let path = index.root.join(&index.entries[m.entry]);
        self.close_overlay(window, cx);
        match action {
            FinderAction::Open => self.open_in_editor(&path, None, window, cx),
            FinderAction::Insert => {
                self.active_pane()
                    .update(cx, |t, cx| t.insert_path(&path, false, cx));
                self.focus_terminal(Some(window), cx);
            }
            FinderAction::Reveal => {
                self.drawer_visible = true;
                self.tree.update(cx, |tree, cx| tree.reveal(path, cx));
                self.focus_tree(Some(window), cx);
            }
        }
    }

    fn render_finder_body(&self, f: &FinderState, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let root_name = self
            .finder_index
            .as_ref()
            .and_then(|ix| ix.root.file_name().map(|n| n.to_string_lossy().to_string()))
            .unwrap_or_default();

        let input = div()
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(accent).child(format!("{root_name}/")))
            .child(self.render_line_edit(&f.query, "find a file…", cx));

        let mut list = div().flex().flex_col().p_1().gap(px(1.0));
        if self.finder_indexing && self.finder_index.is_none() {
            list = list.child(
                div()
                    .mx_2()
                    .my_1()
                    .px_3()
                    .py_1()
                    .text_color(dim)
                    .child("indexing…"),
            );
        } else if f.matches.is_empty() {
            list = list.child(
                div()
                    .mx_2()
                    .my_1()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.ansi[1])
                    .text_color(dim)
                    .child("no matching file"),
            );
        }
        if let Some(index) = &self.finder_index {
            let end = (f.scroll + PALETTE_ROWS).min(f.matches.len());
            for (ix, m) in f.matches.iter().enumerate().take(end).skip(f.scroll) {
                let rel = &index.entries[m.entry];
                let is_selected = ix == f.selected;
                list = list.child(
                    div()
                        .id(("finder-item", ix))
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .when(is_selected, |d| d.bg(theme.selection_bg))
                        .on_mouse_move(cx.listener(
                            move |this, _: &gpui::MouseMoveEvent, _w, cx| {
                                if let Some(Overlay::FileFinder(f)) = &mut this.overlay
                                    && f.selected != ix
                                {
                                    f.selected = ix;
                                    cx.notify();
                                }
                            },
                        ))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                                if let Some(Overlay::FileFinder(f)) = &mut this.overlay {
                                    f.selected = ix;
                                }
                                let action = if crate::terminal::open_modifier(&ev.modifiers) {
                                    FinderAction::Insert
                                } else if ev.modifiers.alt {
                                    FinderAction::Reveal
                                } else {
                                    FinderAction::Open
                                };
                                this.finder_confirm(action, window, cx);
                            }),
                        )
                        .child(div().flex_1().overflow_hidden().child(highlighted_text(
                            rel,
                            &m.highlights,
                            accent,
                        ))),
                );
            }
            if f.matches.len() > PALETTE_ROWS {
                list = list.child(
                    div()
                        .px_3()
                        .py_0p5()
                        .text_size(px(11.0))
                        .text_color(dim)
                        .child(format!("{} of {}", f.selected + 1, f.matches.len())),
                );
            }
        }

        let note = match &self.finder_index {
            Some(ix) if ix.truncated => format!(" · index truncated at {} files", FINDER_INDEX_CAP),
            _ => String::new(),
        };
        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(border)
            .text_size(px(11.0))
            .text_color(dim)
            .child(format!(
                "↑↓ move · {}esc close{note}",
                self.overlay_hints(&[
                    ("overlay::confirm", "open"),
                    ("overlay::confirm_alt", "insert path"),
                    ("overlay::confirm_reveal", "reveal in tree"),
                ])
            ));

        div()
            .flex()
            .flex_col()
            .child(input)
            .child(list)
            .child(footer)
    }

    /// Open a file in `$EDITOR` through the shell, at a line when given.
    /// Without shell integration there's no silent channel to the shell, so
    /// the tree reveals the file instead and says why.
    fn open_in_editor(
        &mut self,
        path: &Path,
        at: Option<(u32, Option<u32>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.push_front(path.to_path_buf());
        self.recent_files.truncate(30);
        // A picture isn't something to edit: OmniPTY can show it.
        if self.config.images.enabled
            && let Some(size) = crate::terminal::images::file_size(path)
        {
            return self.open_image_preview(path, size, window, cx);
        }
        let shell = self.shell_program();
        let name = crate::terminal::session::shell_name(&shell);
        let widgets = name.starts_with("zsh") || name.starts_with("bash");
        if !(self.config.shell.integration && widgets) {
            self.drawer_visible = true;
            self.tree
                .update(cx, |tree, cx| tree.reveal(path.to_path_buf(), cx));
            self.toast(ToastKind::Info,
                "shell integration is off, so OmniPTY can't ask the shell for $EDITOR — revealed in the tree instead".into(),
                cx,
            );
            return;
        }
        let command = editor_command(path, at, &shell, self.config.editor.open_at_line.as_deref());
        self.run_at_prompt(command, window, cx);
    }

    // --- Command history (cmd-r) ---

    fn toggle_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.overlay {
            Some(Overlay::History(_)) => self.close_overlay(window, cx),
            Some(Overlay::ThemePicker(_)) => {
                self.picker_cancel(window, cx);
                self.open_history(window, cx);
            }
            Some(_) => {
                self.close_overlay(window, cx);
                self.open_history(window, cx);
            }
            None => self.open_history(window, cx),
        }
    }

    /// Every command with known text, across every pane in the window,
    /// newest first, one row per distinct command line.
    fn gather_history(&self, cx: &Context<Self>) -> Vec<HistoryItem> {
        let mut items: Vec<HistoryItem> = Vec::new();
        for pane in self.panes.values() {
            for cmd in pane.read(cx).log.entries() {
                if let Some(text) = &cmd.text {
                    items.push(HistoryItem {
                        text: text.clone(),
                        cwd: cmd.cwd.clone(),
                        exit: cmd.exit,
                        finished: cmd.finished,
                    });
                }
            }
        }
        items.sort_by_key(|a| std::cmp::Reverse(a.finished));
        let mut seen = std::collections::HashSet::new();
        items.retain(|it| seen.insert(it.text.clone()));
        items
    }

    fn open_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let return_focus = self.current_focus_target(window, cx);
        let items = self.gather_history(cx);
        self.overlay = Some(Overlay::History(HistoryState {
            query: LineEdit::default(),
            items,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            return_focus,
        }));
        self.history_refresh(cx);
        window.focus(&self.picker_focus);
        cx.notify();
    }

    fn history_refresh(&mut self, cx: &Context<Self>) {
        let here = self.active_pane().read(cx).cwd.clone();
        let Some(Overlay::History(h)) = &mut self.overlay else {
            return;
        };
        let query = h.query.text.trim().to_string();
        let mut matcher = palette::Matcher::new(&query);
        let mut matches: Vec<HistoryMatch> = h
            .items
            .iter()
            .enumerate()
            .filter_map(|(ix, item)| {
                let m = matcher.score(&item.text)?;
                // Commands run in the directory you're in now are the ones
                // you most likely want again.
                let local = if item.cwd.is_some() && item.cwd == here {
                    1
                } else {
                    0
                };
                Some(HistoryMatch {
                    item: ix,
                    highlights: m.positions,
                    score: m.score * 2 + local,
                })
            })
            .collect();
        // Best match first; equal scores keep newest-first order. With no
        // query every score is 0 or 1, so this just floats local commands.
        matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.item.cmp(&b.item)));
        h.matches = matches;
        h.selected = 0;
        h.scroll = 0;
    }

    fn history_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Overlay::History(h)) = &mut self.overlay else {
            return;
        };
        let n = h.matches.len();
        if n == 0 {
            return;
        }
        h.selected = (h.selected as isize + delta).rem_euclid(n as isize) as usize;
        if h.selected < h.scroll {
            h.scroll = h.selected;
        } else if h.selected >= h.scroll + PALETTE_ROWS {
            h.scroll = h.selected + 1 - PALETTE_ROWS;
        }
        cx.notify();
    }

    /// Insert the command at the prompt, or (`run`) execute it through the
    /// silent-run channel so it lands in shell history exactly once.
    fn history_confirm(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::History(h)) = &self.overlay else {
            return;
        };
        let Some(m) = h.matches.get(h.selected) else {
            return;
        };
        let text = h.items[m.item].text.clone();
        self.close_overlay(window, cx);
        let pane = self.active_pane();
        pane.update(cx, |t, _| {
            if run {
                t.run_command(&text);
            } else {
                t.write_command(&text);
            }
        });
        self.focus_terminal(Some(window), cx);
    }

    fn render_history_body(&self, h: &HistoryState, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        let home = home_dir();

        let input = div()
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(accent).child("history"))
            .child(self.render_line_edit(&h.query, "search commands you've run…", cx));

        let mut list = div().flex().flex_col().p_1().gap(px(1.0));
        if h.matches.is_empty() {
            let message = if h.items.is_empty() {
                "no commands yet — history fills in as the shell integration sees them run"
            } else {
                "no matching command"
            };
            list = list.child(
                div()
                    .mx_2()
                    .my_1()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.ansi[1])
                    .text_color(dim)
                    .child(message),
            );
        }
        let end = (h.scroll + PALETTE_ROWS).min(h.matches.len());
        for (ix, m) in h.matches.iter().enumerate().take(end).skip(h.scroll) {
            let item = &h.items[m.item];
            let is_selected = ix == h.selected;
            let (mark, mark_color) = match item.exit {
                Some(0) => ("✓", blend(theme.ansi[2], theme.background, 0.2)),
                Some(_) => ("✗", theme.ansi[1]),
                None => ("•", dim),
            };
            let where_ = item
                .cwd
                .as_ref()
                .map(|p| pretty_path(p, home.as_deref()))
                .unwrap_or_default();
            list = list.child(
                div()
                    .id(("history-item", ix))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(is_selected, |d| d.bg(theme.selection_bg))
                    .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _w, cx| {
                        if let Some(Overlay::History(h)) = &mut this.overlay
                            && h.selected != ix
                        {
                            h.selected = ix;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                            if let Some(Overlay::History(h)) = &mut this.overlay {
                                h.selected = ix;
                            }
                            this.history_confirm(
                                crate::terminal::open_modifier(&ev.modifiers),
                                window,
                                cx,
                            );
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .text_color(mark_color)
                            .child(mark),
                    )
                    .child(div().flex_1().overflow_hidden().child(highlighted_text(
                        &item.text,
                        &m.highlights,
                        accent,
                    )))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(dim)
                            .child(where_),
                    ),
            );
        }
        if h.matches.len() > PALETTE_ROWS {
            list = list.child(
                div()
                    .px_3()
                    .py_0p5()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child(format!("{} of {}", h.selected + 1, h.matches.len())),
            );
        }

        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(border)
            .text_size(px(11.0))
            .text_color(dim)
            .child(format!(
                "↑↓ move · {}esc close",
                self.overlay_hints(&[
                    ("overlay::confirm", "insert at prompt"),
                    ("overlay::confirm_alt", "run"),
                ])
            ));

        div()
            .flex()
            .flex_col()
            .child(input)
            .child(list)
            .child(footer)
    }

    fn render_palette_body(&self, p: &PaletteState, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);

        let input = div()
            .flex_none()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(div().text_color(accent).child(">"))
            .child(self.render_line_edit(&p.query, "type a command…", cx));

        let mut list = div().flex().flex_col().p_1().gap(px(1.0));
        if p.matches.is_empty() {
            list = list.child(
                div()
                    .mx_2()
                    .my_1()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.ansi[1])
                    .text_color(dim)
                    .child("no matching command"),
            );
        }
        let end = (p.scroll + PALETTE_ROWS).min(p.matches.len());
        for (ix, item) in p.matches.iter().enumerate().take(end).skip(p.scroll) {
            let is_selected = ix == p.selected;
            list = list.child(
                div()
                    .id(("palette-item", ix))
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(is_selected, |d| d.bg(theme.selection_bg))
                    .on_mouse_move(cx.listener(move |this, _: &gpui::MouseMoveEvent, _w, cx| {
                        if let Some(Overlay::Palette(p)) = &mut this.overlay
                            && p.selected != ix
                        {
                            p.selected = ix;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            if let Some(Overlay::Palette(p)) = &mut this.overlay {
                                p.selected = ix;
                            }
                            this.palette_confirm(window, cx);
                        }),
                    )
                    .child(div().flex_none().text_color(dim).child(item.category))
                    .child(div().flex_none().text_color(dim).child("›"))
                    .child(div().flex_1().overflow_hidden().child(highlighted_text(
                        item.title,
                        &item.highlights,
                        accent,
                    )))
                    .when_some(item.binding.clone(), |d, keys| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(dim)
                                .child(keys),
                        )
                    }),
            );
        }
        if p.matches.len() > PALETTE_ROWS {
            list = list.child(
                div()
                    .px_3()
                    .py_0p5()
                    .text_size(px(11.0))
                    .text_color(dim)
                    .child(format!("{} of {}", p.selected + 1, p.matches.len())),
            );
        }

        let footer = div()
            .flex_none()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(border)
            .text_size(px(11.0))
            .text_color(dim)
            .child("↑↓ move · ⏎ run · esc close");

        div()
            .flex()
            .flex_col()
            .child(input)
            .child(list)
            .child(footer)
    }

    fn render_overlay(&self, cx: &Context<Self>) -> gpui::Div {
        let Some(overlay) = &self.overlay else {
            return div();
        };
        let theme = &self.theme;
        let panel_bg = blend(theme.background, gpui::black(), 0.2);
        // Dim the window behind the modal — but only a solid one. Over a
        // translucent window the scrim is a second layer on every
        // background, and the window goes dark and solid as if opacity and
        // blur had been switched off.
        // ponytail: no dimming at all when translucent; to dim there, each
        // region would have to thin its own background while a modal is up.
        let mut backdrop = gpui::black();
        backdrop.a = if self.config.window.opacity < 1.0 {
            0.0
        } else {
            0.35
        };

        let panel = div()
            .key_context("Overlay")
            .on_action(cx.listener(|this, _: &PickerNext, _w, cx| this.overlay_move(1, cx)))
            .on_action(cx.listener(|this, _: &PickerPrev, _w, cx| this.overlay_move(-1, cx)))
            .on_action(cx.listener(|this, _: &PickerConfirm, window, cx| {
                this.overlay_confirm(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PickerCancel, window, cx| {
                this.overlay_cancel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PickerConfirmAlt, window, cx| {
                this.overlay_confirm_alt(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PickerConfirmReveal, window, cx| {
                this.overlay_confirm_reveal(window, cx);
            }))
            // The terminal's clipboard keys, for the overlay's text field.
            .on_action(cx.listener(|this, _: &Copy, _w, cx| this.overlay_copy(cx)))
            .on_action(cx.listener(|this, _: &Paste, _w, cx| this.overlay_paste(cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _w, cx| {
                if let Some(q) = this.overlay_query_mut() {
                    q.select_all();
                    cx.notify();
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                |_: &gpui::MouseDownEvent, _w, cx| cx.stop_propagation(),
            )
            .mt(px(80.0))
            .rounded_lg()
            .border_1()
            .border_color(theme.ansi[8])
            .bg(panel_bg)
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden();

        let panel = match overlay {
            Overlay::ThemePicker(picker) => panel
                .w(px(360.0))
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(blend(theme.foreground, theme.background, 0.85))
                        .text_color(blend(theme.foreground, theme.background, 0.3))
                        .child("Select Theme — ↑↓ preview, ⏎ apply, esc cancel"),
                )
                // The list has no text input, so vim keys are free here.
                .child(
                    div()
                        .key_context("OverlayList")
                        .track_focus(&self.picker_focus)
                        .child(self.render_theme_list(picker, cx)),
                ),
            Overlay::Palette(p) => panel
                .w(px(560.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_palette_body(p, cx)),
            Overlay::History(h) => panel
                .w(px(640.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_history_body(h, cx)),
            Overlay::FileFinder(f) => panel
                .w(px(640.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_finder_body(f, cx)),
            Overlay::Prompt(p) => panel
                .w(px(420.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_prompt_body(p, cx)),
            Overlay::Confirm(c) => panel
                .w(px(420.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_confirm_body(c)),
            Overlay::StartupCommand(s) => panel
                .w(px(560.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_startup_command_body(s, cx)),
            Overlay::StartupEditor(e) => panel
                .w(px(640.0))
                .track_focus(&self.picker_focus)
                .on_key_down(cx.listener(Self::on_overlay_key_down))
                .child(self.render_startup_editor_body(e, cx)),
            Overlay::About(_) => panel
                .w(px(340.0))
                .track_focus(&self.picker_focus)
                .child(self.render_about_body(cx)),
        };

        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .bg(backdrop)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    this.overlay_cancel(window, cx);
                }),
            )
            .child(panel)
    }

    // --- Split resizing ---

    /// Smallest pane size along `axis`, in pixels, for a pane with these
    /// cell metrics: a few cells plus the padding and focus ring.
    fn min_pane_px(&self, axis: Axis, layout: &LastLayout) -> f32 {
        let pad = self.config.window.padding;
        match axis {
            Axis::Horizontal => MIN_PANE_COLS * layout.cell_width + pad.x * 2.0 + 2.0,
            Axis::Vertical => MIN_PANE_ROWS * layout.cell_height + pad.y * 2.0 + 2.0,
        }
    }

    /// A pane's on-screen extent along `axis`, including its focus ring.
    fn pane_extent(&self, id: PaneId, axis: Axis, cx: &Context<Self>) -> Option<f32> {
        let bounds = self.pane_bounds(id, cx)?;
        Some(
            match axis {
                Axis::Horizontal => f32::from(bounds.size.width),
                Axis::Vertical => f32::from(bounds.size.height),
            } + 2.0,
        )
    }

    fn start_divider_drag(
        &mut self,
        path: NodePath,
        divider: usize,
        axis: Axis,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let layout = &self.tab().layout;
        let Some(Node::Split {
            ratios, children, ..
        }) = layout.at_path(&path)
        else {
            return;
        };
        if divider + 1 >= children.len() || ratios.len() != children.len() {
            return;
        }
        // Measure the split through a pane inside the child before the
        // divider: that child spans the split's full extent along the axis
        // scaled by its ratio, since nested splits always alternate axes.
        let Some(leaf) = children[divider].leaves().first().copied() else {
            return;
        };
        let Some(extent_child) = self.pane_extent(leaf, axis, cx) else {
            return;
        };
        let Some(pane_layout) = self.panes.get(&leaf).and_then(|p| p.read(cx).last_layout) else {
            return;
        };
        let extent = extent_child / ratios[divider];
        if !extent.is_finite() || extent <= 0.0 {
            return;
        }
        let min_ratio = (self.min_pane_px(axis, &pane_layout) / extent).min(0.45);
        let start_pos = match axis {
            Axis::Horizontal => f32::from(position.x),
            Axis::Vertical => f32::from(position.y),
        };
        self.divider_drag = Some(DividerDrag {
            path,
            divider,
            axis,
            start_ratios: ratios.clone(),
            start_pos,
            extent,
            min_ratio,
        });
        cx.notify();
    }

    fn update_divider_drag(&mut self, position: gpui::Point<gpui::Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = &self.divider_drag else {
            return;
        };
        let pos = match drag.axis {
            Axis::Horizontal => f32::from(position.x),
            Axis::Vertical => f32::from(position.y),
        };
        let delta = (pos - drag.start_pos) / drag.extent;
        let (path, divider, start, min) = (
            drag.path.clone(),
            drag.divider,
            drag.start_ratios.clone(),
            drag.min_ratio,
        );
        let layout = &mut self.tab_mut().layout;
        // Always move relative to where the drag began, so the divider tracks
        // the pointer instead of accumulating rounding from each event.
        if let Some(Node::Split { ratios, .. }) = layout.at_path_mut(&path)
            && ratios.len() == start.len()
        {
            *ratios = start;
        }
        layout.resize_divider(&path, divider, delta, min);
        cx.notify();
    }

    fn end_divider_drag(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.drawer_drag) {
            self.write_window_state();
            cx.notify();
        }
        if self.divider_drag.take().is_some() {
            self.save_workspaces(cx);
            cx.notify();
        }
    }

    /// Grow or shrink the focused pane along `axis` by a number of cells.
    fn resize_active(&mut self, axis: Axis, cells: f32, cx: &mut Context<Self>) {
        let id = self.active_id();
        let Some(pane_extent) = self.pane_extent(id, axis, cx) else {
            return;
        };
        let Some(pane_layout) = self.panes.get(&id).and_then(|p| p.read(cx).last_layout) else {
            return;
        };
        let layout = &self.tab().layout;
        let Some(path) = layout.path_to(&id) else {
            return;
        };
        // The nearest enclosing split that runs along `axis` is the one the
        // resize applies to; the pane's own extent equals its child's there.
        let mut share = None;
        for depth in (0..path.len()).rev() {
            if let Some(Node::Split {
                axis: a, ratios, ..
            }) = layout.at_path(&path[..depth])
                && *a == axis
            {
                share = ratios.get(path[depth]).copied();
                break;
            }
        }
        let Some(share) = share else { return };
        let extent = pane_extent / share;
        if !extent.is_finite() || extent <= 0.0 {
            return;
        }
        let cell = match axis {
            Axis::Horizontal => pane_layout.cell_width,
            Axis::Vertical => pane_layout.cell_height,
        };
        let delta = cells * cell / extent;
        let min = (self.min_pane_px(axis, &pane_layout) / extent).min(0.45);
        if self.tab_mut().layout.resize_leaf(&id, axis, delta, min) {
            self.save_workspaces(cx);
            cx.notify();
        }
    }

    fn equalize_splits(&mut self, cx: &mut Context<Self>) {
        self.tab_mut().layout.equalise();
        self.save_workspaces(cx);
        cx.notify();
    }

    fn render_drag_overlay(&self, cx: &Context<Self>) -> gpui::Div {
        // A pane divider, or the drawer's edge.
        let cursor = match &self.divider_drag {
            Some(drag) if drag.axis == Axis::Vertical => gpui::CursorStyle::ResizeUpDown,
            _ => gpui::CursorStyle::ResizeLeftRight,
        };
        // Sits over everything while dragging so the pointer can leave the
        // divider, and so the terminals underneath never see the drag as a
        // text selection.
        div()
            .absolute()
            .inset_0()
            .occlude()
            .cursor(cursor)
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, window, cx| {
                if this.drawer_drag {
                    // The drawer starts at the window's left edge, so the
                    // pointer's x is its width.
                    let window_width = f32::from(window.viewport_size().width);
                    this.drawer_width =
                        Some(clamp_drawer_width(f32::from(ev.position.x), window_width));
                    cx.notify();
                } else {
                    this.update_divider_drag(ev.position, cx);
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _w, cx| this.end_divider_drag(cx)),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _w, cx| this.end_divider_drag(cx)),
            )
    }

    // --- Tabs ---

    /// A user-set name wins; then the foreground program (`vim`, `cargo`,
    /// `ssh prod-web`) when one is running; else the directory.
    fn tab_title(&self, tab: &TabState, cx: &Context<Self>) -> String {
        if let Some(title) = &tab.title {
            return title.clone();
        }
        let Some(pane) = self.panes.get(&tab.active) else {
            return "shell".into();
        };
        let pane = pane.read(cx);
        if let Some(fg) = &pane.foreground
            && !fg.is_shell()
        {
            return fg.label();
        }
        let cwd = pane.cwd.clone();
        match cwd {
            Some(p) => {
                if home_dir().is_some_and(|h| h == p) {
                    "~".into()
                } else {
                    p.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "/".into())
                }
            }
            None => "shell".into(),
        }
    }

    fn new_tab_cwd(&self, cx: &Context<Self>) -> PathBuf {
        match self.config.window.new_tab_directory {
            crate::config::schema::NewTabDirectory::Home => home_dir(),
            crate::config::schema::NewTabDirectory::Pwd => {
                self.active_pane().read(cx).cwd.clone().or_else(home_dir)
            }
        }
        .unwrap_or_else(|| PathBuf::from("/"))
    }

    fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.new_tab_cwd(cx);
        let id = self.create_pane(cwd, window, cx);
        let ws = self.ws_mut();
        ws.tabs.push(TabState::new(Node::Leaf(id), id));
        ws.active_tab = ws.tabs.len() - 1;
        self.focus_pane(id, window, cx);
        self.save_workspaces(cx);
    }

    fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.ws().tabs.len() {
            return;
        }
        self.ws_mut().active_tab = ix;
        let id = self.active_id();
        self.focus_pane(id, window, cx);
        self.save_workspaces(cx);
    }

    /// Reorder: move the tab at `from` to `to`, keeping the active tab active.
    fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let ws = self.ws_mut();
        let n = ws.tabs.len();
        if from == to || from >= n || to >= n {
            return;
        }
        let tab = ws.tabs.remove(from);
        ws.tabs.insert(to, tab);
        ws.active_tab = index_after_move(ws.active_tab, from, to);
        self.save_workspaces(cx);
        cx.notify();
    }

    /// Reorder: move the workspace at `from` to `to`. The active workspace
    /// and the panel's cursor stay on the workspaces they were on.
    fn move_workspace(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let n = self.workspaces.len();
        if from == to || from >= n || to >= n {
            return;
        }
        let ws = self.workspaces.remove(from);
        self.workspaces.insert(to, ws);
        self.active_ws = index_after_move(self.active_ws, from, to);
        self.ws_selected = index_after_move(self.ws_selected, from, to);
        self.save_workspaces(cx);
        cx.notify();
    }

    fn move_active_tab(&mut self, delta: isize, cx: &mut Context<Self>) {
        let from = self.ws().active_tab;
        let n = self.ws().tabs.len() as isize;
        let to = from as isize + delta;
        if to >= 0 && to < n {
            self.move_tab(from, to as usize, cx);
        }
    }

    /// What a tab looks like on disk: pane records (directory, startup
    /// command) in place of pane ids.
    fn saved_tab(&self, t: &TabState, cx: &Context<Self>) -> SavedTab {
        let layout = t.layout.map(&mut |id| self.saved_pane(*id, cx));
        let active = t
            .layout
            .leaves()
            .iter()
            .position(|l| *l == t.active)
            .unwrap_or(0);
        SavedTab {
            layout,
            active,
            title: t.title.clone(),
        }
    }

    fn saved_pane(&self, id: PaneId, cx: &Context<Self>) -> SavedPane {
        let pane = self.panes.get(&id).map(|p| p.read(cx));
        let cwd = pane
            .and_then(|p| p.cwd.clone())
            .or_else(home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let startup = pane.and_then(|p| p.startup.clone());
        SavedPane {
            cwd,
            command: startup.as_ref().map(|s| s.command.clone()),
            on_exit: startup.map(|s| s.on_exit).unwrap_or_default(),
        }
    }

    /// A fresh shell from a saved pane. With `run`, its startup command is
    /// queued to fire once the shell is ready.
    fn create_pane_from(
        &mut self,
        saved: &SavedPane,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PaneId {
        let id = self.create_pane(saved.cwd.clone(), window, cx);
        if let Some(startup) = saved.startup() {
            let timeout = self.config.workspaces.startup_timeout.0;
            self.panes[&id].update(cx, |pane, cx| {
                pane.set_startup(Some(startup), cx);
                if run {
                    pane.arm_startup(timeout, cx);
                }
            });
        }
        id
    }

    fn pane_startup(&self, id: PaneId, cx: &Context<Self>) -> Option<StartupCommand> {
        self.panes.get(&id).and_then(|p| p.read(cx).startup.clone())
    }

    /// Whether any pane in `ws` has a startup command.
    fn workspace_has_startup(&self, ws: &Workspace, cx: &Context<Self>) -> bool {
        ws.tabs
            .iter()
            .flat_map(|t| t.layout.leaves())
            .any(|id| self.pane_startup(id, cx).is_some())
    }

    /// Shift held while the window comes up, seen late (see `opened_at`):
    /// cancel every pane's still-pending startup command, as if the flag
    /// had been read at launch.
    fn on_launch_modifiers(&mut self, modifiers: gpui::Modifiers, cx: &mut Context<Self>) {
        const SHIFT_AT_LAUNCH_WINDOW: Duration = Duration::from_millis(1500);
        if !modifiers.shift
            || self.startup_skipped_at_launch
            || !self.run_startup_commands
            || self.opened_at.elapsed() > SHIFT_AT_LAUNCH_WINDOW
        {
            return;
        }
        self.startup_skipped_at_launch = true;
        self.run_startup_commands = false;
        let mut cancelled = false;
        for pane in self.panes.values() {
            cancelled |= pane.update(cx, |pane, cx| pane.cancel_startup(cx));
        }
        if cancelled || self.any_startup_commands(cx) {
            self.toast(
                ToastKind::Info,
                "startup commands skipped for this launch".into(),
                cx,
            );
        }
    }

    fn any_startup_commands(&self, cx: &Context<Self>) -> bool {
        self.workspaces
            .iter()
            .any(|w| self.workspace_has_startup(w, cx))
    }

    /// Keep a closed tab's shape and directories so cmd-shift-t can bring
    /// it back with fresh shells.
    fn remember_closed_tab(&mut self, tab: &TabState, cx: &Context<Self>) {
        let saved = self.saved_tab(tab, cx);
        self.closed_tabs.push_front(saved);
        self.closed_tabs.truncate(CLOSED_TAB_RING);
    }

    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(saved) = self.closed_tabs.pop_front() else {
            self.toast(ToastKind::Info, "no closed tab to reopen".into(), cx);
            return;
        };
        let run = self.run_startup_commands;
        let layout = saved
            .layout
            .map(&mut |pane| self.create_pane_from(pane, run, window, cx));
        let leaves = layout.leaves();
        let active = leaves.get(saved.active).copied().unwrap_or(leaves[0]);
        let mut tab = TabState::new(layout, active);
        tab.title = saved.title;
        let ws = self.ws_mut();
        ws.tabs.push(tab);
        ws.active_tab = ws.tabs.len() - 1;
        self.focus_pane(active, window, cx);
        self.save_workspaces(cx);
    }

    /// Move to the next/previous tab in the active workspace.
    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.ws().tabs.len();
        if n < 2 {
            return;
        }
        let ix = (self.ws().active_tab as isize + delta).rem_euclid(n as isize) as usize;
        self.select_tab(ix, window, cx);
    }

    fn render_tab_bar(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let bar_bg = translucent(
            blend(theme.background, gpui::black(), 0.25),
            self.config.window.opacity,
        );
        let active_bg = translucent(theme.background, self.config.window.opacity);
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);
        // No background on the bar itself: each piece along it paints its
        // own, so the active tab is a lighter stretch of the same single
        // layer rather than a second one on top, which would make it more
        // solid than the rest of a translucent window.
        let mut bar = div()
            .flex_none()
            .h(px(30.0))
            .flex()
            .flex_row()
            .items_center()
            .border_b_1()
            .border_color(border)
            .text_size(px(12.0));

        // Linux: with the drawer hidden this bar is the top-left corner the
        // ☰ menu button floats over; leave it room before the first tab.
        if self.app_menu_corner() == Some(AppMenuCorner::TabBar) {
            bar = bar.child(
                div()
                    .flex_none()
                    .h_full()
                    .w(px(APP_MENU_BUTTON_CLEARANCE))
                    .bg(bar_bg),
            );
        }

        for (ix, tab) in self.ws().tabs.iter().enumerate() {
            let is_active = ix == self.ws().active_tab;
            let title = self.tab_title(tab, cx);
            let close_target = tab.active;
            // Activity: running beats failed beats unread; nothing at all is
            // the common case and should look like it.
            let mut running = false;
            let mut latest_failed: Option<(Instant, bool)> = None;
            for id in tab.layout.leaves() {
                let Some(pane) = self.panes.get(&id) else {
                    continue;
                };
                let log = &pane.read(cx).log;
                running |= log.is_running();
                if let Some(cmd) = log.last_finished()
                    && let Some(at) = cmd.finished
                    && latest_failed.is_none_or(|(t, _)| at > t)
                {
                    latest_failed = Some((at, cmd.failed()));
                }
            }
            let indicator = if running {
                Some(theme.ansi[4])
            } else if latest_failed.is_some_and(|(_, failed)| failed) {
                Some(theme.ansi[1])
            } else if tab.unread && !is_active {
                Some(dim)
            } else {
                None
            };
            let drag_title: gpui::SharedString = title.clone().into();
            bar = bar.child(
                div()
                    .id(("tab", ix))
                    .h_full()
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .border_r_1()
                    .border_color(border)
                    .cursor_pointer()
                    .when(is_active, |d| d.bg(active_bg).text_color(theme.foreground))
                    .when(!is_active, |d| d.bg(bar_bg).text_color(dim))
                    // Double-click renames; a single click selects.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                            if ev.click_count >= 2 {
                                this.open_tab_rename(ix, window, cx);
                            } else {
                                this.select_tab(ix, window, cx);
                            }
                        }),
                    )
                    // Drag a tab onto another to reorder.
                    .on_drag(TabDrag { ix }, move |_, _, _window, cx| {
                        cx.new(|_| TabDragLabel(drag_title.clone()))
                    })
                    .on_drop(cx.listener(move |this, drag: &TabDrag, _window, cx| {
                        this.move_tab(drag.ix, ix, cx);
                    }))
                    .when(self.config.tabs.show_numbers, |d| {
                        d.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .text_color(dim)
                                .child((ix + 1).to_string()),
                        )
                    })
                    .when_some(indicator, |d, color| {
                        d.child(
                            div()
                                .flex_none()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(color),
                        )
                    })
                    .child(title)
                    .child(
                        div()
                            .id(("tab-close", ix))
                            .text_color(dim)
                            .cursor_pointer()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    // Closing a tab closes every pane in it;
                                    // close_pane cascades from its active pane
                                    // only when it's the last one, so close
                                    // the whole tab explicitly.
                                    if let Some((wix, tix)) = this.locate_pane(close_target) {
                                        if this.workspaces[wix].tabs.len() > 1 {
                                            this.close_tab_at(wix, tix, window, cx);
                                        } else if !this.close_last_tab(wix, window, cx) {
                                            window.remove_window();
                                        }
                                    }
                                }),
                            )
                            .child("×"),
                    ),
            );
        }

        bar.child(
            div()
                .id("tab-add")
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .bg(bar_bg)
                .text_color(dim)
                .cursor_pointer()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.new_tab(window, cx);
                    }),
                )
                .child("+"),
        )
        // The rest of the bar.
        .child(div().flex_1().h_full().bg(bar_bg))
    }

    // --- Workspaces ---

    fn next_ws_name(&self) -> String {
        default_ws_name(self.workspaces.iter().map(|w| w.name.as_str()))
    }

    fn new_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A workspace is a fresh context, not a continuation of the current
        // one — unlike new tabs, it always starts at home rather than
        // inheriting the focused pane's directory.
        let cwd = home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let id = self.create_pane(cwd, window, cx);
        let name = self.next_ws_name();
        self.workspaces.push(Workspace {
            name,
            persist: false,
            tabs: vec![TabState::new(Node::Leaf(id), id)],
            active_tab: 0,
        });
        self.active_ws = self.workspaces.len() - 1;
        self.ws_selected = self.active_ws;
        self.focus_pane(id, window, cx);
        self.save_workspaces(cx);
    }

    fn select_workspace(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.workspaces.len() {
            return;
        }
        self.active_ws = ix;
        self.ws_selected = ix;
        let id = self.active_id();
        self.focus_pane(id, window, cx);
    }

    /// Serialize every persist-flagged workspace: the split trees with pane
    /// ids swapped for their shells' current directories.
    fn save_workspaces(&self, cx: &Context<Self>) {
        let saved: Vec<SavedWorkspace> = self
            .workspaces
            .iter()
            .filter(|w| w.persist)
            .map(|w| SavedWorkspace {
                name: w.name.clone(),
                active_tab: w.active_tab,
                tabs: w.tabs.iter().map(|t| self.saved_tab(t, cx)).collect(),
            })
            .collect();
        crate::workspaces::save(&saved);
    }

    /// First-run state: restore persisted workspaces (fresh shells in their
    /// saved directories) or create the default "workspace 1".
    fn bootstrap_workspaces(
        &mut self,
        initial_cwd: PathBuf,
        command: Option<Vec<String>>,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if restore {
            let run = self.run_startup_commands;
            let pinned = crate::workspaces::load();
            crate::telemetry::breadcrumb(
                "workspaces",
                format!("restoring {} pinned", pinned.len()),
            );
            for saved in pinned {
                let mut tabs = Vec::new();
                for st in &saved.tabs {
                    let layout = st
                        .layout
                        .map(&mut |pane| self.create_pane_from(pane, run, window, cx));
                    let leaves = layout.leaves();
                    let active = leaves.get(st.active).copied().unwrap_or(leaves[0]);
                    let mut tab = TabState::new(layout, active);
                    tab.title = st.title.clone();
                    tabs.push(tab);
                }
                let active_tab = saved.active_tab.min(tabs.len() - 1);
                self.workspaces.push(Workspace {
                    name: saved.name,
                    persist: true,
                    tabs,
                    active_tab,
                });
            }
        }
        if self.workspaces.is_empty() {
            let id = self.create_pane_running(initial_cwd, command, window, cx);
            let name = self.next_ws_name();
            self.workspaces.push(Workspace {
                name,
                persist: false,
                tabs: vec![TabState::new(Node::Leaf(id), id)],
                active_tab: 0,
            });
        }
        self.active_ws = 0;
        self.ws_selected = 0;
        let id = self.active_id();
        self.focus_pane(id, window, cx);
    }

    fn focus_workspaces_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drawer_visible = true;
        self.ws_selected = self.active_ws;
        window.focus(&self.ws_focus);
        cx.notify();
    }

    /// The panel's only footer question: "delete? (y/n)". Anything but y
    /// cancels.
    fn on_ws_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !std::mem::take(&mut self.ws_confirm_delete) {
            return;
        }
        if event.keystroke.key == "y" {
            let ix = self.ws_selected;
            self.remove_workspace_at(ix, window, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn ws_footer_text(&self) -> Option<String> {
        let name = &self.workspaces.get(self.ws_selected)?.name;
        self.ws_confirm_delete
            .then(|| format!("delete {name}? (y/n)"))
    }

    fn render_workspace_panel(&self, window: &Window, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let focused = self.ws_focus.is_focused(window);
        let accent = theme.ansi[4];
        let dim = blend(theme.foreground, theme.background, 0.45);
        let border = blend(theme.foreground, theme.background, 0.85);

        let mut list = div().flex_1().min_h_0().flex().flex_col().overflow_hidden();
        for (ix, w) in self.workspaces.iter().enumerate() {
            let is_active = ix == self.active_ws;
            let is_selected = focused && ix == self.ws_selected;
            let mut selection_bg = theme.selection_bg;
            if !focused {
                selection_bg.a = 0.45;
            }
            let mut active_bg = accent;
            active_bg.a = 0.16;
            let (drag_name, pinned): (gpui::SharedString, _) = (w.name.clone().into(), w.persist);
            let font: gpui::SharedString = self.config.font.family.primary().to_string().into();
            let drag_theme = self.theme.clone();
            // The row spans the drawer, less its border and its own margins.
            let row_width = self.drawer_width(window) - 1.0 - WS_ROW_INSET * 2.0;
            list = list.child(
                div()
                    .id(("workspace", ix))
                    .flex_none()
                    .h(px(WS_ROW_HEIGHT))
                    .mx(px(WS_ROW_INSET))
                    .my_0p5()
                    .px_2()
                    .rounded_md()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .when(is_active, |d| d.bg(active_bg).text_color(theme.foreground))
                    .when(!is_active, |d| d.text_color(dim))
                    .when(is_selected, |d| d.bg(selection_bg))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            this.select_workspace(ix, window, cx);
                        }),
                    )
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                            this.ws_selected = ix;
                            this.ws_context_menu = Some(WsContextMenu {
                                ix,
                                position: ev.position,
                            });
                            cx.notify();
                        }),
                    )
                    // Drag a workspace onto another to reorder.
                    .on_drag(WsDrag { ix }, move |_, grabbed, _window, cx| {
                        cx.new(|_| WsDragCard {
                            name: drag_name.clone(),
                            pinned,
                            width: row_width,
                            grab_x: f32::from(grabbed.x),
                            font: font.clone(),
                            theme: drag_theme.clone(),
                        })
                    })
                    .on_drop(cx.listener(move |this, drag: &WsDrag, _window, cx| {
                        this.move_workspace(drag.ix, ix, cx);
                    }))
                    .child(div().flex_1().truncate().child(w.name.clone()))
                    .when(self.workspace_has_startup(w, cx), |d| {
                        // Has startup commands, visible without opening anything.
                        d.child(div().flex_none().text_color(dim).child("▸"))
                    })
                    .when(w.persist, |d| {
                        // Pin: this workspace survives restarts.
                        d.child(div().flex_none().text_color(accent).child("\u{f08d}"))
                    })
                    .child(
                        div()
                            .id(("workspace-close", ix))
                            .flex_none()
                            .text_color(dim)
                            .cursor_pointer()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    // No y/n here: the click was aimed.
                                    this.ws_confirm_delete = false;
                                    this.remove_workspace_at(ix, window, cx);
                                }),
                            )
                            .child("×"),
                    ),
            );
        }

        div()
            .key_context(if self.ws_confirm_delete {
                "WorkspacesInput"
            } else {
                "Workspaces"
            })
            .track_focus(&self.ws_focus)
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .text_size(px(13.0))
            .on_key_down(cx.listener(Self::on_ws_key_down))
            .on_action(cx.listener(|this, _: &WsDown, _w, cx| {
                if !this.workspaces.is_empty() {
                    this.ws_selected = (this.ws_selected + 1).min(this.workspaces.len() - 1);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &WsUp, _w, cx| {
                this.ws_selected = this.ws_selected.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &WsOpen, window, cx| {
                let ix = this.ws_selected;
                this.select_workspace(ix, window, cx);
            }))
            .on_action(cx.listener(|this, _: &WsAdd, window, cx| {
                this.open_prompt(
                    "New workspace".into(),
                    "",
                    LineEdit::default(),
                    PromptAction::WsAdd,
                    window,
                    cx,
                );
            }))
            .on_action(cx.listener(|this, _: &WsDelete, _w, cx| {
                this.ws_confirm_delete = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &WsRename, window, cx| {
                let ix = this.ws_selected;
                this.open_ws_rename(ix, window, cx);
            }))
            .on_action(cx.listener(|this, _: &WsTogglePersist, _w, cx| {
                if let Some(ws) = this.workspaces.get_mut(this.ws_selected) {
                    ws.persist = !ws.persist;
                    this.save_workspaces(cx);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &WsEditStartup, window, cx| {
                let ix = this.ws_selected;
                this.open_startup_editor(ix, window, cx);
            }))
            .on_action(cx.listener(|this, _: &WsEscape, window, cx| {
                if this.ws_confirm_delete {
                    this.ws_confirm_delete = false;
                    cx.notify();
                } else {
                    this.focus_terminal(Some(window), cx);
                }
            }))
            .child(
                div()
                    .flex_none()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_b_1()
                    .border_color(border)
                    .flex()
                    .flex_row()
                    .items_center()
                    .text_color(blend(theme.foreground, theme.background, 0.3))
                    .child(div().flex_1().child("Workspaces"))
                    .child(
                        div()
                            .id("ws-add")
                            .px_1()
                            .text_color(dim)
                            .cursor_pointer()
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.new_workspace(window, cx);
                                }),
                            )
                            .child("+"),
                    ),
            )
            .child(list)
            .when_some(self.ws_footer_text(), |d, text| {
                d.child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(border)
                        .text_color(theme.ansi[3])
                        .child(text),
                )
            })
    }

    fn render_ws_context_menu(&self, window: &Window, cx: &Context<Self>) -> gpui::Div {
        let Some(menu) = &self.ws_context_menu else {
            return div();
        };
        let Some(ws) = self.workspaces.get(menu.ix) else {
            return div();
        };
        let theme = &self.theme;
        let ix = menu.ix;
        let panel_bg = blend(theme.background, gpui::black(), 0.2);
        let border = blend(theme.foreground, theme.background, 0.8);
        let mut hover_bg = theme.selection_bg;
        hover_bg.a = 0.6;

        // Keep the menu on screen when the click lands near an edge.
        let viewport = window.viewport_size();
        let (menu_w, menu_h) = (200.0, 130.0);
        let x = f32::from(menu.position.x).min(f32::from(viewport.width) - menu_w - 8.0);
        let y = f32::from(menu.position.y).min(f32::from(viewport.height) - menu_h - 8.0);

        let item = |id: &'static str, label: String| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .child(label)
        };

        div()
            .absolute()
            .inset_0()
            // Backdrop: the first click anywhere else just dismisses.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                    this.ws_context_menu = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                    this.ws_context_menu = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(menu_w))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(panel_bg)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .text_size(px(13.0))
                    .text_color(theme.foreground)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        |_: &gpui::MouseDownEvent, _w, cx| cx.stop_propagation(),
                    )
                    .child(item("ws-menu-rename", "Rename".into()).on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            this.ws_context_menu = None;
                            this.ws_selected = ix;
                            window.focus(&this.ws_focus);
                            this.open_ws_rename(ix, window, cx);
                        }),
                    ))
                    .child(
                        item(
                            "ws-menu-pin",
                            if ws.persist {
                                "Unpin".into()
                            } else {
                                "Pin".into()
                            },
                        )
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, _w, cx| {
                                this.ws_context_menu = None;
                                if let Some(ws) = this.workspaces.get_mut(ix) {
                                    ws.persist = !ws.persist;
                                    this.save_workspaces(cx);
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        item("ws-menu-startup", "Edit Startup Commands…".into()).on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                this.ws_context_menu = None;
                                this.ws_selected = ix;
                                this.open_startup_editor(ix, window, cx);
                            }),
                        ),
                    )
                    .child(div().h(px(1.0)).my_1().bg(border))
                    .child(
                        item("ws-menu-delete", "Delete".into())
                            .text_color(theme.ansi[1])
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.ws_context_menu = None;
                                    this.ws_selected = ix;
                                    // Same y/n confirmation the keyboard flow uses.
                                    this.ws_confirm_delete = true;
                                    window.focus(&this.ws_focus);
                                    cx.notify();
                                }),
                            ),
                    ),
            )
    }

    /// Linux: which bar the ☰ menu button floats over. A top status bar
    /// spans the full width, so it is the corner whenever it's shown; then
    /// the drawer's header; then the tab bar. With all three hidden the
    /// button floats over the terminal's padding. `None` on macOS, where
    /// the real menu bar does this job.
    fn app_menu_corner(&self) -> Option<AppMenuCorner> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        let status_bar = self
            .status_bar_override
            .unwrap_or(self.config.status_bar.enabled);
        let tab_bar = self.tab_bar_override.unwrap_or(self.config.tabs.enabled);
        if status_bar && self.config.status_bar.position == StatusBarPosition::Top {
            Some(AppMenuCorner::StatusBar)
        } else if self.drawer_visible {
            Some(AppMenuCorner::Tree)
        } else if tab_bar {
            Some(AppMenuCorner::TabBar)
        } else {
            None
        }
    }

    /// The keymap's binding for a menu entry, for the shortcut column.
    fn shortcut_for(&self, action: &dyn Action) -> Option<String> {
        let meta = registry::all()
            .iter()
            .find(|m| (m.build)().partial_eq(action))?;
        self.keymap
            .display_for(meta.id)
            .map(|e| pretty_keys(&e.keys))
    }

    /// Open the ☰ menu on its first heading, or close it. Also the
    /// `app::menu` action, for keyboards and for when every bar is hidden.
    fn toggle_app_menu(&mut self, cx: &mut Context<Self>) {
        self.app_menu = match self.app_menu {
            Some(_) => None,
            None => Some(AppMenu { open: 0 }),
        };
        cx.notify();
    }

    /// Linux only: the ☰ button that stands in for the macOS menu bar. It
    /// floats over the top-left corner, whichever bar happens to be there;
    /// `app_menu_corner` tells that bar to pad for it.
    fn render_app_menu_button(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = &self.theme;
        let open = self.app_menu.is_some();
        let fg = theme.foreground;
        let dim = blend(theme.foreground, theme.background, 0.4);
        let mut hover_bg = theme.foreground;
        hover_bg.a = 0.1;
        div()
            .id("app-menu-button")
            .absolute()
            .top(px(APP_MENU_BUTTON_TOP))
            .left(px(APP_MENU_BUTTON_LEFT))
            .size(px(APP_MENU_BUTTON_SIZE))
            .rounded_md()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(if open { fg } else { dim })
            .when(open, |d| d.bg(hover_bg))
            .hover(move |s| s.bg(hover_bg).text_color(fg))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                    this.toggle_app_menu(cx);
                }),
            )
            // nf-fa-bars from the bundled Nerd Font.
            .child("\u{f0c9}")
    }

    /// The ☰ popover: the app menus down the left, the open one's entries
    /// on the right, each with its binding. Built from the same `menus()`
    /// list macOS installs in the menu bar, so the two never drift.
    fn render_app_menu(&self, window: &Window, cx: &Context<Self>) -> gpui::Div {
        let Some(menu) = &self.app_menu else {
            return div();
        };
        let menus = crate::menus();
        let open = menu.open.min(menus.len().saturating_sub(1));
        let theme = &self.theme;
        let panel_bg = blend(theme.background, gpui::black(), 0.2);
        let border = blend(theme.foreground, theme.background, 0.8);
        let dim = blend(theme.foreground, theme.background, 0.45);
        let mut hover_bg = theme.selection_bg;
        hover_bg.a = 0.6;
        let mut open_bg = theme.ansi[4];
        open_bg.a = 0.16;

        let mut headings = div()
            .flex_none()
            .w(px(112.0))
            .p_1()
            .border_r_1()
            .border_color(border)
            .flex()
            .flex_col();
        for (ix, m) in menus.iter().enumerate() {
            let is_open = ix == open;
            headings = headings.child(
                div()
                    .id(("app-menu-heading", ix))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .when(is_open, |d| d.bg(open_bg))
                    .when(!is_open, |d| d.hover(move |s| s.bg(hover_bg)))
                    // Hovering a heading opens it, the way a menu bar does
                    // once one menu is down; clicking covers touchpads that
                    // don't hover.
                    .on_hover(cx.listener(move |this, hovered: &bool, _w, cx| {
                        if *hovered
                            && let Some(menu) = &mut this.app_menu
                            && menu.open != ix
                        {
                            menu.open = ix;
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, _w, cx| {
                            if let Some(menu) = &mut this.app_menu {
                                menu.open = ix;
                                cx.notify();
                            }
                        }),
                    )
                    .child(m.name.clone())
                    .child(div().flex_none().text_color(dim).child("›")),
            );
        }

        let mut items = div().flex_1().min_w(px(240.0)).p_1().flex().flex_col();
        for (ix, item) in menus[open].items.iter().enumerate() {
            match item {
                MenuItem::Separator => {
                    items = items.child(div().h(px(1.0)).my_1().mx_2().bg(border));
                }
                MenuItem::Action { name, action, .. } => {
                    let keys = self.shortcut_for(action.as_ref());
                    let action = action.boxed_clone();
                    items = items.child(
                        div()
                            // Distinct per menu so hover state doesn't carry
                            // over to the same row of the next one.
                            .id(("app-menu-item", open * 100 + ix))
                            .px_3()
                            .py_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .hover(move |s| s.bg(hover_bg))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.app_menu = None;
                                    // On the focused element, so it reaches
                                    // the same handlers a key binding would.
                                    window.dispatch_action(action.boxed_clone(), cx);
                                    cx.notify();
                                }),
                            )
                            .child(name.clone())
                            .when_some(keys, |d, keys| {
                                d.child(div().flex_none().text_color(dim).child(keys))
                            }),
                    );
                }
                // Nested and OS-managed submenus: none of ours use them.
                MenuItem::Submenu(_) | MenuItem::SystemMenu(_) => {}
            }
        }

        let top = APP_MENU_BUTTON_TOP + APP_MENU_BUTTON_SIZE + 4.0;
        let max_h = f32::from(window.viewport_size().height) - top - 8.0;

        div()
            .absolute()
            .inset_0()
            // Backdrop: the first click anywhere else just dismisses, and
            // goes no further — not to the terminal, and not to the ☰
            // button, which would only reopen the menu.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                    this.app_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                    this.app_menu = None;
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .absolute()
                    .left(px(APP_MENU_BUTTON_LEFT))
                    .top(px(top))
                    .max_h(px(max_h.max(60.0)))
                    .overflow_hidden()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .bg(panel_bg)
                    .shadow_lg()
                    .flex()
                    .flex_row()
                    .items_start()
                    .text_size(px(13.0))
                    .text_color(theme.foreground)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        |_: &gpui::MouseDownEvent, _w, cx| cx.stop_propagation(),
                    )
                    .child(headings)
                    .child(items),
            )
    }

    fn render_status_bar(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = &self.theme;
        let dim = blend(theme.foreground, theme.background, 0.35);
        let bar_bg = translucent(
            blend(theme.background, gpui::black(), 0.25),
            self.config.window.opacity,
        );
        // Linux: on top, this bar is the corner the ☰ menu button floats over.
        let under_menu_button = self.app_menu_corner() == Some(AppMenuCorner::StatusBar);
        let cwd_text = self
            .active_pane()
            .read(cx)
            .cwd
            .as_ref()
            .map(|cwd| pretty_path(cwd, home_dir().as_deref()))
            .unwrap_or_default();
        let git = &self.git_status;

        div()
            .flex_none()
            .h(px(26.0))
            .px_3()
            .when(under_menu_button, |d| d.pl(px(APP_MENU_BUTTON_CLEARANCE)))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .bg(bar_bg)
            .text_size(px(12.0))
            .text_color(dim)
            .child({
                // Which workspace you're in, next to the directory.
                let accent = theme.ansi[4];
                let mut chip_bg = accent;
                chip_bg.a = 0.16;
                div()
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .bg(chip_bg)
                    .text_color(accent)
                    .child(self.ws().name.clone())
            })
            .child({
                // Which tab, for when the tab bar is hidden. Pulled left to
                // sit against the workspace chip as one "where am I" unit.
                let color = theme.ansi[6];
                let mut chip_bg = color;
                chip_bg.a = 0.16;
                let label = match self.config.status_bar.tab {
                    StatusBarTab::Number => {
                        format!("{}/{}", self.ws().active_tab + 1, self.ws().tabs.len())
                    }
                    StatusBarTab::Name => self.tab_title(self.tab(), cx),
                };
                div()
                    .flex_none()
                    .ml(px(-6.0))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .bg(chip_bg)
                    .text_color(color)
                    .child(label)
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .overflow_hidden()
                    .child(div().text_color(theme.ansi[4]).child("\u{f07b}"))
                    .child(cwd_text),
            )
            .when(self.tab().broadcast, |d| {
                // Loud on purpose: `rm -rf build` into four panes is not a
                // mistake anyone forgives a subtle dot for.
                d.child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(theme.ansi[1])
                        .text_color(theme.background)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("⇶ BROADCAST"),
                )
            })
            .when(self.tab().zoomed.is_some(), |d| {
                // A zoomed pane looks exactly like a one-pane tab otherwise.
                let mut chip_bg = theme.ansi[3];
                chip_bg.a = 0.18;
                d.child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(chip_bg)
                        .text_color(theme.ansi[3])
                        .child(format!("⤢ zoom · {} panes", self.tab().layout.len())),
                )
            })
            .when_some(self.active_pane().read(cx).vi_mode(), |d, kind| {
                d.child(
                    div()
                        .flex_none()
                        .px_2()
                        .py_0p5()
                        .rounded_sm()
                        .bg(theme.ansi[3])
                        .text_color(theme.background)
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(kind.label()),
                )
            })
            .when_some(
                self.active_pane().read(cx).ssh_host().map(str::to_string),
                |d, host| {
                    // Where you are, when it isn't this machine. A configured
                    // host accent colours the chip too.
                    let color = self
                        .config
                        .ssh
                        .accent_for(&host)
                        .and_then(parse_hex)
                        .unwrap_or(theme.ansi[5]);
                    let mut chip_bg = color;
                    chip_bg.a = 0.18;
                    d.child(
                        div()
                            .flex_none()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .bg(chip_bg)
                            .text_color(color)
                            .child(format!("ssh: {host}")),
                    )
                },
            )
            .child({
                // What the focused pane is doing: elapsed time while a
                // command runs; the last failure once it's done. Success is
                // silent, like the prompt's exit segment.
                let log = &self.active_pane().read(cx).log;
                if let Some(cmd) = log.running() {
                    div().flex_none().text_color(theme.ansi[4]).child(format!(
                        "⟳ {}  {}",
                        cmd.label(),
                        format_duration(cmd.duration())
                    ))
                } else if let Some(cmd) = log.last_finished().filter(|c| c.failed()) {
                    div().flex_none().text_color(theme.ansi[1]).child(format!(
                        "✗ {} · {}",
                        cmd.exit.unwrap_or(1),
                        format_duration(cmd.duration())
                    ))
                } else {
                    div().flex_none()
                }
            })
            .child(div().flex_1())
            .when_some(git.branch.clone(), |d, branch| {
                let branch_color = if git.dirty {
                    theme.ansi[3]
                } else {
                    theme.ansi[2]
                };
                let mut label = format!("\u{e0a0} {branch}");
                if git.dirty {
                    label.push_str(" ●");
                }
                if git.ahead > 0 {
                    label.push_str(&format!(" ⇡{}", git.ahead));
                }
                if git.behind > 0 {
                    label.push_str(&format!(" ⇣{}", git.behind));
                }
                d.child(div().text_color(branch_color).child(label))
            })
    }
}

/// Every file under `root` as root-relative strings, shallow first, with
/// the same gitignore and hidden-file rules as the drawer. Capped; the
/// second value says whether the cap was hit.
fn walk_files(root: &Path, respect_gitignore: bool, show_hidden: bool) -> (Vec<String>, bool) {
    let walk = ignore::WalkBuilder::new(root)
        .hidden(!show_hidden)
        .parents(respect_gitignore)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .follow_links(false)
        .build();
    let mut out = Vec::new();
    let mut truncated = false;
    for entry in walk.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if rel.file_name().is_some_and(|n| n == ".DS_Store") {
            continue;
        }
        if out.len() >= FINDER_INDEX_CAP {
            truncated = true;
            break;
        }
        out.push(rel.to_string_lossy().to_string());
    }
    out.sort_by(|a, b| {
        a.matches('/')
            .count()
            .cmp(&b.matches('/').count())
            .then_with(|| a.cmp(b))
    });
    (out, truncated)
}

/// A palette row's title with the matched characters picked out.
/// `$HOME` as `~`, for compact directory labels.
fn pretty_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Text with the matched character positions picked out in the accent.
fn highlighted_text(text: &str, highlights: &[usize], accent: gpui::Hsla) -> gpui::StyledText {
    let mut ranges: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::new();
    let byte_offsets: Vec<(usize, usize)> = text
        .char_indices()
        .map(|(b, c)| (b, b + c.len_utf8()))
        .collect();
    for &pos in highlights {
        let Some(&(start, end)) = byte_offsets.get(pos) else {
            continue;
        };
        // Merge with the previous range when adjacent, so a run is one span.
        if let Some((last, _)) = ranges.last_mut()
            && last.end == start
        {
            last.end = end;
            continue;
        }
        ranges.push((
            start..end,
            gpui::HighlightStyle {
                color: Some(accent),
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            },
        ));
    }
    gpui::StyledText::new(text.to_string()).with_highlights(ranges)
}

/// Rewrite the `[colors]` section of config.toml to just the chosen preset,
/// preserving the rest of the file's comments and formatting. Explicit color
/// keys are dropped deliberately — they override presets, so leaving them
/// would make the newly chosen theme a no-op.
///
/// With `variant` (`preset_dark` / `preset_light`, when following the
/// system appearance) only that key changes and the rest of the block —
/// `follow_system`, the other variant, any overrides — is kept.
fn persist_preset(name: &str, variant: Option<&str>) -> Result<(), String> {
    let path = config::config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("couldn't edit config: {e}"))?;
    match variant {
        Some(key) => {
            if !doc.contains_table("colors") {
                doc["colors"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            doc["colors"][key] = toml_edit::value(name);
        }
        None => {
            let mut colors = toml_edit::Table::new();
            colors["preset"] = toml_edit::value(name);
            doc["colors"] = toml_edit::Item::Table(colors);
        }
    }
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("couldn't write config: {e}"))
}

/// Write the answer to the crash-reports question into config.toml, keeping
/// the rest of the file as it is. The live reload then sees it: off stops a
/// running copy at once, on waits for the next launch (see telemetry::init).
fn persist_crash_reports(enabled: bool) -> Result<(), String> {
    let path = config::config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("couldn't edit config: {e}"))?;
    if !doc.contains_table("crash_reports") {
        doc["crash_reports"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["crash_reports"]["enabled"] = toml_edit::value(enabled);
    std::fs::write(&path, doc.to_string()).map_err(|e| format!("couldn't write config: {e}"))
}

/// What GPUI is asked for. On macOS the blur isn't GPUI's to do: see
/// `set_window_blur`.
fn window_background(translucent: bool, blur: bool) -> gpui::WindowBackgroundAppearance {
    match (translucent, blur) {
        (false, _) => gpui::WindowBackgroundAppearance::Opaque,
        (true, true) if !cfg!(target_os = "macos") => gpui::WindowBackgroundAppearance::Blurred,
        (true, _) => gpui::WindowBackgroundAppearance::Transparent,
    }
}

/// How far the blur behind a translucent window reaches, in points.
#[cfg(target_os = "macos")]
const WINDOW_BLUR_RADIUS: i64 = 30;

/// Blur what's behind the window, or stop. This is the WindowServer call
/// Terminal.app uses. GPUI's own blur — an `NSVisualEffectView` with its
/// tint stripped out — leaves what's behind the window sharp on current
/// macOS.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // objc's macros test a cfg rustc doesn't know
fn set_window_blur(window: &Window, on: bool) {
    use objc::runtime::Object;
    use objc::{msg_send, sel, sel_impl};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::ffi::c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSMainConnectionID() -> *mut c_void;
        fn CGSSetWindowBackgroundBlurRadius(
            connection: *mut c_void,
            window: isize,
            radius: i64,
        ) -> i32;
    }

    // Spelled out: `Window::window_handle` is GPUI's own handle.
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return;
    };
    let radius = if on { WINDOW_BLUR_RADIUS } else { 0 };
    unsafe {
        let view = appkit.ns_view.as_ptr() as *mut Object;
        let ns_window: *mut Object = msg_send![view, window];
        if !ns_window.is_null() {
            let number: isize = msg_send![ns_window, windowNumber];
            CGSSetWindowBackgroundBlurRadius(CGSMainConnectionID(), number, radius);
        }
    }
}

/// Open an OmniPTY window: shared by startup and the NewWindow action.
/// `command` is the `-e` program for the first pane, only ever set for the
/// window a launch opens; windows made from inside the app get a shell.
pub fn open_omnipty_window(
    config: Config,
    config_error: Option<String>,
    cwd: Option<PathBuf>,
    command: Option<Vec<String>>,
    restore: bool,
    cx: &mut gpui::App,
) {
    let window_background = window_background(config.window.opacity < 1.0, config.window.blur);

    // `window.titlebar` is a macOS setting: on Linux the compositor owns
    // decorations (GPUI asks for server-side ones; Hyprland and friends draw
    // none anyway), so the window is always the bare content.
    let titlebar = match config.window.titlebar {
        TitlebarMode::Hidden if cfg!(target_os = "macos") => gpui::TitlebarOptions {
            title: Some("omnipty".into()),
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(12.0), px(10.0))),
        },
        _ => gpui::TitlebarOptions {
            title: Some("omnipty".into()),
            appears_transparent: false,
            traffic_light_position: None,
        },
    };

    let bounds = load_window_bounds()
        .unwrap_or_else(|| gpui::Bounds::centered(None, gpui::size(px(1200.0), px(800.0)), cx));
    cx.open_window(
        gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            titlebar: Some(titlebar),
            focus: true,
            window_background,
            window_min_size: Some(gpui::size(px(400.0), px(300.0))),
            // Wayland app_id / X11 WM_CLASS: what window rules and the
            // .desktop file's StartupWMClass match on. macOS ignores it.
            // `--app-id` overrides it for the whole process, the way
            // xdg-terminal-exec expects when it launches a TUI under its own id.
            app_id: Some(
                crate::cli::cli()
                    .app_id
                    .clone()
                    .unwrap_or_else(|| "omnipty".into()),
            ),
            ..Default::default()
        },
        |window, cx| {
            cx.new(|cx| OmniPTY::new(config, config_error, cwd, command, restore, window, cx))
        },
    )
    .expect("failed to open window");
    cx.activate(true);
}

impl Render for OmniPTY {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Focus can move without going through our actions — clicking a pane
        // is the common case — so adopt whatever is actually focused before
        // anything reads `active`. Otherwise close/split/status-bar all act on
        // a stale pane. Focus on the drawer leaves the last active pane alone.
        let focused_pane = self.tab().layout.leaves().into_iter().find(|id| {
            self.panes
                .get(id)
                .is_some_and(|p| p.focus_handle(cx).is_focused(window))
        });
        if let Some(id) = focused_pane
            && self.active_id() != id
        {
            self.tab_mut().active = id;
            self.sync_tree_to_active(cx);
        }

        // Root bindings still fire under an overlay (cmd-1 switches tabs and
        // focuses that tab's pane, say). An overlay that lost focus that way
        // would linger unreachable, so drop it; a theme preview reverts.
        if self.overlay.is_some()
            && !self.picker_focus.is_focused(window)
            && let Some(Overlay::ThemePicker(picker)) = self.overlay.take()
        {
            let original = picker.original;
            let entity = cx.entity();
            // Deferred: entity updates aren't allowed from inside render.
            cx.defer(move |cx| {
                entity.update(cx, |this, cx| this.apply_theme(original, cx));
            });
        }

        self.tab_mut().unread = false;

        let theme = self.theme.clone();
        let config = self.config.clone();

        // Opacity repaints on its own; whether the window is see-through at
        // all, and blurred, is the window's to be told — at first paint and
        // after a config reload.
        let translucency = (config.window.opacity < 1.0, config.window.blur);
        if self.translucency != Some(translucency) {
            self.translucency = Some(translucency);
            let (see_through, blur) = translucency;
            window.set_background_appearance(window_background(see_through, blur));
            #[cfg(target_os = "macos")]
            set_window_blur(window, see_through && blur);
        }

        let title = self.active_pane().read(cx).title.clone();
        window.set_window_title(&title);

        let bounds = window.bounds();
        if self.last_bounds != Some(bounds) {
            self.save_bounds_debounced(bounds, cx);
        }

        let status_bar = self
            .status_bar_override
            .unwrap_or(self.config.status_bar.enabled);
        let tab_bar = self.tab_bar_override.unwrap_or(self.config.tabs.enabled);
        let bar_on_top = self.config.status_bar.position == StatusBarPosition::Top;

        // Linux: the drawer pads its header for the ☰ menu button only
        // while it is the bar in the corner.
        let menu_over_tree = self.app_menu_corner() == Some(AppMenuCorner::Tree);
        if self.tree.read(cx).app_menu_clearance != menu_over_tree {
            self.tree.update(cx, |tree, cx| {
                tree.app_menu_clearance = menu_over_tree;
                cx.notify();
            });
        }

        let tree_focused = self.tree_focus(cx).is_focused(window);
        let accent = theme.ansi[4];
        let drawer_width = self.drawer_width(window);
        // Like the pane dividers' grab areas: not under a modal or a menu.
        let drawer_resizable = self.drawer_visible
            && self.overlay.is_none()
            && self.ws_context_menu.is_none()
            && self.app_menu.is_none();
        // The 30px band is where macOS draws its traffic lights over our
        // content; Linux windows have no such inset.
        let hidden_titlebar =
            cfg!(target_os = "macos") && config.window.titlebar == TitlebarMode::Hidden;

        // One layer per region — the bars, the drawer's two halves, the
        // panes — each at `window.opacity`, and none here underneath them
        // when that's below 1: two layers at 0.5 are a window at 0.75.
        let background = translucent(theme.background, config.window.opacity);

        div()
            .key_context("Root")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .when(config.window.opacity >= 1.0, |d| d.bg(background))
            .on_modifiers_changed(
                cx.listener(|this, ev: &gpui::ModifiersChangedEvent, _w, cx| {
                    this.on_launch_modifiers(ev.modifiers, cx);
                }),
            )
            .font_family(config.font.family.primary().to_string())
            .text_size(px(13.0))
            .text_color(theme.foreground)
            .when(hidden_titlebar, |d| {
                // The padding band doubles as the titlebar: double-click
                // zooms (respecting the System Settings double-click action),
                // matching what a real titlebar would do. Rendered first so
                // overlays like the update pill still get their clicks.
                d.pt(px(30.0)).child(
                    div()
                        .id("titlebar-strip")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(30.0))
                        .bg(background)
                        .window_control_area(gpui::WindowControlArea::Drag)
                        .on_click(|event, window, _cx| {
                            if event.click_count() >= 2 {
                                window.titlebar_double_click();
                            }
                        }),
                )
            })
            .on_action(cx.listener(|this, _: &FocusTree, window, cx| {
                this.focus_tree(Some(window), cx);
            }))
            .on_action(cx.listener(|this, _: &FocusTerminal, window, cx| {
                this.focus_terminal(Some(window), cx);
            }))
            .on_action(cx.listener(|this, _: &FocusToggle, window, cx| {
                if this.tree_focus(cx).is_focused(window) {
                    this.focus_terminal(Some(window), cx);
                } else {
                    this.focus_tree(Some(window), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleDrawer, window, cx| {
                this.drawer_visible = !this.drawer_visible;
                if !this.drawer_visible && this.tree_focus(cx).is_focused(window) {
                    window.focus(&this.term_focus(cx));
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &FontIncrease, _w, cx| {
                this.for_each_pane(cx, |t, cx| t.adjust_font(Some(1.0), cx));
            }))
            .on_action(cx.listener(|this, _: &FontDecrease, _w, cx| {
                this.for_each_pane(cx, |t, cx| t.adjust_font(Some(-1.0), cx));
            }))
            .on_action(cx.listener(|this, _: &FontReset, _w, cx| {
                this.for_each_pane(cx, |t, cx| t.adjust_font(None, cx));
            }))
            .on_action(cx.listener(|this, _: &NewWindow, _w, cx| {
                let cwd = this.active_pane().read(cx).cwd.clone();
                let (config, error) = config::load();
                open_omnipty_window(config, error, cwd, None, false, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                this.new_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewWorkspace, window, cx| {
                this.new_workspace(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusWorkspaces, window, cx| {
                this.focus_workspaces_panel(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectNextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPreviousTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &SelectTab1, window, cx| this.select_tab(0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab2, window, cx| this.select_tab(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab3, window, cx| this.select_tab(2, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab4, window, cx| this.select_tab(3, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab5, window, cx| this.select_tab(4, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab6, window, cx| this.select_tab(5, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab7, window, cx| this.select_tab(6, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab8, window, cx| this.select_tab(7, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab9, window, cx| this.select_tab(8, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectWorkspace1, window, cx| {
                this.select_workspace(0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace2, window, cx| {
                this.select_workspace(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace3, window, cx| {
                this.select_workspace(2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace4, window, cx| {
                this.select_workspace(3, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace5, window, cx| {
                this.select_workspace(4, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace6, window, cx| {
                this.select_workspace(5, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace7, window, cx| {
                this.select_workspace(6, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace8, window, cx| {
                this.select_workspace(7, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace9, window, cx| {
                this.select_workspace(8, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split_active(Direction::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitLeft, window, cx| {
                this.split_active(Direction::Left, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitUp, window, cx| {
                this.split_active(Direction::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split_active(Direction::Down, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneLeft, window, cx| {
                this.focus_in_direction(Direction::Left, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, window, cx| {
                this.focus_in_direction(Direction::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, window, cx| {
                this.focus_in_direction(Direction::Up, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, window, cx| {
                this.focus_in_direction(Direction::Down, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ClosePane, window, cx| {
                let id = this.active_id();
                if !this.close_pane(id, window, cx) {
                    window.remove_window();
                }
            }))
            // cmd-w closes the focused split first; the window goes with the
            // last pane, which is what every other terminal does.
            .on_action(cx.listener(|this, _: &CloseWindow, window, cx| {
                let id = this.active_id();
                if !this.close_pane(id, window, cx) {
                    window.remove_window();
                }
            }))
            .on_action(|_: &Minimize, window, _cx| window.minimize_window())
            .on_action(|_: &Zoom, window, _cx| window.zoom_window())
            .on_action(|_: &ToggleFullscreen, window, _cx| window.toggle_fullscreen())
            .on_action(cx.listener(|this, _: &ToggleStatusBar, _w, cx| {
                let current = this
                    .status_bar_override
                    .unwrap_or(this.config.status_bar.enabled);
                this.status_bar_override = Some(!current);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleTabBar, _w, cx| {
                let current = this.tab_bar_override.unwrap_or(this.config.tabs.enabled);
                this.tab_bar_override = Some(!current);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                let command = edit_file_command(&config::config_path(), &this.shell_program());
                this.run_at_prompt(command, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectTheme, window, cx| {
                this.open_theme_picker(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| {
                this.toggle_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CommandHistory, window, cx| {
                this.toggle_history(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FileFinder, window, cx| {
                this.toggle_finder(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RevealInTree, window, cx| {
                let Some(cwd) = this.active_pane().read(cx).cwd.clone() else {
                    return;
                };
                this.drawer_visible = true;
                this.tree.update(cx, |tree, cx| tree.reveal(cwd, cx));
                this.focus_tree(Some(window), cx);
            }))
            // Resize steps are in cells, converted to a share of the split at
            // apply time; a few columns per press feels right for a keyboard.
            .on_action(cx.listener(|this, _: &PaneWider, _w, cx| {
                this.resize_active(Axis::Horizontal, 4.0, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneNarrower, _w, cx| {
                this.resize_active(Axis::Horizontal, -4.0, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneTaller, _w, cx| {
                this.resize_active(Axis::Vertical, 2.0, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneShorter, _w, cx| {
                this.resize_active(Axis::Vertical, -2.0, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneEqualize, _w, cx| {
                this.equalize_splits(cx);
            }))
            .on_action(cx.listener(|this, _: &PaneZoom, _w, cx| this.toggle_zoom(cx)))
            .on_action(cx.listener(|this, _: &PaneBroadcast, _w, cx| this.toggle_broadcast(cx)))
            .on_action(cx.listener(|this, _: &PaneOnly, window, cx| {
                this.close_other_panes_prompt(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PaneSwap, _w, cx| this.swap_active_pane(cx)))
            .on_action(cx.listener(|this, _: &RenameTab, window, cx| {
                let ix = this.ws().active_tab;
                this.open_tab_rename(ix, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                this.reopen_closed_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SetStartupCommand, window, cx| {
                let id = this.active_id();
                this.open_startup_command(id, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveTabLeft, _w, cx| this.move_active_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &MoveTabRight, _w, cx| this.move_active_tab(1, cx)))
            .on_action(cx.listener(|this, _: &CheckForUpdates, _w, cx| {
                this.check_for_updates(true, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowChangelog, window, cx| {
                this.open_changelog_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &InstallUpdate, _w, cx| {
                this.install_update(cx);
            }))
            .on_action(cx.listener(|this, _: &ShowAppMenu, _w, cx| {
                this.toggle_app_menu(cx);
            }))
            .on_action(cx.listener(|this, _: &About, window, cx| {
                this.open_about(window, cx);
            }))
            .when(status_bar && bar_on_top, |d| {
                d.child(self.render_status_bar(cx))
            })
            .child(
                div()
                    .flex_1()
                    .relative()
                    .flex()
                    .flex_row()
                    .min_h_0()
                    .child(
                        div()
                            .flex_none()
                            .h_full()
                            .overflow_hidden()
                            .w(if self.drawer_visible {
                                px(drawer_width)
                            } else {
                                px(0.0)
                            })
                            .when(self.drawer_visible, |d| {
                                let drawer_focused =
                                    tree_focused || self.ws_focus.is_focused(window);
                                d.border_r_1()
                                    .border_color(if drawer_focused {
                                        accent
                                    } else {
                                        blend(theme.foreground, theme.background, 0.85)
                                    })
                                    .flex()
                                    .flex_col()
                                    // 50/50: file tree above, workspaces below.
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_hidden()
                                            .child(self.tree.clone()),
                                    )
                                    .child(self.render_workspace_panel(window, cx).bg(background))
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .when(tab_bar, |d| d.child(self.render_tab_bar(cx)))
                            .child({
                                // Zoomed: just that leaf, at full size. The
                                // hidden panes aren't laid out, so their
                                // PTYs keep their size until they're back.
                                let tab = self.tab();
                                let layout = match tab.zoomed {
                                    Some(id) if tab.layout.leaves().contains(&id) => Node::Leaf(id),
                                    _ => tab.layout.clone(),
                                };
                                div().flex_1().min_h_0().overflow_hidden().child(
                                    self.render_pane_node(&layout, &Vec::new(), accent, window, cx),
                                )
                            }),
                    )
                    // The drawer's right edge drags like a pane divider.
                    .when(drawer_resizable, |d| {
                        d.child(
                            div()
                                .id("drawer-resize")
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(drawer_width - 4.0))
                                .w(px(7.0))
                                .cursor(gpui::CursorStyle::ResizeLeftRight)
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|this, ev: &gpui::MouseDownEvent, _w, cx| {
                                        cx.stop_propagation();
                                        if ev.click_count > 1 {
                                            // Double-click: back to `tree.width`.
                                            this.drawer_width = None;
                                            this.write_window_state();
                                        } else {
                                            this.drawer_drag = true;
                                        }
                                        cx.notify();
                                    }),
                                ),
                        )
                    }),
            )
            .when(status_bar && !bar_on_top, |d| {
                d.child(self.render_status_bar(cx))
            })
            .child(self.render_toasts(status_bar && !bar_on_top, cx))
            // Linux has no menu bar: the ☰ button in the corner is the
            // same menus as a popover. Painted after the bars so it sits
            // on top of whichever one is there.
            .when(cfg!(target_os = "linux"), |d| {
                d.child(self.render_app_menu_button(cx))
            })
            .map(|d| match &self.update {
                UpdateState::Available { version, .. } => d.child(
                    div()
                        .id("update-available")
                        .absolute()
                        .top(px(5.0))
                        .right(px(8.0))
                        .px_3()
                        .py_0p5()
                        .rounded_full()
                        .bg(accent)
                        .text_size(px(11.0))
                        .text_color(theme.background)
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                                this.install_update(cx);
                            }),
                        )
                        .child(format!("↑ v{version} available — click for the release")),
                ),
                UpdateState::Downloading(version) => d.child(
                    div()
                        .absolute()
                        .top(px(5.0))
                        .right(px(8.0))
                        .px_3()
                        .py_0p5()
                        .rounded_full()
                        .bg(blend(theme.background, theme.foreground, 0.08))
                        .text_size(px(11.0))
                        .text_color(blend(theme.foreground, theme.background, 0.4))
                        .child(format!("downloading v{version}…")),
                ),
                UpdateState::Ready { version, .. } => d.child(
                    div()
                        .id("install-update")
                        .absolute()
                        .top(px(5.0))
                        .right(px(8.0))
                        .px_3()
                        .py_0p5()
                        .rounded_full()
                        .bg(accent)
                        .text_size(px(11.0))
                        .text_color(theme.background)
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _: &gpui::MouseDownEvent, _w, cx| {
                                this.install_update(cx);
                            }),
                        )
                        .child(format!("↓ v{version} ready — click to install")),
                ),
                _ => d,
            })
            .when(self.ws_context_menu.is_some(), |d| {
                d.child(self.render_ws_context_menu(window, cx))
            })
            .when(self.app_menu.is_some(), |d| {
                d.child(self.render_app_menu(window, cx))
            })
            .when(self.divider_drag.is_some() || self.drawer_drag, |d| {
                d.child(self.render_drag_overlay(cx))
            })
            .when(self.overlay.is_some(), |d| d.child(self.render_overlay(cx)))
            // Another app is frontmost: a shade in the background's colour,
            // over the whole window. Takes no mouse events, so the first
            // click still lands where it was aimed.
            .when(
                !self.window_active && config.window.inactive_window_opacity < 1.0,
                |d| {
                    let mut shade = theme.background;
                    shade.a = 1.0 - config.window.inactive_window_opacity.clamp(0.05, 1.0);
                    d.child(div().absolute().inset_0().bg(shade))
                },
            )
    }
}

#[cfg(test)]
mod reorder_tests {
    use super::*;

    /// Dragging a tab or workspace must leave the active one active.
    #[test]
    fn indices_follow_their_items_through_a_move() {
        for (from, to) in [(0, 3), (3, 0), (1, 2), (2, 2)] {
            let mut items = vec!['a', 'b', 'c', 'd'];
            let moved = items.remove(from);
            items.insert(to, moved);
            for (ix, item) in ['a', 'b', 'c', 'd'].into_iter().enumerate() {
                let now = items.iter().position(|i| *i == item).unwrap();
                assert_eq!(
                    index_after_move(ix, from, to),
                    now,
                    "{item}: {from} -> {to}"
                );
            }
        }
    }

    #[test]
    fn a_new_workspace_takes_the_lowest_free_number() {
        let name = |names: &[&str]| default_ws_name(names.iter().copied());
        // Named workspaces, and ones deleted since, hold no number.
        assert_eq!(name(&["api", "site", "notes"]), "workspace 1");
        assert_eq!(name(&["api", "workspace 1"]), "workspace 2");
        assert_eq!(name(&["workspace 1", "workspace 3"]), "workspace 2");
    }

    #[test]
    fn a_dragged_drawer_leaves_room_for_the_terminal() {
        assert_eq!(clamp_drawer_width(20.0, 1200.0), 160.0);
        assert_eq!(clamp_drawer_width(400.0, 1200.0), 400.0);
        assert_eq!(clamp_drawer_width(1190.0, 1200.0), 960.0);
        // A window too small for both: the drawer keeps its minimum.
        assert_eq!(clamp_drawer_width(400.0, 300.0), 160.0);
    }
}

#[cfg(test)]
mod edit_command_tests {
    use super::*;
    use std::process::Command;

    /// What the image preview's command really prints, run by a shell the
    /// way the pane runs it: the inline-image sequence with the file's
    /// base64 inside, for a path that needs quoting.
    #[test]
    fn the_image_preview_command_prints_the_file_inline() {
        let dir = std::env::temp_dir().join(format!("omnipty img'test {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a picture.png");
        std::fs::write(&path, b"not really a png").unwrap();
        for (fit, sizing) in [(true, ";width=100%;height=100%"), (false, "")] {
            let out = Command::new("/bin/sh")
                .args(["-c", &image_preview_command(&path, fit)])
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let printed = String::from_utf8_lossy(&out.stdout).replace('\n', "");
            let expect = format!("\x1b]1337;File=inline=1{sizing}:bm90IHJlYWxseSBhIHBuZw==\x07");
            assert!(printed.contains(&expect), "{printed:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Shells someone might plausibly have as their login shell on a Mac.
    const CANDIDATES: &[&str] = &[
        "/bin/sh",
        "/bin/bash",
        "/bin/zsh",
        "/bin/dash",
        "/bin/ksh",
        "/bin/tcsh",
        "/bin/csh",
        "/usr/bin/fish",
        "/usr/bin/nu",
        "/usr/bin/elvish",
        "/usr/bin/xonsh",
        "/opt/homebrew/bin/bash",
        "/opt/homebrew/bin/fish",
        "/opt/homebrew/bin/nu",
        "/opt/homebrew/bin/elvish",
        "/opt/homebrew/bin/xonsh",
        "/usr/local/bin/fish",
        "/usr/local/bin/nu",
    ];

    /// Run the command OmniPTY would type for `target`, in `shell`, with an
    /// $EDITOR that records the path it was handed. Returns what the editor
    /// actually received, so both quoting layers are checked end to end.
    fn opened_path(shell: &str, target: &Path, label: &str) -> Result<String, String> {
        // Per-test scratch: these tests run in parallel and would otherwise
        // read each other's recorded path.
        let dir = std::env::temp_dir().join(format!("omnipty-edit-command-test-{label}"));
        std::fs::create_dir_all(&dir).unwrap();
        let recorder = dir.join("fake-editor.sh");
        let record = dir.join("opened.txt");
        std::fs::write(
            &recorder,
            format!("#!/bin/sh\nprintf '%s' \"$1\" > {}\n", record.display()),
        )
        .unwrap();
        Command::new("/bin/chmod")
            .arg("+x")
            .arg(&recorder)
            .status()
            .unwrap();
        let _ = std::fs::remove_file(&record);

        let command = edit_file_command(target, shell);
        let out = Command::new(shell)
            .arg("-c")
            .arg(&command)
            .env("EDITOR", &recorder)
            .output()
            .map_err(|e| format!("could not run {shell}: {e}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() || !stderr.trim().is_empty() {
            return Err(format!(
                "{shell} rejected the command:\n  {command}\n  status: {}\n  stderr: {stderr}",
                out.status
            ));
        }
        std::fs::read_to_string(&record)
            .map_err(|e| format!("{shell}: the editor never ran ({e})\n  command: {command}"))
    }

    fn installed_shells() -> Vec<&'static str> {
        CANDIDATES
            .iter()
            .copied()
            .filter(|s| Path::new(s).exists())
            .collect()
    }

    /// The core guarantee: whatever OmniPTY types has to parse in the shell that
    /// will run it, and open the right file. The Bourne one-liner is a syntax
    /// error in fish and the csh family, which is why non-POSIX shells get it
    /// via /bin/sh.
    #[test]
    fn edit_command_opens_the_right_path_in_every_installed_shell() {
        let shells = installed_shells();
        // macOS ships zsh, bash, sh, dash, ksh, tcsh and csh; a Linux box
        // may have nothing beyond sh and bash (CI installs fish, zsh and
        // dash on top).
        let minimum = if cfg!(target_os = "macos") { 3 } else { 2 };
        assert!(
            shells.len() >= minimum,
            "expected several shells to test against, found {shells:?}"
        );
        // A space is the everyday hard case — "Application Support" and the
        // like show up in real paths constantly.
        let target = std::env::temp_dir().join("omnipty-edit-command-test/a config file.toml");
        for shell in shells {
            match opened_path(shell, &target, "spaces") {
                Ok(opened) => {
                    assert_eq!(opened, target.to_string_lossy(), "{shell} mangled the path")
                }
                Err(e) => panic!("{e}"),
            }
        }
    }

    /// Paths containing characters no shell can quote uniformly. Split out
    /// from the test above so a failure here is unmistakably about exotic
    /// paths in one shell, not about whether OmniPTY works there at all.
    ///
    /// Each character here broke a real shell: `'` has no escape inside a
    /// nushell literal, fish reads `\` inside single quotes as an escape, and
    /// csh expands `!` even there.
    #[test]
    fn edit_command_handles_paths_no_shell_can_quote() {
        let cases = [
            ("apostrophe", "it's a config.toml"),
            ("bang", "bang!.toml"),
            ("backslash", "back\\slash.toml"),
            ("all-three", "it's a bang!back\\slash.toml"),
        ];
        for (label, name) in cases {
            let target = std::env::temp_dir()
                .join("omnipty-edit-command-test")
                .join(name);
            for shell in installed_shells() {
                match opened_path(shell, &target, label) {
                    Ok(opened) => {
                        assert_eq!(opened, target.to_string_lossy(), "{shell} mangled {name:?}")
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
    }

    /// The indirection file is consumed by the command that reads it, so a
    /// pane that opens a hundred files does not leave a hundred files behind.
    #[test]
    fn indirection_file_is_cleaned_up_after_use() {
        let target = std::env::temp_dir().join("omnipty-edit-command-test/it's cleaned.toml");
        let command = edit_file_command(&target, "/opt/homebrew/bin/fish");
        let name = command
            .split("/.cache/omnipty/edit/")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("command should reference an indirection file")
            .to_string();
        let file = directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .join(".cache/omnipty/edit")
            .join(&name);
        assert!(
            file.exists(),
            "the path was never written to {}",
            file.display()
        );

        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .env("EDITOR", "/usr/bin/true")
            .output()
            .unwrap();
        assert!(out.status.success(), "command failed: {command}");
        assert!(!file.exists(), "{} was left behind", file.display());
    }

    /// nushell rejected `'\''` — its single-quoted literals have no escape
    /// at all, so a quote can only ever end the string. Whatever OmniPTY types
    /// at a non-POSIX prompt must therefore never rely on that splice; the
    /// paths that would need it go through a file instead.
    #[test]
    fn non_posix_commands_never_use_the_quote_splice() {
        let hazards = [
            "/tmp/it's a config.toml",
            "/tmp/bang!.toml",
            "/tmp/back\\slash.toml",
            "/tmp/plain.toml",
            "/tmp/a space.toml",
        ];
        for shell in [
            "/opt/homebrew/bin/fish",
            "/bin/tcsh",
            "/opt/homebrew/bin/nu",
            "/usr/bin/elvish",
        ] {
            for hazard in hazards {
                let command = edit_file_command(&PathBuf::from(hazard), shell);
                assert!(
                    !command.contains("'\\''"),
                    "{shell} would get a quote splice for {hazard:?}:\n  {command}"
                );
            }
        }
    }

    /// The line-number mapping is decided by the shell on `$EDITOR`'s name.
    /// Run the generated command with fake editors that record their
    /// arguments, in a POSIX shell directly and via the /bin/sh delegation
    /// non-POSIX shells get.
    #[test]
    fn line_numbers_reach_the_editor_in_its_own_dialect() {
        let dir = std::env::temp_dir().join("omnipty-editor-line-test");
        std::fs::create_dir_all(&dir).unwrap();
        let record = dir.join("args.txt");
        for name in ["nvim", "code", "emacs", "hx", "ed"] {
            let script = dir.join(name);
            std::fs::write(
                &script,
                format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", record.display()),
            )
            .unwrap();
            Command::new("/bin/chmod")
                .arg("+x")
                .arg(&script)
                .status()
                .unwrap();
        }
        let target = dir.join("a file.rs");
        let expected = |editor: &str, at: Option<(u32, Option<u32>)>| -> Vec<String> {
            let p = target.to_string_lossy().to_string();
            match (editor, at) {
                ("nvim", Some((42, Some(8)))) => vec!["+call cursor(42,8)".into(), p],
                ("nvim", Some((42, None))) => vec!["+42".into(), p],
                ("code", Some((42, Some(8)))) => vec!["--goto".into(), format!("{p}:42:8")],
                ("emacs", Some((42, Some(8)))) => vec!["+42:8".into(), p],
                ("hx", Some((42, None))) => vec![format!("{p}:42")],
                ("ed", Some((42, Some(8)))) => vec![p],
                (_, None) => vec![p],
                _ => unreachable!(),
            }
        };
        type At = Option<(u32, Option<u32>)>;
        let cases: Vec<(&str, At)> = vec![
            ("nvim", Some((42, Some(8)))),
            ("nvim", Some((42, None))),
            ("code", Some((42, Some(8)))),
            ("emacs", Some((42, Some(8)))),
            ("hx", Some((42, None))),
            ("ed", Some((42, Some(8)))),
            ("nvim", None),
        ];
        for shell in [
            "/bin/sh",
            "/bin/zsh",
            "/bin/bash",
            "/opt/homebrew/bin/fish",
            "/bin/tcsh",
        ] {
            if !Path::new(shell).exists() {
                continue;
            }
            for (editor, at) in &cases {
                let _ = std::fs::remove_file(&record);
                let command = editor_command(&target, *at, shell, None);
                let out = Command::new(shell)
                    .arg("-c")
                    .arg(&command)
                    .env("EDITOR", dir.join(editor))
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{shell} {editor} {at:?}: {command}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let got: Vec<String> = std::fs::read_to_string(&record)
                    .unwrap_or_else(|e| {
                        panic!("{shell} {editor} {at:?}: editor never ran ({e}): {command}")
                    })
                    .lines()
                    .map(str::to_string)
                    .collect();
                assert_eq!(
                    got,
                    expected(editor, *at),
                    "{shell} {editor} {at:?}: {command}"
                );
            }
        }
    }

    #[test]
    fn open_at_line_override_substitutes_placeholders() {
        let snippet = editor_snippet(
            "'p q'",
            Some((3, None)),
            Some("my --line {line} --col {col} {path}"),
        );
        assert_eq!(snippet, "my --line 3 --col 1 'p q'");
        let snippet = editor_snippet("'p'", Some((3, Some(7))), Some("my {path}:{line}:{col}"));
        assert_eq!(snippet, "my 'p':3:7");
        // No line: the override doesn't apply, the plain open does.
        assert!(editor_snippet("'p'", None, Some("my {path}")).contains("$EDITOR 'p'"));
    }

    /// The routing decision itself, independent of what's installed.
    #[test]
    fn only_non_posix_shells_are_delegated_to_sh() {
        let path = PathBuf::from("/tmp/config.toml");
        for direct in [
            "/bin/sh",
            "/bin/bash",
            "/bin/zsh",
            "/bin/dash",
            "/bin/ksh",
            "/opt/homebrew/bin/bash-5.2",
        ] {
            assert!(
                !edit_file_command(&path, direct).starts_with("/bin/sh -c"),
                "{direct} understands the snippet directly and should not pay for a subshell"
            );
        }
        for delegated in [
            "/opt/homebrew/bin/fish",
            "/bin/tcsh",
            "/bin/csh",
            "/opt/homebrew/bin/nu",
            "/usr/bin/elvish",
        ] {
            assert!(
                edit_file_command(&path, delegated).starts_with("/bin/sh -c"),
                "{delegated} cannot parse the snippet and must go through /bin/sh"
            );
        }
    }
}
