use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

pub use super::event_loop::SessionEvent;
use super::event_loop::{EventLoop, LoopSender, Msg};
use super::graphics::GraphicsState;

/// Bridge from the PTY thread to the GPUI main thread. Invoked on the PTY
/// reader thread, possibly while it holds the term lock — it must do nothing
/// but send on the channel.
#[derive(Clone)]
pub struct EventProxy(UnboundedSender<SessionEvent>);

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        self.0.unbounded_send(SessionEvent::Term(event)).ok();
    }
}

/// Grid geometry: cell counts plus the measured cell box, in logical pixels
/// at `scale` device pixels each.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
    pub cell_width: f32,
    pub cell_height: f32,
    pub scale: f32,
}

impl TermSize {
    /// One cell in device pixels: what programs are told, so they send
    /// images at the display's real resolution, and what image placement
    /// divides by.
    pub fn cell_pixels(&self) -> (f32, f32) {
        (self.cell_width * self.scale, self.cell_height * self.scale)
    }

    /// For spawning the PTY, which takes alacritty's whole-pixel cell.
    pub fn window_size(&self) -> WindowSize {
        let (cell_width, cell_height) = self.cell_pixels();
        WindowSize {
            num_lines: self.screen_lines as u16,
            num_cols: self.columns as u16,
            cell_width: cell_width.round() as u16,
            cell_height: cell_height.round() as u16,
        }
    }

    /// What `TIOCGWINSZ` reports, pixel fields from the unrounded cell.
    pub fn winsize(&self) -> libc::winsize {
        let (cell_width, cell_height) = self.cell_pixels();
        libc::winsize {
            ws_row: self.screen_lines as u16,
            ws_col: self.columns as u16,
            ws_xpixel: (self.columns as f32 * cell_width).round() as u16,
            ws_ypixel: (self.screen_lines as f32 * cell_height).round() as u16,
        }
    }
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

pub struct SessionOptions {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub scrollback: usize,
    pub env: HashMap<String, String>,
    /// `images.enabled`: draw the pictures programs send.
    pub images: bool,
}

/// Resolve the shell: config, then $SHELL, then /bin/zsh.
pub fn resolve_shell(configured: Option<&str>) -> String {
    configured
        .map(str::to_string)
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/zsh".to_string())
}

/// The program name of a shell path, for family checks. A version suffix is
/// kept (`bash-5.2` stays whole) so callers match on a prefix.
pub fn shell_name(program: &str) -> &str {
    std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
}

/// Whether this shell understands Bourne syntax, so OmniPTY can hand it a `[ -n
/// … ]` snippet directly. fish, the csh family, nushell, xonsh and friends do
/// not, and need such a snippet delegated to `/bin/sh`.
pub fn is_posix_shell(program: &str) -> bool {
    let name = shell_name(program);
    [
        "sh", "bash", "zsh", "dash", "ksh", "mksh", "pdksh", "ash", "yash",
    ]
    .iter()
    .any(|family| name == *family || name.starts_with(&format!("{family}-")))
}

pub struct TerminalSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    sender: LoopSender,
    master_fd: RawFd,
    child_pid: i32,
    join: Option<JoinHandle<()>>,
    /// Unique per spawn; exported to the shell as `OMNIPTY_SESSION` so the
    /// silent-cd/run handlers read their own channel file.
    session_id: String,
}

/// A session id nothing else in this process (or a previous one) will
/// reuse: pid plus a counter. Only ever used as a file name, so it is kept
/// to characters no shell quotes.
fn next_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

