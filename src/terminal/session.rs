use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

pub use super::event_loop::SessionEvent;
use super::event_loop::{Endpoint, EventLoop, LoopSender, Msg, Remote};
use super::graphics::GraphicsState;
use super::remote;

/// Bridge from the PTY thread to the GPUI main thread. Invoked on the PTY
/// reader thread, possibly while it holds the term lock — it must do nothing
/// but send on the channel.
///
/// While `replaying` is set (the event loop feeding a session server's
/// history), events that answer or act on the program's requests are
/// dropped: the DA1 / DECRQM / OSC colour replies would land in the shell as
/// keystrokes, a clipboard read would paste, a bell would ring for last
/// week's output.
#[derive(Clone)]
pub struct EventProxy(UnboundedSender<SessionEvent>, Arc<AtomicBool>);

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        if self.1.load(Ordering::Relaxed)
            && matches!(
                event,
                AlacEvent::PtyWrite(_)
                    | AlacEvent::ColorRequest(..)
                    | AlacEvent::ClipboardLoad(..)
                    | AlacEvent::ClipboardStore(..)
                    | AlacEvent::Bell
            )
        {
            return;
        }
        self.0.unbounded_send(SessionEvent::Term(event)).ok();
    }
}

/// Grid geometry: cell counts plus the measured cell box, in logical pixels
/// at `scale` device pixels each. Serialised as part of the session-server
/// protocol (`remote::Request::Spawn`).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
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

#[derive(Clone)]
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

/// What the session keeps of the PTY for process introspection and
/// teardown, once alacritty's `Pty` has moved onto the I/O thread.
#[derive(Clone, Copy)]
struct PtyHandle {
    master_fd: RawFd,
    child_pid: i32,
}

impl PtyHandle {
    fn of(pty: &tty::Pty) -> Self {
        Self {
            master_fd: pty.file().as_raw_fd(),
            child_pid: pty.child().id() as i32,
        }
    }
}

/// End the shell behind a PTY whose I/O loop has been told to stop: hang it
/// up now, run `wait` (which should block until the `Pty`, and with it the
/// child's `wait()`, has been dropped) off the calling thread, and kill the
/// shell outright if it is still there after a grace period. An interactive
/// shell that ignores SIGHUP would otherwise hang a quit indefinitely (this
/// presented as "the app won't close").
pub(crate) fn end_child(child_pid: i32, wait: impl FnOnce() + Send + 'static) {
    unsafe {
        libc::kill(child_pid, libc::SIGHUP);
    }
    std::thread::spawn(move || {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            unsafe {
                libc::kill(child_pid, libc::SIGKILL);
            }
        });
        wait();
    });
}

pub struct TerminalSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    sender: LoopSender,
    handle: PtyHandle,
    join: Option<JoinHandle<()>>,
    /// Unique per spawn; exported to the shell as `OMNIPTY_SESSION` so the
    /// silent-cd/run handlers read their own channel file. For a shell the
    /// session server runs, also its name there.
    session_id: String,
    /// The shell lives in the session server, not this process: dropping
    /// the session detaches rather than hanging the shell up.
    remote: bool,
    /// Remote: our copy of the PTY master, which `handle.master_fd` names.
    /// Never read or written, only asked `tcgetpgrp`.
    _master: Option<OwnedFd>,
}

