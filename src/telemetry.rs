//! Crash reports, plus a handful of handled-error reports, sent to Sentry.
//!
//! Every Sentry call in OmniPTY goes through this module, so grepping for
//! `telemetry::` shows everything that can leave the machine.
//!
//! Only builds made by the release scripts carry a DSN. Forks, CI and
//! `cargo run` compile without one, and every function here is then a
//! cheap no-op. Never pass terminal content to anything in this module:
//! no PTY output, command lines, working directories, environment, or
//! workspace and tab names.

use std::borrow::Cow;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sentry::Hub;
use sentry::protocol::{Breadcrumb, Event, Level};

use crate::config::Config;

/// Set by scripts/dmg.sh and scripts/release-linux.sh. Never committed.
const DSN: Option<&str> = option_env!("OMNIPTY_SENTRY_DSN");

static HOME: OnceLock<String> = OnceLock::new();
/// The report ID of a crash in the previous run, if there was one.
static PREVIOUS_CRASH: OnceLock<Option<String>> = OnceLock::new();
static CRASH_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Start Sentry. Hold the guard for the whole of `main`.
pub fn init(config: &Config) -> Option<sentry::ClientInitGuard> {
    let crash_marker = crate::paths::cache_dir().join("last_crash.txt");
    // Read (and clear) even when reports are off now, so a stale marker
    // never outlives the run after it.
    let previous = std::fs::read_to_string(&crash_marker)
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty());
    let _ = std::fs::remove_file(&crash_marker);
    let _ = PREVIOUS_CRASH.set(previous);

    // Bail out before `sentry::init`, not by handing it no DSN: with no DSN
    // the SDK falls back to $SENTRY_DSN, and OmniPTY inherits the
    // environment of whatever launched it, maybe a shell where the user
    // exported their own project's DSN.
    if !config.crash_reports.enabled {
        return None;
    }
    // Parsed here because `sentry::init` panics on a malformed DSN, and an
    // empty one would fall through to $SENTRY_DSN as above.
    let dsn: sentry::types::Dsn = DSN?.parse().ok()?;
    if let Some(home) = std::env::var_os("HOME").filter(|h| h.len() > 1) {
        let _ = HOME.set(home.to_string_lossy().into_owned());
    }
    let guard = sentry::init((
        dsn,
        sentry::ClientOptions::new()
            .maybe_release(sentry::release_name!())
            .auto_session_tracking(true)
            // Empty rather than unset: unset, the contexts integration fills
            // in the hostname ("Alice's MacBook Pro") on every event and log.
            .server_name("")
            // Our frames, not gpui's or alacritty's, title and group issues.
            .in_app_include(vec!["omnipty"])
            .before_send(move |mut event| {
                event.server_name = None;
                scrub_event(&mut event);
                // Only a panic is Fatal. Leave its ID where the next launch
                // can find it; this runs before the panic hook flushes.
                if event.level == Level::Fatal {
                    let _ = std::fs::write(&crash_marker, event.event_id.simple().to_string());
                }
                Some(event)
            })
            .before_send_log(|mut log| {
                log.body = scrub(&log.body);
                Some(log)
            }),
    ));

    // Settings and platform details that often explain a crash. Nothing
    // here identifies the user.
    tag("images", config.images.enabled);
    tag("prompt", config.prompt.enabled);
    tag("shell_integration", config.shell.integration);
    #[cfg(target_os = "linux")]
    for (key, var) in [
        ("session_type", "XDG_SESSION_TYPE"),
        ("desktop", "XDG_CURRENT_DESKTOP"),
    ] {
        if let Ok(value) = std::env::var(var) {
            tag(key, value);
        }
    }
    Some(guard)
}

/// End the session and send whatever is queued. Call from `on_app_quit`:
/// on macOS GPUI quits through `[NSApp terminate:]`, which exits without
/// returning from `main`, so the guard's Drop never runs there.
pub fn shutdown() {
    let hub = Hub::main();
    hub.end_session();
    if let Some(client) = hub.client() {
        client.close(Some(Duration::from_secs(2)));
    }
}

/// Crash reports were switched off in a live config reload: stop now.
/// Switching them back on takes effect at the next launch.
pub fn disable() {
    if let Some(client) = Hub::main().client() {
        client.close(Some(Duration::ZERO));
    }
}

