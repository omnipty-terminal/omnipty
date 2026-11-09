//! The session server: `omnipty --server`.
//!
//! A detached process that owns the shells' PTYs so they outlive the window
//! that started them — the tmux arrangement. The GUI starts it on demand
//! and talks to it over a Unix socket (`terminal::remote` has the protocol).
//! Each pane is one thread with its own poller, the shape of the GUI's PTY
//! loop (`terminal::event_loop`) with the parser taken out: output goes into
//! a ring buffer and, when a GUI is attached, down the socket; input comes
//! back up and goes to the PTY. A GUI going away (quit, crash, window
//! closed) is just its socket closing: a pane flagged `persist` keeps
//! running and buffering, any other is hung up. An attach starts with a
//! replay of the ring, which is how the screen comes back.
//!
//! The server exits once it has no panes and no connections. It logs to
//! `<cache>/server.log`; it has no stdio and no crash reporting of its own.
//!
//! windows-port: the listener becomes a named pipe (`platform::win`), the
//! detached spawn is in `remote::start_server`, `fdpass` stays Unix-only.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::terminal::event_loop::{
    PTY_CHILD_EVENT_TOKEN, PTY_READ_WRITE_TOKEN, READ_BUFFER_SIZE, WriteQueue, is_hangup,
    resize_pty,
};
use crate::terminal::remote::{self, Frame, Request, Response, fdpass};
use crate::terminal::session::{TermSize, end_child};

/// The attached GUI's socket, in the pane's poller.
const STREAM_TOKEN: usize = 2;

/// How much of each pane's output is kept for replay.
// ponytail: fixed 2 MiB raw ring per pane; make it an option if anyone asks.
const RING_BYTES: usize = 2 * 1024 * 1024;

fn log(message: impl std::fmt::Display) {
    let path = crate::paths::cache_dir().join("server.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{now} [{}] {message}", std::process::id());
    }
}

/// What a connection thread (or the shutdown) tells a pane thread.
enum PaneMsg {
    /// A GUI wants this pane's I/O. Refused on the wire if it is taken.
    Attach(UnixStream),
    Persist(bool),
    Kill,
}

/// A pane as the rest of the server sees it: enough to find it and to
/// wake its thread.
struct PaneHandle {
    tx: Sender<PaneMsg>,
    poller: Arc<Poller>,
    /// Mirrors the pane thread's `attached`, for `Prune`.
    attached: Arc<AtomicBool>,
}

impl PaneHandle {
    fn send(&self, msg: PaneMsg) -> bool {
        self.tx.send(msg).is_ok() && self.poller.notify().is_ok()
    }
}

struct Server {
    panes: Mutex<HashMap<String, PaneHandle>>,
    connections: AtomicUsize,
    shutting_down: AtomicBool,
    socket: PathBuf,
    on_exit: Box<dyn Fn() + Send + Sync>,
}

impl Server {
    /// Exit once nothing is left to serve. Under the pane lock, so a spawn
    /// racing this either lands first (and keeps the server up) or finds the
    /// socket gone and the GUI falls back to a local shell.
    fn exit_if_idle(&self) {
        let panes = self.panes.lock().unwrap();
        if panes.is_empty() && self.connections.load(Ordering::SeqCst) == 0 {
            self.finish();
        }
    }

    fn finish(&self) {
        log("exiting");
        let _ = std::fs::remove_file(&self.socket);
        (self.on_exit)();
    }

    fn remove_pane(&self, id: &str) {
        self.panes.lock().unwrap().remove(id);
        self.exit_if_idle();
    }
}

/// Run the server on `socket`, holding `lock` so only one does. Returns
/// only when the lock is already held (another server is up). `on_exit` is
/// called, after the panes are gone and the socket removed, when the server
/// is done — `main` exits the process there.
pub fn run(
    socket: &Path,
    lock: &Path,
    on_exit: impl Fn() + Send + Sync + 'static,
) -> io::Result<()> {
    run_inner(socket, lock, on_exit).inspect_err(|e| {
        // The server has no stderr anyone sees; the log is the only trace.
        log(format!("failed to start on {}: {e}", socket.display()));
    })
}

