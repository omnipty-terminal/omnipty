mod app;
mod changelog;
mod cli;
mod config;
mod git;
mod keymap;
mod line_edit;
mod markdown;
mod notifications;
mod palette;
mod panes;
mod paths;
mod prompt;
mod startup;
mod telemetry;
mod terminal;
mod tree;
mod update;
mod workspaces;

use gpui::{App, Application, Menu, MenuItem, SystemMenuType};

use crate::keymap::actions::*;

pub(crate) const WEBSITE_URL: &str = "https://omnipty.com";

/// The application menus. macOS installs them in the menu bar; Linux has no
/// menu bar, so the same list backs the ☰ popover in the window's top-left
/// corner, minus the entries that only AppKit can honour.
pub(crate) fn menus() -> Vec<Menu> {
    let mut omnipty = vec![
        MenuItem::action("About OmniPTY", About),
        MenuItem::action("Check for Updates…", CheckForUpdates),
        MenuItem::separator(),
        MenuItem::action("Settings…", OpenSettings),
        MenuItem::action("Select Theme…", SelectTheme),
    ];
    if cfg!(target_os = "macos") {
        // Services and app hiding are AppKit concepts with no Wayland/X11
        // equivalent; a tiling WM has nowhere to hide a window to.
        omnipty.extend([
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide OmniPTY", Hide),
            MenuItem::action("Hide Others", HideOthers),
        ]);
    }
    omnipty.extend([
        MenuItem::separator(),
        MenuItem::action("Quit OmniPTY", Quit),
    ]);
    vec![
        Menu {
            // The first menu takes the app's name in the menu bar.
            name: "OmniPTY".into(),
            items: omnipty,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Tab", NewTab),
                MenuItem::action("New Workspace", NewWorkspace),
                MenuItem::action("New Window", NewWindow),
                MenuItem::separator(),
                MenuItem::action("Rename Tab…", RenameTab),
                MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
                MenuItem::action("Set Startup Command…", SetStartupCommand),
                MenuItem::separator(),
                MenuItem::action("Split Right", SplitRight),
                MenuItem::action("Split Down", SplitDown),
                MenuItem::action("Split Left", SplitLeft),
                MenuItem::action("Split Up", SplitUp),
                MenuItem::separator(),
                MenuItem::action("Close Pane", ClosePane),
                MenuItem::action("Close Other Panes", PaneOnly),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
                MenuItem::separator(),
                MenuItem::action("Select All", SelectAll),
                MenuItem::separator(),
                MenuItem::action("Copy Last Command's Output", CopyLastOutput),
                MenuItem::action("Copy Last Command", CopyLastCommand),
                MenuItem::action("Copy Last Command and Output", CopyLastBlock),
                MenuItem::separator(),
                MenuItem::action("Find", Search),
                MenuItem::action("Copy Mode", CopyMode),
                MenuItem::action("Command History…", CommandHistory),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette…", CommandPalette),
                MenuItem::separator(),
                MenuItem::action("Toggle File Tree", ToggleDrawer),
                MenuItem::action("Focus File Tree", FocusTree),
                MenuItem::action("Toggle Status Bar", ToggleStatusBar),
                MenuItem::action("Toggle Tab Bar", ToggleTabBar),
                MenuItem::separator(),
                MenuItem::action("Increase Font Size", FontIncrease),
                MenuItem::action("Decrease Font Size", FontDecrease),
                MenuItem::action("Reset Font Size", FontReset),
                MenuItem::separator(),
                MenuItem::action("Clear Scrollback", ClearScrollback),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullscreen),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Equalize Splits", PaneEqualize),
                MenuItem::action("Zoom Pane", PaneZoom),
                MenuItem::action("Swap Pane with Neighbour", PaneSwap),
                MenuItem::action("Broadcast Input to All Panes", PaneBroadcast),
                MenuItem::separator(),
                MenuItem::action("Show Next Tab", SelectNextTab),
                MenuItem::action("Show Previous Tab", SelectPreviousTab),
                MenuItem::action("Move Tab Left", MoveTabLeft),
                MenuItem::action("Move Tab Right", MoveTabRight),
            ],
        },
        Menu {
            name: "Help".into(),
            items: vec![
                MenuItem::action("OmniPTY Help", OpenHelp),
                MenuItem::action("What's New", ShowChangelog),
                MenuItem::action("Report an Issue", ReportIssue),
            ],
        },
    ]
}

