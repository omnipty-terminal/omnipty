//! The built-in keybinding table, as data: keystrokes, action id, context.
//!
//! Every action id here must exist in the registry (a test checks). The
//! `Terminal` context deliberately has no bare-key bindings: every key the
//! shell could want must fall through to the raw key handler. Keys we steal
//! from the terminal, exhaustively: none — everything terminal-scoped is
//! modifier-prefixed (`cmd-*` / `ctrl-shift-*`) or sequence-led (`ctrl-w …`)
//! and bound at Root.
//!
//! Three tables: `SHARED` (no platform modifier — `ctrl-w` chords and the
//! bare keys of the modeless panels), `MACOS` (`cmd-*`) and `LINUX`
//! (`ctrl-shift-*`, the Linux terminal convention, because Super belongs to
//! the compositor on Hyprland/GNOME/KDE and `cmd-*` would never arrive).
//! `defaults()` yields the shared table plus the platform's; the tests run
//! over all three so a bad entry fails on either platform.

use super::resolve::KeyCtx::{self, *};

pub struct DefaultBinding {
    pub keys: &'static str,
    pub action: &'static str,
    pub ctx: KeyCtx,
}

const fn b(keys: &'static str, action: &'static str, ctx: KeyCtx) -> DefaultBinding {
    DefaultBinding { keys, action, ctx }
}

/// The built-in table for this platform.
pub fn defaults() -> impl Iterator<Item = &'static DefaultBinding> {
    let platform: &[DefaultBinding] = if cfg!(target_os = "macos") {
        MACOS
    } else {
        LINUX
    };
    SHARED.iter().chain(platform.iter())
}

/// Every table, for the tests.
#[cfg(test)]
pub fn all_tables() -> [(&'static str, &'static [DefaultBinding]); 3] {
    [("shared", SHARED), ("macos", MACOS), ("linux", LINUX)]
}