fn run_inner(
    socket: &Path,
    lock: &Path,
    on_exit: impl Fn() + Send + Sync + 'static,
) -> io::Result<()> {
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
        // Only ours: a shared `/tmp` is one place this directory can be
        // (see `remote::runtime_dir`).
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock)?;
    if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(
            ErrorKind::AddrInUse,
            "another session server holds the lock",
        ));
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
    }
    tty::setup_env();
    log(format!("listening on {}", socket.display()));

    let server = Arc::new(Server {
        panes: Mutex::new(HashMap::new()),
        connections: AtomicUsize::new(0),
        shutting_down: AtomicBool::new(false),
        socket: socket.to_path_buf(),
        on_exit: Box::new(on_exit),
    });
    // The lock lives as long as the process.
    std::mem::forget(lock_file);

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if server.shutting_down.load(Ordering::SeqCst) {
            break;
        }
        server.connections.fetch_add(1, Ordering::SeqCst);
        let server = Arc::clone(&server);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(&server, stream) {
                log(format!("connection: {e}"));
            }
            server.connections.fetch_sub(1, Ordering::SeqCst);
            server.exit_if_idle();
        });
    }
    Ok(())
}

/// Requests until the connection closes or is handed to a pane.
fn handle_connection(server: &Arc<Server>, mut stream: UnixStream) -> io::Result<()> {
    let mut reader = io::BufReader::new(stream.try_clone()?);
    while let Some(request) = remote::read_json::<Request>(&mut reader)? {
        let response = match request {
            Request::Spawn {
                id,
                program,
                args,
                cwd,
                env,
                size,
                persist,
            } => match spawn_pane(server, id, program, args, cwd, env, size, persist) {
                Ok(pid) => Response::Spawned { pid },
                Err(e) => Response::Err {
                    message: e.to_string(),
                },
            },
            Request::Attach { id } => {
                let handed = {
                    let panes = server.panes.lock().unwrap();
                    match panes.get(&id) {
                        // The pane thread answers (it holds the master fd)
                        // and owns the connection from here.
                        Some(pane) => pane.send(PaneMsg::Attach(stream.try_clone()?)),
                        None => false,
                    }
                };
                if handed {
                    return Ok(());
                }
                log(format!("{id}: attach refused, no such session"));
                Response::Err {
                    message: format!("no session {id}"),
                }
            }
            Request::Prune { keep } => {
                for (id, pane) in server.panes.lock().unwrap().iter() {
                    if !pane.attached.load(Ordering::SeqCst) && !keep.contains(id) {
                        log(format!("{id}: pruned, no saved workspace refers to it"));
                        pane.send(PaneMsg::Kill);
                    }
                }
                Response::Ok
            }
            Request::Persist { id, value } => {
                if let Some(pane) = server.panes.lock().unwrap().get(&id) {
                    pane.send(PaneMsg::Persist(value));
                }
                Response::Ok
            }
            Request::Kill { id } => {
                if let Some(pane) = server.panes.lock().unwrap().get(&id) {
                    pane.send(PaneMsg::Kill);
                }
                Response::Ok
            }
            Request::Shutdown => {
                shutdown(server);
                Response::Ok
            }
        };
        remote::write_json(&mut stream, &response)?;
    }
    Ok(())
}