fn main() {
    // Plain CLI queries and bad arguments: answer without touching the
    // display or any state.
    let launch = match cli::parse(std::env::args().skip(1)) {
        Ok(cli::Parsed::Version) => {
            println!("omnipty {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Ok(cli::Parsed::Help) => {
            println!("{}", cli::USAGE);
            return;
        }
        Ok(cli::Parsed::Run(launch)) => launch,
        Err(message) => {
            eprintln!("omnipty: {message}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    // `-e`: the first pane runs this instead of a shell, and pinned
    // workspaces stay put — a TUI launched from the desktop shouldn't drag
    // the whole saved session up with it.
    let command = launch.command.clone();
    let restore = command.is_none();
    cli::install(launch);

    // A bundle the old updater wrote OmniPTY into under its former name
    // moves to OmniPTY.app and relaunches from there.
    #[cfg(target_os = "macos")]
    if update::relocate_renamed_bundle() {
        return;
    }

    let (config, config_error) = config::load();
    // After the config, so `[crash_reports] enabled = false` is honoured, and
    // after the early returns above, so `--version` never starts it.
    let _sentry = telemetry::init(&config);
    #[cfg(target_os = "macos")]
    update::check_install_attempt();
    // Silent-cd/run handoff files a killed shell never consumed.
    prompt::integration::clean_stale_channels();

    // Closing the last window leaves OmniPTY running, the way most macOS apps
    // behave; clicking the Dock icon brings a fresh window back. cmd-q quits.
    let app = Application::new();
    // AppKit only delivers this when the app has no open windows, so there is
    // nothing further to check — a closed handle can linger in cx.windows().
    app.on_reopen(|cx| {
        let (config, error) = config::load();
        app::open_omnipty_window(config, error, None, None, true, cx);
    });
    app.run(move |cx: &mut App| {
        // macOS quits through [NSApp terminate:], which exits without coming
        // back here, so `_sentry` is never dropped. End the session and flush
        // on quit instead, in the closure body rather than the future: GPUI
        // waits only 100 ms on quit futures.
        cx.on_app_quit(|_| {
            telemetry::shutdown();
            async {}
        })
        .detach();

        // Must happen before launch completes, or banners never show while
        // OmniPTY is frontmost.
        notifications::init();

        // Linux has no Dock to reopen from, and a windowless process is
        // just a stray one under a tiling WM: closing the last window quits.
        if cfg!(target_os = "linux") {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        }

        // Bundle the default font so a machine with no Nerd Font installed
        // still gets crisp monospace and every powerline/tree glyph. GPUI
        // consults in-memory fonts before system ones, so a user-installed
        // copy of the same family behaves identically.
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")
                        .as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Italic.ttf")
                        .as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-BoldItalic.ttf")
                        .as_slice(),
                ),
            ])
            .ok();

        // Defaults merged with the user's [keymap]; bad entries are skipped
        // here and reported in the window banner by `OmniPTY::new`.
        cx.bind_keys(keymap::resolve(&config.keymap).bindings());

        // App-level actions (no window required).
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        // Windows show the About panel; this is the no-window fallback,
        // reachable from the macOS menu bar.
        cx.on_action(|_: &About, cx| cx.open_url(WEBSITE_URL));
        // App-level fallback: with no windows open there is no element tree to
        // dispatch to, so the window-scoped handler cannot run. Without this,
        // closing the last window strands the app with a dead File menu.
        cx.on_action(|_: &NewWindow, cx| {
            if cx.windows().is_empty() {
                let (config, error) = config::load();
                app::open_omnipty_window(config, error, None, None, false, cx);
            }
        });
        cx.on_action(|_: &OpenHelp, cx| cx.open_url(&format!("{WEBSITE_URL}/docs/")));
        cx.on_action(|_: &ReportIssue, cx| cx.open_url(&telemetry::issue_url()));

        // The menu bar is a macOS thing; on Linux the window draws its own
        // ☰ menu from the same list (see `OmniPTY::render_app_menu`).
        if cfg!(target_os = "macos") {
            cx.set_menus(menus());
        }

        // Notification clicks arrive on the AppKit main thread through a
        // channel; whichever window owns the routed pane brings it forward.
        let mut clicks = notifications::install_click_channel();
        cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while let Some(key) = clicks.next().await {
                cx.update(|cx| {
                    for handle in cx.windows() {
                        let handled = handle
                            .update(cx, |root, window, cx| {
                                root.downcast::<app::OmniPTY>()
                                    .map(|omnipty| {
                                        omnipty.update(cx, |o, cx| {
                                            o.on_notification_click(key, window, cx)
                                        })
                                    })
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if handled {
                            break;
                        }
                    }
                })
                .ok();
            }
        })
        .detach();

        app::open_omnipty_window(config, config_error, None, command, restore, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every menu entry must resolve to a registry row: the Linux ☰ menu
    /// looks bindings up that way, and the palette lists the same actions.
    #[test]
    fn menu_actions_have_registry_rows() {
        for menu in menus() {
            for item in &menu.items {
                if let MenuItem::Action { name, action, .. } = item {
                    assert!(
                        crate::keymap::registry::all()
                            .iter()
                            .any(|m| (m.build)().partial_eq(action.as_ref())),
                        "{name} in the {} menu has no registry row",
                        menu.name
                    );
                }
            }
        }
    }

    /// The ☰ popover can't show OS-managed submenus or hide the app, so
    /// those entries are macOS-only.
    #[test]
    fn linux_menus_skip_appkit_only_entries() {
        let appkit_only = menus()
            .into_iter()
            .flat_map(|m| m.items)
            .any(|item| match item {
                MenuItem::SystemMenu(_) => true,
                MenuItem::Action { action, .. } => {
                    action.partial_eq(&Hide) || action.partial_eq(&HideOthers)
                }
                _ => false,
            });
        assert_eq!(appkit_only, cfg!(target_os = "macos"));
    }
}