/// One step in the trail a crash report carries. Recorded on the main hub
/// so a crash on any thread sees it: another thread's hub is a snapshot of
/// the main one, taken the first time that thread touches Sentry.
pub fn breadcrumb(category: &'static str, message: impl Into<String>) {
    Hub::main().add_breadcrumb(Breadcrumb {
        category: Some(category.into()),
        message: Some(message.into()),
        ..Default::default()
    });
}

/// A handled failure worth counting. `what` is a fixed label and reports
/// group by it; `detail` is the error text (before_send scrubs paths).
pub fn report(what: &'static str, detail: impl Display) {
    // Built by hand rather than with `sentry::with_scope`: that pushes onto
    // the hub's scope stack, which isn't safe to share between threads, and
    // these come from the PTY and background threads too.
    Hub::main().capture_event(Event {
        message: Some(format!("{what}: {detail}")),
        level: Level::Error,
        fingerprint: Cow::Owned(vec![Cow::Borrowed(what)]),
        ..Default::default()
    });
}

/// `report`, at most once per run for each `what`. For failures that
/// repeat (every directory the file tree tries to watch, say).
pub fn report_once(what: &'static str, detail: impl Display) {
    static SENT: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    {
        let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
        if sent.contains(&what) {
            return;
        }
        sent.push(what);
    }
    report(what, detail);
}

/// A tag on every report from now on.
pub fn tag(key: &str, value: impl ToString) {
    Hub::main().configure_scope(|scope| scope.set_tag(key, value));
}

/// The first launch after an update, for counting how updates land.
pub fn updated(from: &str, to: &str) {
    sentry::logger_info!(from = from, to = to, "updated");
}

/// The previous run's crash report ID, the first time it's asked for: one
/// toast, not one per window.
pub fn crash_notice() -> Option<&'static str> {
    if CRASH_NOTICE_SHOWN.swap(true, Ordering::Relaxed) {
        return None;
    }
    PREVIOUS_CRASH.get()?.as_deref()
}

/// Help → Report an Issue. After a crash, the new issue's body names the
/// report, so the GitHub issue and the Sentry event find each other.
pub fn issue_url() -> String {
    let base = format!("{}/issues/new", crate::WEBSITE_URL);
    match PREVIOUS_CRASH.get().and_then(|id| id.as_deref()) {
        Some(id) => format!("{base}?body=%0A%0A---%0ACrash%20report%3A%20{id}"),
        None => base,
    }
}

/// `/Users/alice/x` → `~/x`. Usernames are the personal detail most likely
/// to slip into a panic message or an io error.
fn scrub(text: &str) -> String {
    match HOME.get() {
        Some(home) => text.replace(home.as_str(), "~"),
        None => text.to_owned(),
    }
}

fn scrub_event(event: &mut Event<'static>) {
    if let Some(message) = &mut event.message {
        *message = scrub(message);
    }
    if let Some(entry) = &mut event.logentry {
        entry.message = scrub(&entry.message);
    }
    for exception in &mut event.exception.values {
        if let Some(value) = &mut exception.value {
            *value = scrub(value);
        }
    }
    for crumb in &mut event.breadcrumbs.values {
        if let Some(message) = &mut crumb.message {
            *message = scrub(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `report_once` sends a label once per run and leaves others alone;
    /// without Sentry running the send itself is a no-op, so this checks
    /// the gate, not the wire.
    #[test]
    fn report_once_gates_per_label() {
        static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
        fn once(what: &'static str) -> bool {
            let mut seen = SEEN.lock().unwrap();
            if seen.contains(&what) {
                return false;
            }
            seen.push(what);
            true
        }
        assert!(once("a"));
        assert!(!once("a"));
        assert!(once("b"));
        // The real thing: no panic, no client, twice in a row.
        report_once("test label", "detail");
        report_once("test label", "detail");
    }

    #[test]
    fn issue_url_without_a_crash_is_the_plain_page() {
        // PREVIOUS_CRASH is unset in tests: `init` never ran.
        assert_eq!(issue_url(), format!("{}/issues/new", crate::WEBSITE_URL));
        assert_eq!(crash_notice(), None);
    }
}