/// End every pane, give the shells a moment to go, then exit.
fn shutdown(server: &Arc<Server>) {
    server.shutting_down.store(true, Ordering::SeqCst);
    let handles: Vec<_> = server
        .panes
        .lock()
        .unwrap()
        .values()
        .map(|p| (p.tx.clone(), p.poller.clone()))
        .collect();
    for (tx, poller) in &handles {
        let _ = tx.send(PaneMsg::Kill);
        let _ = poller.notify();
    }
    let server = Arc::clone(server);
    std::thread::spawn(move || {
        // `end_child` escalates to SIGKILL at 300 ms; by 500 ms every pane
        // thread has seen its exit and gone.
        let deadline = std::time::Instant::now() + Duration::from_millis(1500);
        while !server.panes.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        server.finish();
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_pane(
    server: &Arc<Server>,
    id: String,
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    env: HashMap<String, String>,
    size: TermSize,
    persist: bool,
) -> io::Result<u32> {
    let options = tty::Options {
        shell: Some(tty::Shell::new(program, args)),
        working_directory: cwd,
        drain_on_exit: false,
        env,
    };
    let pty = tty::new(&options, size.window_size(), 0)?;
    // The exact pixel size, as the GUI's own loop sets it right after spawn.
    resize_pty(&pty, size);
    let pid = pty.child().id();
    let poller = Arc::new(Poller::new()?);
    let (tx, rx) = mpsc::channel();
    let attached_flag = Arc::new(AtomicBool::new(false));
    let handle = PaneHandle {
        tx,
        poller: Arc::clone(&poller),
        attached: Arc::clone(&attached_flag),
    };
    {
        let mut panes = server.panes.lock().unwrap();
        if panes.contains_key(&id) {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!("session {id} exists"),
            ));
        }
        panes.insert(id.clone(), handle);
    }
    log(format!("spawned {id} pid {pid}"));
    let server = Arc::clone(server);
    let pane = Pane {
        id: id.clone(),
        pid,
        pty,
        poller,
        rx,
        ring: Ring::default(),
        pty_queue: WriteQueue::default(),
        attached: None,
        attached_flag,
        persist,
        was_attached: false,
    };
    std::thread::Builder::new()
        .name(format!("pane {id}"))
        .spawn(move || {
            pane.run();
            server.remove_pane(&id);
        })?;
    Ok(pid)
}

/// The last `RING_BYTES` of a pane's output.
#[derive(Default)]
struct Ring {
    bytes: VecDeque<u8>,
    /// Something has been dropped off the front, so the start is probably
    /// mid-line or mid-escape-sequence.
    wrapped: bool,
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        if self.bytes.len() > RING_BYTES {
            let excess = self.bytes.len() - RING_BYTES;
            self.bytes.drain(..excess);
            self.wrapped = true;
        }
    }

    /// What to replay: everything, or from the first whole line once the
    /// ring has wrapped, so the parser never starts inside a sequence.
    fn replay(&mut self) -> &[u8] {
        let bytes = self.bytes.make_contiguous();
        let start = replay_start(bytes, self.wrapped);
        &bytes[start..]
    }
}

fn replay_start(bytes: &[u8], wrapped: bool) -> usize {
    if !wrapped {
        return 0;
    }
    bytes
        .iter()
        .position(|&b| b == b'\n')
        .map(|ix| ix + 1)
        .unwrap_or(bytes.len())
}

/// A GUI on the other end of the socket.
struct Attached {
    stream: UnixStream,
    decoder: remote::Decoder,
    queue: WriteQueue,
}

struct Pane {
    id: String,
    pid: u32,
    pty: tty::Pty,
    poller: Arc<Poller>,
    rx: Receiver<PaneMsg>,
    ring: Ring,
    pty_queue: WriteQueue,
    attached: Option<Attached>,
    /// `attached.is_some()`, readable from the connection threads.
    attached_flag: Arc<AtomicBool>,
    persist: bool,
    /// Whether a GUI has ever been attached. Output buffered before the
    /// first attach (the shell's startup, which may well include terminal
    /// queries) has never been answered, so it goes out as live `Output`;
    /// only output a GUI has already seen is `Replay`.
    was_attached: bool,
}