pub static SHARED: &[DefaultBinding] = &[
    // Root — reachable from both panes. Splits, vim-style: ctrl-w h/j/k/l
    // walks panes geometrically; going left from the leftmost pane lands on
    // the file tree, so the old "ctrl-w h focuses the drawer" reflex still
    // works.
    b("ctrl-w h", "pane::focus_left", Root),
    b("ctrl-w j", "pane::focus_down", Root),
    b("ctrl-w k", "pane::focus_up", Root),
    b("ctrl-w l", "pane::focus_right", Root),
    b("ctrl-w v", "pane::split_right", Root),
    b("ctrl-w s", "pane::split_down", Root),
    b("ctrl-w shift-v", "pane::split_left", Root),
    b("ctrl-w shift-s", "pane::split_up", Root),
    b("ctrl-w q", "pane::close", Root),
    b("ctrl-w w", "drawer::focus_toggle", Root),
    // tmux muscle memory: zoom, broadcast, only, exchange, copy mode.
    b("ctrl-w z", "pane::zoom", Root),
    b("ctrl-w b", "pane::broadcast", Root),
    b("ctrl-w o", "pane::only", Root),
    b("ctrl-w x", "pane::swap", Root),
    b("ctrl-w [", "terminal::copy_mode", Root),
    b("ctrl-w ,", "tab::rename", Root),
    // "run": what this pane runs when its workspace is restored.
    b("ctrl-w r", "pane::set_startup_command", Root),
    // Resizing, vim/tmux-style. Shifted punctuation arrives as the shifted
    // character (macOS and xkb alike), so `<` is bound as itself rather
    // than `shift-,`.
    b("ctrl-w <", "pane::narrower", Root),
    b("ctrl-w >", "pane::wider", Root),
    b("ctrl-w -", "pane::shorter", Root),
    b("ctrl-w +", "pane::taller", Root),
    b("ctrl-w =", "pane::equalize", Root),
    // Jump straight to the drawer from any pane, without walking there.
    b("ctrl-w t", "drawer::focus_tree", Root),
    b("ctrl-tab", "tab::next", Root),
    b("ctrl-shift-tab", "tab::previous", Root),
    // Workspaces panel (drawer, below the file tree).
    b("ctrl-w p", "drawer::focus_workspaces", Root),
    // Overlay — any modal list. Arrows and emacs-style ctrl-n/p move; the
    // palette has a text input, so bare letters must stay free for typing.
    b("down", "overlay::next", Overlay),
    b("ctrl-n", "overlay::next", Overlay),
    b("up", "overlay::prev", Overlay),
    b("ctrl-p", "overlay::prev", Overlay),
    b("enter", "overlay::confirm", Overlay),
    b("alt-enter", "overlay::confirm_reveal", Overlay),
    b("escape", "overlay::cancel", Overlay),
    // OverlayList — modal lists with no text input (the theme picker), where
    // vim keys are free.
    b("j", "overlay::next", OverlayList),
    b("k", "overlay::prev", OverlayList),
    // FileTree — modeless "always normal mode"; bare letters are free.
    b("j", "tree::down", FileTree),
    b("down", "tree::down", FileTree),
    b("k", "tree::up", FileTree),
    b("up", "tree::up", FileTree),
    b("g g", "tree::top", FileTree),
    b("shift-g", "tree::bottom", FileTree),
    b("ctrl-d", "tree::half_page_down", FileTree),
    b("ctrl-u", "tree::half_page_up", FileTree),
    b("l", "tree::expand", FileTree),
    b("right", "tree::expand", FileTree),
    b("h", "tree::collapse", FileTree),
    b("left", "tree::collapse", FileTree),
    b("enter", "tree::open", FileTree),
    b("o", "tree::open", FileTree),
    b("p", "tree::parent", FileTree),
    b("c", "tree::set_root", FileTree),
    b("-", "tree::root_up", FileTree),
    b("shift-i", "tree::toggle_hidden", FileTree),
    b("shift-r", "tree::refresh", FileTree),
    b("/", "tree::filter", FileTree),
    b("a", "tree::add", FileTree),
    b("r", "tree::rename", FileTree),
    b("m", "tree::move", FileTree),
    b("d", "tree::delete", FileTree),
    b("shift-p", "tree::preview", FileTree),
    // The tree as an input device: hand the selection to the shell.
    b("y", "tree::yank_path", FileTree),
    b("shift-y", "tree::yank_path_absolute", FileTree),
    // escape is a dismiss chain: clear filter/input first, else focus terminal.
    b("escape", "tree::escape", FileTree),
    // tab hops between the drawer's two panels.
    b("tab", "drawer::focus_workspaces", FileTree),
    // Workspaces panel — modeless like the tree; bare letters are free.
    b("j", "workspace::down", Workspaces),
    b("down", "workspace::down", Workspaces),
    b("k", "workspace::up", Workspaces),
    b("up", "workspace::up", Workspaces),
    b("enter", "workspace::open", Workspaces),
    b("o", "workspace::open", Workspaces),
    b("a", "workspace::add", Workspaces),
    b("d", "workspace::delete", Workspaces),
    b("r", "workspace::rename", Workspaces),
    b("p", "workspace::toggle_persist", Workspaces),
    b("e", "workspace::edit_startup_commands", Workspaces),
    b("escape", "workspace::escape", Workspaces),
    b("tab", "drawer::focus_tree", Workspaces),
];