impl TerminalSession {
    pub fn spawn(
        options: SessionOptions,
        size: TermSize,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        let (tx, rx) = unbounded();
        let proxy = EventProxy(tx);

        tty::setup_env();
        let session_id = next_session_id();
        let mut env = options.env;
        env.insert("TERM".into(), "xterm-256color".into());
        env.insert("COLORTERM".into(), "truecolor".into());
        env.insert("OMNIPTY_VERSION".into(), env!("CARGO_PKG_VERSION").into());
        // The pre-rename names, kept through one minor so dotfiles that read
        // $OXIDE_SESSION keep working. Remove with 0.10.
        env.insert("OXIDE_VERSION".into(), env!("CARGO_PKG_VERSION").into());
        // Always ours: launched from another terminal, the inherited value
        // would have programs speak that terminal's dialect at OmniPTY.
        env.insert("TERM_PROGRAM".into(), "OmniPTY".into());
        env.insert(
            "TERM_PROGRAM_VERSION".into(),
            env!("CARGO_PKG_VERSION").into(),
        );
        env.insert("OMNIPTY_SESSION".into(), session_id.clone());
        env.insert("OXIDE_SESSION".into(), session_id.clone());
        // GUI-launched apps get no locale; a C-locale shell breaks multibyte
        // input and prompt glyphs. Mirror Terminal.app: set one if absent.
        if std::env::var("LANG").is_err() && !env.contains_key("LANG") {
            env.insert("LANG".into(), "en_US.UTF-8".into());
        }

        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(options.program, options.args)),
            working_directory: options.working_directory,
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&pty_options, size.window_size(), 0)?;
        let master_fd = pty.file().as_raw_fd();
        let child_pid = pty.child().id() as i32;

        let term_config = TermConfig {
            scrolling_history: options.scrollback,
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(term_config, &size, proxy.clone())));

        let event_tx = proxy.0.clone();
        let graphics = GraphicsState::new(options.images, size.cell_pixels());
        let event_loop = EventLoop::new(Arc::clone(&term), proxy, pty, graphics, move |event| {
            event_tx.unbounded_send(event).ok();
        })?;
        let sender = event_loop.sender();
        let join = event_loop.spawn();

        Ok((
            Self {
                term,
                sender,
                master_fd,
                child_pid,
                join: Some(join),
                session_id,
            },
            rx,
        ))
    }

    /// The value of `OMNIPTY_SESSION` in this shell's environment.
    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn write_input(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.sender.write(bytes);
    }

    pub fn resize(&self, size: TermSize) {
        self.term.lock().resize(size);
        let _ = self.sender.send(Msg::Resize(size));
    }

    /// `images.enabled` changed in the config.
    pub fn set_images_enabled(&self, enabled: bool) {
        let _ = self.sender.send(Msg::ImagesEnabled(enabled));
    }

    /// These images were evicted; the PTY thread stops vouching for them.
    pub fn forget_images(&self, ids: Vec<u32>) {
        let _ = self.sender.send(Msg::ForgetImages(ids));
    }

    /// The cwd of the foreground process group on the PTY, via tcgetpgrp +
    /// proc_pidinfo (macOS) or procfs (Linux). Works with no shell
    /// cooperation at all.
    #[cfg(target_os = "linux")]
    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        let pgrp = unsafe { libc::tcgetpgrp(self.master_fd) };
        if pgrp <= 0 {
            return None;
        }
        // Unreadable (another user's process, or one that already exited)
        // maps to None, the same contract as the macOS lookup.
        std::fs::read_link(format!("/proc/{pgrp}/cwd")).ok()
    }

    /// The cwd of the foreground process group on the PTY, via tcgetpgrp +
    /// proc_pidinfo. Works with no shell cooperation at all.
    #[cfg(target_os = "macos")]
    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        unsafe {
            let pgrp = libc::tcgetpgrp(self.master_fd);
            if pgrp <= 0 {
                return None;
            }
            let mut info: libc::proc_vnodepathinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            let written = libc::proc_pidinfo(
                pgrp,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            );
            if written <= 0 {
                return None;
            }
            let path = std::ffi::CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast());
            let path = path.to_str().ok()?;
            if path.is_empty() {
                None
            } else {
                Some(PathBuf::from(path))
            }
        }
    }
}

impl TerminalSession {
    /// The foreground process on this PTY, and its ssh host when it's ssh.
    pub fn foreground_process(&self) -> Option<super::process::ForegroundProcess> {
        super::process::foreground(self.master_fd)
    }