impl Pane {
    fn run(mut self) {
        let poll_opts = PollMode::Level;
        let mut interest = PollingEvent::readable(PTY_READ_WRITE_TOKEN);
        if let Err(e) = unsafe { self.pty.register(&self.poller, interest, poll_opts) } {
            log(format!("{}: pty registration failed: {e}", self.id));
            return;
        }
        let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        let mut hung_up = false;
        let mut stream_interest = PollingEvent::readable(STREAM_TOKEN);

        'pane: loop {
            events.clear();
            if let Err(e) = self.poller.wait(&mut events, None) {
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                log(format!("{}: poll failed: {e}", self.id));
                break;
            }

            while let Ok(msg) = self.rx.try_recv() {
                match msg {
                    PaneMsg::Attach(stream) => self.attach(stream, &mut stream_interest),
                    PaneMsg::Persist(value) => self.persist = value,
                    PaneMsg::Kill => {
                        log(format!("{}: kill", self.id));
                        end_child(self.pid as i32, || {});
                    }
                }
            }

            for event in events.iter() {
                match event.key {
                    PTY_CHILD_EVENT_TOKEN => {
                        if let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                            // The last output is usually the error message.
                            let _ = self.pty_read(&mut buf);
                            let raw = status.map(|s| {
                                use std::os::unix::process::ExitStatusExt;
                                s.into_raw()
                            });
                            log(format!("{}: exited {raw:?}", self.id));
                            self.send_exit(raw.unwrap_or(-1));
                            break 'pane;
                        }
                    }
                    PTY_READ_WRITE_TOKEN => {
                        if hung_up {
                            continue;
                        }
                        if event.is_interrupt() {
                            let _ = self.pty_read(&mut buf);
                            self.hang_up(&mut hung_up);
                            continue;
                        }
                        if event.readable
                            && let Err(e) = self.pty_read(&mut buf)
                        {
                            if is_hangup(&e) {
                                self.hang_up(&mut hung_up);
                                continue;
                            }
                            log(format!("{}: pty read failed: {e}", self.id));
                            break 'pane;
                        }
                        if event.writable
                            && let Err(e) = self.pty_queue.write_to(self.pty.writer())
                        {
                            if is_hangup(&e) {
                                self.hang_up(&mut hung_up);
                                continue;
                            }
                            log(format!("{}: pty write failed: {e}", self.id));
                            break 'pane;
                        }
                    }
                    STREAM_TOKEN => {
                        if event.readable || event.is_interrupt() {
                            if let Err(e) = self.stream_read(&mut buf) {
                                self.detach(Some(e));
                                continue;
                            }
                        }
                        if event.writable
                            && let Some(attached) = &mut self.attached
                            && let Err(e) = attached.queue.write_to(&attached.stream)
                        {
                            self.detach(Some(e));
                        }
                    }
                    _ => {}
                }
            }