/// macOS: `cmd-*`, the way every Mac terminal does it.
pub static MACOS: &[DefaultBinding] = &[
    b("cmd-q", "app::quit", Root),
    b("cmd-shift-v", "terminal::copy_mode", Root),
    b("cmd-shift-t", "tab::reopen", Root),
    b("cmd-shift-e", "drawer::focus_tree", Root),
    b("cmd-d", "pane::split_right", Root),
    b("cmd-shift-d", "pane::split_down", Root),
    b("cmd-alt-left", "pane::focus_left", Root),
    b("cmd-alt-right", "pane::focus_right", Root),
    b("cmd-alt-up", "pane::focus_up", Root),
    b("cmd-alt-down", "pane::focus_down", Root),
    b("cmd-b", "drawer::toggle", Root),
    b("cmd-v", "terminal::paste", Root),
    b("cmd-c", "terminal::copy", Root),
    b("cmd-=", "terminal::font_increase", Root),
    b("cmd-+", "terminal::font_increase", Root),
    b("cmd--", "terminal::font_decrease", Root),
    b("cmd-0", "terminal::font_reset", Root),
    b("cmd-f", "terminal::search", Root),
    b("cmd-alt-r", "terminal::search_regex", Root),
    b("cmd-alt-c", "terminal::search_case", Root),
    b("cmd-alt-w", "terminal::search_word", Root),
    // cmd-k clears like Terminal.app. It can't also start a chord: GPUI
    // holds a prefix key for a second before dispatching it alone.
    b("cmd-k", "terminal::clear_scrollback", Root),
    b("cmd-up", "terminal::prompt_up", Root),
    b("cmd-down", "terminal::prompt_down", Root),
    b("cmd-r", "terminal::history", Root),
    b("cmd-shift-c", "terminal::copy_last_output", Root),
    b("cmd-n", "window::new", Root),
    b("cmd-t", "tab::new", Root),
    // macOS reports shifted punctuation as the shifted character, so
    // shift-cmd-[ arrives as cmd-{ — bind both spellings.
    b("shift-cmd-]", "tab::next", Root),
    b("shift-cmd-[", "tab::previous", Root),
    b("cmd-}", "tab::next", Root),
    b("cmd-{", "tab::previous", Root),
    b("cmd-1", "tab::select_1", Root),
    b("cmd-2", "tab::select_2", Root),
    b("cmd-3", "tab::select_3", Root),
    b("cmd-4", "tab::select_4", Root),
    b("cmd-5", "tab::select_5", Root),
    b("cmd-6", "tab::select_6", Root),
    b("cmd-7", "tab::select_7", Root),
    b("cmd-8", "tab::select_8", Root),
    b("cmd-9", "tab::select_9", Root),
    // cmd-shift-<n> is out: macOS keeps cmd-shift-3/4/5 for screenshots, and
    // the rest arrive as layout-dependent punctuation (cmd-!, cmd-@, …).
    b("cmd-alt-1", "workspace::select_1", Root),
    b("cmd-alt-2", "workspace::select_2", Root),
    b("cmd-alt-3", "workspace::select_3", Root),
    b("cmd-alt-4", "workspace::select_4", Root),
    b("cmd-alt-5", "workspace::select_5", Root),
    b("cmd-alt-6", "workspace::select_6", Root),
    b("cmd-alt-7", "workspace::select_7", Root),
    b("cmd-alt-8", "workspace::select_8", Root),
    b("cmd-alt-9", "workspace::select_9", Root),
    b("cmd-w", "window::close", Root),
    b("cmd-a", "terminal::select_all", Root),
    b("cmd-m", "window::minimize", Root),
    b("ctrl-cmd-f", "window::toggle_fullscreen", Root),
    b("cmd-h", "app::hide", Root),
    b("alt-cmd-h", "app::hide_others", Root),
    b("cmd-alt-t", "app::select_theme", Root),
    b("cmd-shift-p", "app::palette", Root),
    b("cmd-p", "app::file_finder", Root),
    // macOS keeps cmd-g / cmd-shift-g for find-next / find-previous; the
    // search bar doesn't bind them and this is Root-only, so no clash. If
    // search-next ever gets a key, it's cmd-g in the search context.
    b("cmd-shift-g", "app::go_to", Root),
    b("cmd-shift-r", "drawer::reveal", Root),
    b("cmd-,", "app::settings", Root),
    b("cmd-enter", "overlay::confirm_alt", Overlay),
    b("cmd-c", "tree::copy_path", FileTree),
    b("cmd-shift-o", "tree::reveal_in_finder", FileTree),
];