/// A session id nothing else in this process (or a previous or later one)
/// will reuse: pid, a counter, and the process's start time, so a saved id
/// can't be mistaken for a pane of some later process with the same pid.
/// Only ever used as a file name and a protocol key, so it is kept to
/// characters no shell quotes.
fn next_session_id() -> String {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static STARTED: OnceLock<u64> = OnceLock::new();
    let started = STARTED.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    format!(
        "{}-{}-{started}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// The environment every shell gets, over whatever the caller added.
fn session_env(mut env: HashMap<String, String>, session_id: &str) -> HashMap<String, String> {
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
    env.insert("OMNIPTY_SESSION".into(), session_id.to_string());
    env.insert("OXIDE_SESSION".into(), session_id.to_string());
    // GUI-launched apps get no locale; a C-locale shell breaks multibyte
    // input and prompt glyphs. Mirror Terminal.app: set one if absent.
    if std::env::var("LANG").is_err() && !env.contains_key("LANG") {
        env.insert("LANG".into(), "en_US.UTF-8".into());
    }
    env
}

impl TerminalSession {
    /// A shell in a PTY this process owns.
    pub fn spawn(
        options: SessionOptions,
        size: TermSize,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        tty::setup_env();
        let session_id = next_session_id();
        let env = session_env(options.env, &session_id);
        let pty_options = tty::Options {
            shell: Some(tty::Shell::new(options.program, options.args)),
            working_directory: options.working_directory,
            drain_on_exit: false,
            env,
        };
        let pty = tty::new(&pty_options, size.window_size(), 0)?;
        let handle = PtyHandle::of(&pty);
        Self::start(
            Endpoint::Local(pty),
            handle,
            session_id,
            None,
            options.scrollback,
            options.images,
            size,
        )
    }

    /// A shell the session server runs for us, so it outlives this process.
    /// `persist` is whether it should (a pinned workspace's pane). Fails
    /// when the server can't be reached or started; the caller falls back
    /// to `spawn`.
    pub fn spawn_remote(
        options: SessionOptions,
        size: TermSize,
        persist: bool,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        let session_id = next_session_id();
        let env = session_env(options.env, &session_id);
        let stream = remote::connect_or_start()?;
        remote::spawn_on(
            stream,
            remote::Request::Spawn {
                id: session_id.clone(),
                program: options.program,
                args: options.args,
                cwd: options.working_directory,
                env,
                size,
                persist,
            },
        )?;
        Self::attach(&session_id, options.scrollback, options.images, size)
    }

    /// Take over a shell the session server already runs — a pinned
    /// workspace's pane from before the last quit. Its buffered output is
    /// replayed into the fresh grid. Fails when the server or the pane is
    /// gone; the caller starts a new shell then.
    pub fn attach(
        id: &str,
        scrollback: usize,
        images: bool,
        size: TermSize,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        Self::attach_on(remote::connect()?, id, scrollback, images, size)
    }

    /// `attach` over a connection the caller made (tests use a server on a
    /// socket of their own).
    pub fn attach_on(
        stream: remote::Stream,
        id: &str,
        scrollback: usize,
        images: bool,
        size: TermSize,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        let attached = remote::attach_on(stream, id)?;
        let handle = PtyHandle {
            master_fd: attached.master.as_raw_fd(),
            child_pid: attached.pid as i32,
        };
        let endpoint = Endpoint::Remote(Remote::new(attached.stream, attached.leftover, size));
        Self::start(
            endpoint,
            handle,
            id.to_string(),
            Some(attached.master),
            scrollback,
            images,
            size,
        )
    }

    fn start(
        endpoint: Endpoint,
        handle: PtyHandle,
        session_id: String,
        master: Option<OwnedFd>,
        scrollback: usize,
        images: bool,
        size: TermSize,
    ) -> std::io::Result<(Self, UnboundedReceiver<SessionEvent>)> {
        let (tx, rx) = unbounded();
        let replaying = Arc::new(AtomicBool::new(false));
        let proxy = EventProxy(tx, Arc::clone(&replaying));
        let remote = matches!(endpoint, Endpoint::Remote(_));

        let term_config = TermConfig {
            scrolling_history: scrollback,
            ..TermConfig::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(term_config, &size, proxy.clone())));

        let event_tx = proxy.0.clone();
        let graphics = GraphicsState::new(images, size.cell_pixels());
        let event_loop = EventLoop::new(
            Arc::clone(&term),
            proxy,
            replaying,
            endpoint,
            graphics,
            move |event| {
                event_tx.unbounded_send(event).ok();
            },
        )?;
        let sender = event_loop.sender();
        let join = event_loop.spawn();

        Ok((
            Self {
                term,
                sender,
                handle,
                join: Some(join),
                session_id,
                remote,
                _master: master,
            },
            rx,
        ))
    }

    /// The session server's name for this shell, if it runs there.
    pub fn remote_id(&self) -> Option<&str> {
        self.remote.then_some(self.session_id.as_str())
    }

    /// End the shell for good. A local shell ends when the session drops; a
    /// remote one would otherwise live on if its pane is marked persistent,
    /// so a pane the user closes says so explicitly.
    pub fn kill(&self) {
        if self.remote {
            let _ = remote::kill(&self.session_id);
        }
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
        let pgrp = unsafe { libc::tcgetpgrp(self.handle.master_fd) };
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
            let pgrp = libc::tcgetpgrp(self.handle.master_fd);
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
        super::process::foreground(self.handle.master_fd)
    }

    /// Swap the terminal's options (cursor style, scrollback) in place, for
    /// a config reload. The PTY is untouched.
    pub fn set_term_options(&self, config: TermConfig) {
        self.term.lock().set_options(config);
    }
}

impl Drop for TerminalSession {
    /// Teardown must never block the main thread: stop the loop, then let
    /// `end_child` signal the shell and join the loop (which drops the `Pty`,
    /// and with it waits on the child) off this thread. A remote shell is
    /// only detached — closing the socket is the whole of it, and the server
    /// decides from the pane's persist flag whether the shell goes on.
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
        let join = self.join.take();
        let wait = move || {
            if let Some(join) = join {
                let _ = join.join();
            }
        };
        if self.remote {
            std::thread::spawn(wait);
        } else {
            end_child(self.handle.child_pid, wait);
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

    /// A shell in the session server: typed input reaches it through the
    /// socket, dropping the session leaves it running, and a second attach
    /// rebuilds the grid from the replay — the earlier output is there
    /// without the shell printing it again. The passed master fd answers
    /// `tcgetpgrp`, so cwd polling works on the reattached pane too.
    #[test]
    fn remote_session_survives_a_detach_and_replays() {
        let dir = std::env::temp_dir().join(format!("omnipty-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock");
        {
            let (socket, lock) = (socket.clone(), dir.join("s.lock"));
            std::thread::spawn(move || {
                crate::server::run(&socket, &lock, || {}).expect("server runs");
            });
        }
        let connect = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(s) = remote::Stream::connect(&socket) {
                    return s;
                }
                assert!(Instant::now() < deadline, "server never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let size = TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        };
        let id = "remote-test-0".to_string();
        remote::spawn_on(
            connect(),
            remote::Request::Spawn {
                id: id.clone(),
                program: "/bin/sh".into(),
                args: vec![],
                cwd: None,
                env: session_env(
                    HashMap::from([("HISTFILE".to_string(), "/dev/null".to_string())]),
                    &id,
                ),
                size,
                persist: true,
            },
        )
        .expect("spawn via server");

        let (session, mut rx) =
            TerminalSession::attach_on(connect(), &id, 100, false, size).expect("attach");
        assert_eq!(session.remote_id(), Some(id.as_str()));
        // The DA1 query goes to the terminal, which answers it once, now.
        session.write_input(b"printf '\\033[c'; echo omnipty_remote_$((20+22))\r".to_vec());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !visible_text(&session).contains("omnipty_remote_42") {
            assert!(
                Instant::now() < deadline,
                "no output through the server:\n{}",
                visible_text(&session)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            session.foreground_cwd().is_some(),
            "tcgetpgrp on the passed master"
        );
        let answered = |rx: &mut UnboundedReceiver<SessionEvent>| {
            let mut n = 0;
            while let Ok(event) = rx.try_recv() {
                if matches!(event, SessionEvent::Term(AlacEvent::PtyWrite(_))) {
                    n += 1;
                }
            }
            n
        };
        assert!(answered(&mut rx) > 0, "the live DA1 query is answered");
        drop(session);

        // Detached, not dead: a fresh grid gets the old output replayed.
        std::thread::sleep(Duration::from_millis(200));
        let (again, mut rx) =
            TerminalSession::attach_on(connect(), &id, 100, false, size).expect("reattach");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !visible_text(&again).contains("omnipty_remote_42") {
            assert!(
                Instant::now() < deadline,
                "replay missing:\n{}",
                visible_text(&again)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // The replayed query must not be answered again: the reply would
        // land in the shell as keystrokes.
        assert_eq!(answered(&mut rx), 0, "replayed DA1 answered");
        // (`kill()` would talk to the real socket; this server is on a test one.)
        remote::request_on(connect(), remote::Request::Kill { id: id.clone() }).unwrap();
        drop(again);
        // The server exits (socket and all) once its last pane is gone, so
        // a refused connection is the same answer as a refused attach.
        let deadline = Instant::now() + Duration::from_secs(5);
        while let Ok(stream) = remote::Stream::connect(&socket)
            && remote::attach_on(stream, &id).is_ok()
        {
            assert!(Instant::now() < deadline, "killed pane still attachable");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(dir);
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