            let needs_write = self.pty_queue.needs_write();
            if !hung_up && needs_write != interest.writable {
                interest.writable = needs_write;
                if let Err(e) = self.pty.reregister(&self.poller, interest, poll_opts) {
                    log(format!("{}: pty reregistration failed: {e}", self.id));
                    break;
                }
            }
            if let Some(attached) = &self.attached {
                let needs_write = attached.queue.needs_write();
                if needs_write != stream_interest.writable {
                    stream_interest.writable = needs_write;
                    let _ = self.poller.modify_with_mode(
                        &attached.stream,
                        stream_interest,
                        PollMode::Level,
                    );
                }
            }
        }

        let _ = self.pty.deregister(&self.poller);
        if let Some(attached) = self.attached.take() {
            let _ = self.poller.delete(&attached.stream);
        }
        // `self.pty` drops here: it waits on the child, which has exited.
    }

    /// The shell hung up: take the master out of the poller (level-triggered,
    /// it would spin) and wait for the exit status.
    fn hang_up(&mut self, hung_up: &mut bool) {
        if !*hung_up {
            let _ = self.poller.delete(self.pty.file());
            *hung_up = true;
        }
    }

    fn pty_read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        loop {
            match self.pty.reader().read(buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    self.ring.push(&buf[..n]);
                    if let Some(attached) = &mut self.attached {
                        attached.queue.push(
                            remote::header(&Frame::Output(Vec::new()), n)
                                .to_vec()
                                .into(),
                        );
                        attached.queue.push(buf[..n].to_vec().into());
                    }
                }
                Err(e) => match e.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => return Ok(()),
                    _ => return Err(e),
                },
            }
        }
    }

    /// Frames from the GUI: input for the PTY, resizes. EOF is a detach.
    fn stream_read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let Some(attached) = &mut self.attached else {
            return Ok(());
        };
        loop {
            match attached.stream.read(buf) {
                Ok(0) => return Err(io::Error::new(ErrorKind::UnexpectedEof, "gui detached")),
                Ok(n) => attached.decoder.push(&buf[..n]),
                Err(e) => match e.kind() {
                    ErrorKind::Interrupted => continue,
                    ErrorKind::WouldBlock => break,
                    _ => return Err(e),
                },
            }
        }
        while let Some(frame) = attached.decoder.next()? {
            match frame {
                Frame::Input(bytes) if !bytes.is_empty() => self.pty_queue.push(bytes.into()),
                Frame::Input(_) => {}
                Frame::Resize(size) => resize_pty(&self.pty, size),
                // Only the server sends these.
                Frame::Replay(_) | Frame::Output(_) | Frame::Exited(_) => {}
            }
        }
        Ok(())
    }

    /// Answer an attach: the reply line rides with the master fd, then the
    /// ring is queued as the first frame and the socket joins the poller.
    fn attach(&mut self, stream: UnixStream, stream_interest: &mut PollingEvent) {
        if self.attached.is_some() {
            let mut stream = stream;
            let _ = remote::write_json(
                &mut stream,
                &Response::Err {
                    message: format!("session {} is already attached", self.id),
                },
            );
            return;
        }
        // Up before the reply goes out, so a `Prune` racing the GUI's next
        // request never sees this pane as detached.
        self.attached_flag.store(true, Ordering::SeqCst);
        let mut line =
            serde_json::to_vec(&Response::Attached { pid: self.pid }).unwrap_or_default();
        line.push(b'\n');
        if let Err(e) = fdpass::send_fd(&stream, self.pty.file().as_raw_fd(), &line) {
            log(format!("{}: attach reply failed: {e}", self.id));
            self.attached_flag.store(false, Ordering::SeqCst);
            return;
        }
        if stream.set_nonblocking(true).is_err() {
            self.attached_flag.store(false, Ordering::SeqCst);
            return;
        }
        let mut queue = WriteQueue::default();
        let replay = self.ring.replay();
        let tag = if self.was_attached {
            Frame::Replay(Vec::new())
        } else {
            Frame::Output(Vec::new())
        };
        queue.push(remote::header(&tag, replay.len()).to_vec().into());
        queue.push(replay.to_vec().into());
        self.was_attached = true;
        *stream_interest = PollingEvent::readable(STREAM_TOKEN);
        // Level mode, like the PTY: a one-shot interest would go quiet after
        // the first event and never see the GUI's EOF.
        if let Err(e) = unsafe {
            self.poller
                .add_with_mode(&stream, *stream_interest, PollMode::Level)
        } {
            log(format!("{}: attach poll failed: {e}", self.id));
            self.attached_flag.store(false, Ordering::SeqCst);
            return;
        }
        log(format!("{}: attached", self.id));
        self.attached = Some(Attached {
            stream,
            decoder: remote::Decoder::default(),
            queue,
        });
    }

    /// The GUI is gone. A persistent pane carries on; any other is hung up,
    /// the way closing a terminal window hangs up its shell.
    fn detach(&mut self, why: Option<io::Error>) {
        if let Some(attached) = self.attached.take() {
            self.attached_flag.store(false, Ordering::SeqCst);
            let _ = self.poller.delete(&attached.stream);
            log(format!(
                "{}: detached ({}), persist={}",
                self.id,
                why.map(|e| e.to_string()).unwrap_or_default(),
                self.persist
            ));
            if !self.persist {
                end_child(self.pid as i32, || {});
            }
        }
    }

    /// Best effort, synchronously: the pane is about to go.
    fn send_exit(&mut self, raw: i32) {
        if let Some(mut attached) = self.attached.take() {
            let _ = self.poller.delete(&attached.stream);
            let _ = attached.stream.set_nonblocking(false);
            let _ = attached
                .stream
                .set_write_timeout(Some(Duration::from_millis(500)));
            // Whatever output was still queued, then the status.
            while attached.queue.needs_write() {
                if attached.queue.write_to(&attached.stream).is_err() {
                    return;
                }
            }
            let _ = attached
                .stream
                .write_all(&remote::encode(&Frame::Exited(raw)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::remote::{Decoder, Frame, Request};
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn replay_starts_on_a_whole_line_once_wrapped() {
        assert_eq!(replay_start(b"abc\ndef", false), 0);
        assert_eq!(replay_start(b"abc\ndef", true), 4);
        assert_eq!(replay_start(b"no newline", true), 10);
        let mut ring = Ring::default();
        ring.push(&vec![b'x'; RING_BYTES - 2]);
        ring.push(b"\nend");
        assert!(ring.wrapped);
        assert_eq!(ring.replay(), b"end");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omnipty-srv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn size() -> TermSize {
        TermSize {
            columns: 80,
            screen_lines: 24,
            cell_width: 8.0,
            cell_height: 16.0,
            scale: 1.0,
        }
    }

    /// Read frames off an attached stream until `until` says stop or the
    /// deadline passes; returns everything seen.
    fn frames(
        stream: &mut UnixStream,
        leftover: &[u8],
        until: impl Fn(&[Frame]) -> bool,
    ) -> Vec<Frame> {
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut decoder = Decoder::default();
        decoder.push(leftover);
        let mut out = Vec::new();
        let mut buf = [0u8; 65536];
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            while let Some(frame) = decoder.next().unwrap() {
                out.push(frame);
            }
            if until(&out) {
                break;
            }
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => decoder.push(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(e) => panic!("{e}"),
            }
        }
        out
    }

    /// Whether `id` can still be attached. A refused connection counts as
    /// no: the server exits (and removes its socket) once its last pane is
    /// gone, so "gone" can look like either.
    fn pane_alive(socket: &Path, id: &str) -> bool {
        match UnixStream::connect(socket) {
            Ok(stream) => remote::attach_on(stream, id).is_ok(),
            Err(_) => false,
        }
    }

    fn text(frames: &[Frame]) -> String {
        let mut bytes = Vec::new();
        for f in frames {
            if let Frame::Replay(b) | Frame::Output(b) = f {
                bytes.extend_from_slice(b);
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The whole life of a pane through a real server on a temp socket: a
    /// shell is spawned, its output is replayed to a late attach, input
    /// typed through the socket reaches it, a detach keeps it (persist), a
    /// second attach replays everything so far, and a kill ends it — after
    /// which the server, with nothing left, exits.
    #[test]
    fn spawn_detach_reattach_kill() {
        let dir = scratch("life");
        let socket = dir.join("s.sock");
        let lock = dir.join("s.lock");
        let (exit_tx, exit_rx) = mpsc::channel();
        {
            let (socket, lock) = (socket.clone(), lock.clone());
            std::thread::spawn(move || {
                run(&socket, &lock, move || {
                    let _ = exit_tx.send(());
                })
                .unwrap();
            });
        }
        let connect = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(s) = UnixStream::connect(&socket) {
                    return s;
                }
                assert!(Instant::now() < deadline, "server never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let id = "test-1-0".to_string();
        let pid = remote::spawn_on(
            connect(),
            Request::Spawn {
                id: id.clone(),
                program: "/bin/sh".into(),
                args: vec![],
                cwd: Some(std::env::temp_dir()),
                env: HashMap::from([
                    ("PS1".to_string(), "$ ".to_string()),
                    ("HISTFILE".to_string(), "/dev/null".to_string()),
                ]),
                size: size(),
                persist: true,
            },
        )
        .expect("spawn");
        assert!(pid > 0);
        // Let the shell print its prompt before anyone attaches: that output
        // must come back from the ring, not live.
        std::thread::sleep(Duration::from_millis(300));

        let mut first = remote::attach_on(connect(), &id).expect("attach");
        assert_eq!(first.pid, pid);
        let pgrp = unsafe { libc::tcgetpgrp(first.master.as_raw_fd()) };
        assert!(pgrp > 0, "the passed fd is the PTY master");
        // Nobody has seen the prompt yet, so it is live output, not replay:
        // the GUI must answer any startup queries in it.
        let seen = frames(
            &mut first.stream,
            &first.leftover,
            |f| matches!(f.first(), Some(Frame::Output(b)) if !b.is_empty()),
        );
        assert!(matches!(seen.first(), Some(Frame::Output(_))), "{seen:?}");
        first
            .stream
            .write_all(&remote::encode(&Frame::Input(
                b"echo omnipty_server_$((40+2))\r".to_vec(),
            )))
            .unwrap();
        let seen = frames(&mut first.stream, &[], |f| {
            text(f).contains("omnipty_server_42")
        });
        assert!(text(&seen).contains("omnipty_server_42"), "{}", text(&seen));
        // A second attach while the first holds the pane is refused.
        assert!(remote::attach_on(connect(), &id).is_err());
        drop(first);

        // Detached but persistent: still there, and the replay has it all.
        std::thread::sleep(Duration::from_millis(200));
        let mut second = remote::attach_on(connect(), &id).expect("reattach");
        let seen = frames(&mut second.stream, &second.leftover, |f| {
            text(f).contains("omnipty_server_42")
        });
        assert!(matches!(seen.first(), Some(Frame::Replay(_))), "{seen:?}");
        assert!(text(&seen).contains("omnipty_server_42"), "{}", text(&seen));

        remote::request_on(connect(), Request::Kill { id: id.clone() }).unwrap();
        let seen = frames(&mut second.stream, &[], |f| {
            f.iter().any(|x| matches!(x, Frame::Exited(_)))
        });
        assert!(
            seen.iter().any(|x| matches!(x, Frame::Exited(_))),
            "{seen:?}"
        );
        drop(second);
        exit_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server exits once idle");
        assert!(!socket.exists(), "socket removed on exit");
        assert!(!pane_alive(&socket, &id), "a killed pane is gone");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A pane that is not persistent dies with its GUI.
    #[test]
    fn an_unpinned_pane_ends_when_its_gui_detaches() {
        let dir = scratch("detach");
        let socket = dir.join("s.sock");
        let lock = dir.join("s.lock");
        {
            let (socket, lock) = (socket.clone(), lock.clone());
            std::thread::spawn(move || {
                run(&socket, &lock, || {}).expect("server runs");
            });
        }
        let connect = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(s) = UnixStream::connect(&socket) {
                    return s;
                }
                assert!(Instant::now() < deadline, "server never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let id = "test-2-0".to_string();
        remote::spawn_on(
            connect(),
            Request::Spawn {
                id: id.clone(),
                program: "/bin/sh".into(),
                args: vec![],
                cwd: None,
                env: HashMap::new(),
                size: size(),
                persist: false,
            },
        )
        .expect("spawn");
        let attached = remote::attach_on(connect(), &id).expect("attach");
        drop(attached);
        let deadline = Instant::now() + Duration::from_secs(5);
        while pane_alive(&socket, &id) {
            assert!(Instant::now() < deadline, "pane survived its gui");
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prune_ends_only_detached_panes_nobody_saved() {
        let dir = scratch("prune");
        let socket = dir.join("s.sock");
        let lock = dir.join("s.lock");
        {
            let (socket, lock) = (socket.clone(), lock.clone());
            std::thread::spawn(move || {
                run(&socket, &lock, || {}).expect("server runs");
            });
        }
        let connect = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Ok(s) = UnixStream::connect(&socket) {
                    return s;
                }
                assert!(Instant::now() < deadline, "server never listened");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        for id in ["attached", "saved", "orphan"] {
            remote::spawn_on(
                connect(),
                Request::Spawn {
                    id: id.into(),
                    program: "/bin/sh".into(),
                    args: vec![],
                    cwd: None,
                    env: HashMap::new(),
                    size: size(),
                    persist: true,
                },
            )
            .expect("spawn");
        }
        let held = remote::attach_on(connect(), "attached").expect("attach");
        remote::request_on(
            connect(),
            Request::Prune {
                keep: vec!["saved".into()],
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pane_alive(&socket, "orphan") {
            assert!(Instant::now() < deadline, "orphan survived the prune");
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(held);
        std::thread::sleep(Duration::from_millis(100));
        assert!(pane_alive(&socket, "attached"), "attached pane pruned");
        assert!(pane_alive(&socket, "saved"), "kept pane pruned");
        remote::request_on(connect(), Request::Shutdown).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