    /// Swap the terminal's options (cursor style, scrollback) in place, for
    /// a config reload. The PTY is untouched.
    pub fn set_term_options(&self, config: TermConfig) {
        self.term.lock().set_options(config);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        // Teardown must never block the main thread: alacritty's Pty::drop
        // calls wait() on the child, and an interactive shell that ignores
        // SIGHUP would hang quit indefinitely (this presented as "the app
        // won't close"). Signal the shell, then join + drop the event loop
        // (and with it the Pty) on a detached thread, escalating to SIGKILL
        // after a grace period.
        let _ = self.sender.send(Msg::Shutdown);
        let child_pid = self.child_pid;
        unsafe {
            libc::kill(child_pid, libc::SIGHUP);
        }
        if let Some(join) = self.join.take() {
            std::thread::spawn(move || {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    unsafe {
                        libc::kill(child_pid, libc::SIGKILL);
                    }
                });
                let _ = join.join();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn visible_text(session: &TerminalSession) -> String {
        let term = session.term.lock();
        let content = term.renderable_content();
        let mut text = String::new();
        let mut last_line = 0;
        for indexed in content.display_iter {
            if indexed.point.line.0 != last_line {
                text.push('\n');
                last_line = indexed.point.line.0;
            }
            text.push(indexed.cell.c);
        }
        text
    }

    /// A shell that prints and exits at once: on Linux the master's read
    /// fails with EIO (the hangup) in the same wakeup as that last output,
    /// and the SIGCHLD can land before the output is read. Either way the
    /// output must reach the grid and the exit status must reach the pane;
    /// a pane that never gets `ChildExit` neither closes nor shows its
    /// "[process exited]" overlay.
    #[test]
    fn exit_delivers_last_output_and_status() {
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        for run in 0..50 {
            let options = SessionOptions {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "printf last_words; exit 3".into()],
                working_directory: None,
                scrollback: 100,
                env: HashMap::new(),
                images: false,
            };
            let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn sh");
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut status = None;
            while status.is_none() && Instant::now() < deadline {
                while let Ok(event) = rx.try_recv() {
                    if let SessionEvent::Term(AlacEvent::ChildExit(exit)) = event {
                        status = Some(exit.code());
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(status, Some(Some(3)), "run {run}: no exit status");
            assert!(
                visible_text(&session).contains("last_words"),
                "run {run}: last output lost"
            );
        }
    }

    /// The event loop splits parser slices at OSC 133 markers: a real zsh
    /// with the integration installed must deliver `CommandEnd { exit: 1 }`
    /// for `false`, with exact rows, and a non-integrated shell must deliver
    /// no markers at all.
    #[test]
    fn integrated_shell_delivers_markers_with_rows() {
        use super::super::osc::MarkerKind;
        if !std::path::Path::new("/bin/zsh").exists() {
            return;
        }
        let config = crate::config::Config::default();
        let integration = crate::prompt::integration::setup(&config, "/bin/zsh");
        if !integration.env.contains_key("ZDOTDIR") {
            return; // cache dir unavailable in this environment
        }
        let size = TermSize {
            columns: 100,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let mut env = integration.env;
        env.insert("HISTFILE".into(), "/dev/null".into());
        let options = SessionOptions {
            program: "/bin/zsh".into(),
            args: vec![],
            working_directory: Some(std::env::temp_dir()),
            scrollback: 100,
            env,
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn zsh");
        session.write_input(b"false\r".to_vec());

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut markers = Vec::new();
        while Instant::now() < deadline {
            while let Ok(event) = rx.try_recv() {
                if let SessionEvent::Marker(m) = event {
                    markers.push(m);
                }
            }
            let started = markers
                .iter()
                .position(|m| matches!(m.kind, MarkerKind::CommandStart { .. }));
            if started.is_some_and(|ix| {
                markers[ix..]
                    .iter()
                    .any(|m| matches!(m.kind, MarkerKind::CommandEnd { .. }))
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let kinds: Vec<_> = markers.iter().map(|m| &m.kind).collect();
        assert!(kinds.contains(&&MarkerKind::PromptStart), "{kinds:?}");
        assert!(kinds.contains(&&MarkerKind::InputStart), "{kinds:?}");
        // The startup prompt emits its own D;0 before anything runs; the one
        // that matters follows the C marker for `false`.
        let start_ix = markers
            .iter()
            .position(|m| matches!(m.kind, MarkerKind::CommandStart { .. }))
            .expect("C marker");
        let start = &markers[start_ix];
        assert_eq!(
            start.kind,
            MarkerKind::CommandStart {
                cmdline: Some("false".into())
            }
        );
        let end = markers[start_ix..]
            .iter()
            .find(|m| matches!(m.kind, MarkerKind::CommandEnd { .. }))
            .expect("D marker");
        assert_eq!(end.kind, MarkerKind::CommandEnd { exit: Some(1) });
        // Enter moved the cursor to a fresh line before C fired, and `false`
        // prints nothing, so D lands on that same row: exact rows, not
        // "somewhere in the chunk".
        assert_eq!(end.row, start.row, "start {start:?} end {end:?}");
        assert!(
            markers.iter().any(|m| matches!(m.kind, MarkerKind::Cwd(_))),
            "OSC 7 should report the cwd"
        );
        assert!(markers.iter().all(|m| !m.alt_screen));
    }

    #[test]
    fn plain_sh_delivers_no_markers() {
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: "/bin/sh".into(),
            args: vec![],
            working_directory: None,
            scrollback: 100,
            env: HashMap::from([("HISTFILE".to_string(), "/dev/null".to_string())]),
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn sh");
        session.write_input(b"echo marker_free\r".to_vec());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !visible_text(&session).contains("marker_free") {
            std::thread::sleep(Duration::from_millis(50));
        }
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, SessionEvent::Marker(_)),
                "unexpected {event:?}"
            );
        }
    }

    /// An image printed by a real program over a real PTY: its cells land
    /// in the grid, its pixels and placement reach the channel, and the
    /// environment says whose terminal this is.
    #[test]
    fn a_printed_image_reaches_the_grid_and_the_channel() {
        use super::super::graphics::GraphicsEvent;
        use super::super::placeholder::PLACEHOLDER;
        use base64::Engine;
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 2.0,
        };
        let options = SessionOptions {
            program: "/bin/sh".into(),
            args: vec![],
            working_directory: None,
            scrollback: 100,
            env: HashMap::from([
                ("HISTFILE".to_string(), "/dev/null".to_string()),
                ("TERM_PROGRAM".to_string(), "iTerm.app".to_string()),
            ]),
            images: true,
        };
        let (session, mut rx) = TerminalSession::spawn(options, size).expect("spawn sh");
        let file = super::super::images::tests::png(1, 1);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&file);
        session.write_input(
            format!("printf '\\033]1337;File=inline=1:{encoded}\\a'; echo \"<$TERM_PROGRAM>\"\r")
                .into_bytes(),
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            while let Ok(event) = rx.try_recv() {
                if let SessionEvent::Graphics(event) = event {
                    events.push(event);
                }
            }
            let text = visible_text(&session);
            if text.contains(PLACEHOLDER) && text.contains("<OmniPTY>") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = visible_text(&session);
        assert!(text.contains(PLACEHOLDER), "no image cell in:\n{text}");
        assert!(
            text.contains("<OmniPTY>"),
            "TERM_PROGRAM not ours in:\n{text}"
        );
        assert!(
            matches!(&events[..], [
                GraphicsEvent::Image { size: (1, 1), .. },
                GraphicsEvent::Place { spec, .. },
            ] if spec.cell == (16.0, 32.0)),
            "{events:?}"
        );
    }

    /// M3: a real shell runs, output lands in the grid, and input round-trips.
    #[test]
    fn shell_round_trip() {
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let options = SessionOptions {
            program: "/bin/sh".into(),
            args: vec![],
            working_directory: None,
            scrollback: 100,
            env: HashMap::from([("HISTFILE".to_string(), "/dev/null".to_string())]),
            images: true,
        };
        let (session, _rx) = TerminalSession::spawn(options, size).expect("spawn pty");
        session.write_input(b"echo omnipty_roundtrip_$((20+22))\r".to_vec());

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let text = visible_text(&session);
            if text.contains("omnipty_roundtrip_42") {
                break;
            }
            if Instant::now() > deadline {
                panic!("shell output never arrived; grid:\n{text}");
            }
        }

        let cwd = session.foreground_cwd();
        assert!(cwd.is_some(), "foreground cwd lookup failed");
    }
}