/// Linux: `ctrl-shift-*` where a Mac uses `cmd-*` (GNOME Terminal, kitty,
/// VS Code's terminal all do this — plain ctrl-c must stay SIGINT), `alt-<n>`
/// for tabs because Super+<n> switches workspaces on every tiling WM. The
/// window-management set (hide, minimise, fullscreen, quit) is left to the
/// compositor: nothing here binds Super. Anything without a `ctrl-shift`
/// spelling of its own is reachable through a `ctrl-w` chord in `SHARED`.
pub static LINUX: &[DefaultBinding] = &[
    b("ctrl-shift-q", "app::quit", Root),
    b("ctrl-shift-c", "terminal::copy", Root),
    b("ctrl-shift-v", "terminal::paste", Root),
    // GPUI's xkb path reports a shifted symbol as the symbol itself with
    // shift dropped (`ctrl-shift-=` arrives as `ctrl-+`), so both spellings
    // are bound and neither needs shift. ctrl-_ is left alone: readline's
    // undo.
    b("ctrl-+", "terminal::font_increase", Root),
    b("ctrl-=", "terminal::font_increase", Root),
    b("ctrl--", "terminal::font_decrease", Root),
    b("ctrl-0", "terminal::font_reset", Root),
    b("ctrl-shift-f", "terminal::search", Root),
    b("ctrl-alt-r", "terminal::search_regex", Root),
    b("ctrl-alt-c", "terminal::search_case", Root),
    b("ctrl-alt-w", "terminal::search_word", Root),
    // ctrl-shift-k clears like GNOME Terminal's ctrl-shift-l would; it can't
    // start a chord (GPUI holds a prefix key before dispatching it alone).
    b("ctrl-shift-k", "terminal::clear_scrollback", Root),
    b("ctrl-shift-up", "terminal::prompt_up", Root),
    b("ctrl-shift-down", "terminal::prompt_down", Root),
    // Shell ctrl-r stays reverse-search.
    b("ctrl-shift-r", "terminal::history", Root),
    b("ctrl-shift-alt-c", "terminal::copy_last_output", Root),
    b("ctrl-shift-a", "terminal::select_all", Root),
    b("ctrl-shift-n", "window::new", Root),
    b("ctrl-shift-w", "window::close", Root),
    b("ctrl-shift-t", "tab::new", Root),
    b("ctrl-shift-alt-t", "tab::reopen", Root),
    b("ctrl-pageup", "tab::previous", Root),
    b("ctrl-pagedown", "tab::next", Root),
    b("alt-1", "tab::select_1", Root),
    b("alt-2", "tab::select_2", Root),
    b("alt-3", "tab::select_3", Root),
    b("alt-4", "tab::select_4", Root),
    b("alt-5", "tab::select_5", Root),
    b("alt-6", "tab::select_6", Root),
    b("alt-7", "tab::select_7", Root),
    b("alt-8", "tab::select_8", Root),
    b("alt-9", "tab::select_9", Root),
    b("ctrl-alt-1", "workspace::select_1", Root),
    b("ctrl-alt-2", "workspace::select_2", Root),
    b("ctrl-alt-3", "workspace::select_3", Root),
    b("ctrl-alt-4", "workspace::select_4", Root),
    b("ctrl-alt-5", "workspace::select_5", Root),
    b("ctrl-alt-6", "workspace::select_6", Root),
    b("ctrl-alt-7", "workspace::select_7", Root),
    b("ctrl-alt-8", "workspace::select_8", Root),
    b("ctrl-alt-9", "workspace::select_9", Root),
    b("ctrl-shift-b", "drawer::toggle", Root),
    b("ctrl-shift-e", "drawer::focus_tree", Root),
    b("ctrl-shift-alt-r", "drawer::reveal", Root),
    b("ctrl-shift-p", "app::palette", Root),
    b("ctrl-shift-o", "app::file_finder", Root),
    b("ctrl-shift-g", "app::go_to", Root),
    b("ctrl-w shift-t", "app::select_theme", Root),
    b("ctrl-,", "app::settings", Root),
    b("ctrl-enter", "overlay::confirm_alt", Overlay),
    b("ctrl-shift-c", "tree::copy_path", FileTree),
    b("ctrl-shift-o", "tree::reveal_in_finder", FileTree),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::registry;

    fn every_binding() -> impl Iterator<Item = (&'static str, &'static DefaultBinding)> {
        all_tables()
            .into_iter()
            .flat_map(|(name, table)| table.iter().map(move |b| (name, b)))
    }

    #[test]
    fn every_default_names_a_registered_action() {
        for (table, d) in every_binding() {
            assert!(
                registry::by_id(d.action).is_some(),
                "{table} binding {:?} -> {:?} names an action that isn't in the registry",
                d.keys,
                d.action
            );
        }
    }

    #[test]
    fn every_default_keystroke_parses() {
        for (table, d) in every_binding() {
            for token in d.keys.split_whitespace() {
                assert!(
                    gpui::Keystroke::parse(token).is_ok(),
                    "{table} binding {:?} does not parse",
                    d.keys
                );
            }
        }
    }

    #[test]
    fn terminal_and_root_never_take_bare_keys() {
        for (table, d) in every_binding().filter(|(_, d)| matches!(d.ctx, Root | Terminal)) {
            let first = d.keys.split_whitespace().next().unwrap();
            let ks = gpui::Keystroke::parse(first).unwrap();
            let m = ks.modifiers;
            assert!(
                m.control || m.alt || m.platform || m.function,
                "{table} binding {:?} would steal a bare key from the shell",
                d.keys
            );
        }
    }

    /// The platform tables are alternatives: what one binds with `cmd`, the
    /// other must reach some other way. Guards against adding an action to
    /// the Mac table and forgetting Linux.
    #[test]
    fn platform_tables_cover_the_same_actions() {
        use std::collections::BTreeSet;
        let actions = |t: &[DefaultBinding]| t.iter().map(|b| b.action).collect::<BTreeSet<_>>();
        let shared = actions(SHARED);
        let mac: BTreeSet<_> = actions(MACOS).union(&shared).copied().collect();
        let linux: BTreeSet<_> = actions(LINUX).union(&shared).copied().collect();
        // Window management the compositor owns on Linux.
        let wm_only = BTreeSet::from([
            "app::hide",
            "app::hide_others",
            "window::minimize",
            "window::toggle_fullscreen",
        ]);
        let missing: Vec<_> = mac
            .difference(&linux)
            .filter(|a| !wm_only.contains(*a))
            .collect();
        assert!(
            missing.is_empty(),
            "bound on macOS but not Linux: {missing:?}"
        );
        let extra: Vec<_> = linux.difference(&mac).collect();
        assert!(extra.is_empty(), "bound on Linux but not macOS: {extra:?}");
    }

    /// The shared table must work under any compositor: nothing in it may
    /// use the platform (Super/cmd) modifier.
    #[test]
    fn shared_table_never_uses_the_platform_modifier() {
        for d in SHARED {
            for token in d.keys.split_whitespace() {
                let ks = gpui::Keystroke::parse(token).unwrap();
                assert!(
                    !ks.modifiers.platform,
                    "{:?} belongs in a platform table",
                    d.keys
                );
            }
        }
        for d in LINUX {
            for token in d.keys.split_whitespace() {
                let ks = gpui::Keystroke::parse(token).unwrap();
                assert!(
                    !ks.modifiers.platform,
                    "{:?}: Super is the compositor's",
                    d.keys
                );
            }
        }
    }

    /// No two bindings in one platform's table disagree about a keystroke
    /// in the same context.
    #[test]
    fn no_conflicting_keys_within_a_platform() {
        use std::collections::HashMap;
        for (name, table) in [("macos", MACOS), ("linux", LINUX)] {
            let mut seen: HashMap<(String, KeyCtx), &str> = HashMap::new();
            for d in SHARED.iter().chain(table.iter()) {
                let (keys, _) = crate::keymap::resolve::normalise_keys(d.keys).unwrap();
                if let Some(prev) = seen.insert((keys.clone(), d.ctx), d.action)
                    && prev != d.action
                {
                    panic!(
                        "{name}: {keys:?} in {:?} bound to both {prev} and {}",
                        d.ctx, d.action
                    );
                }
            }
        }
    }
}
