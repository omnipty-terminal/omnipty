//! The PTY I/O thread: reads the shell's output, feeds the VT parser, writes
//! input back, and watches for the child exiting.
//!
//! This replaces `alacritty_terminal::event_loop::EventLoop` so the byte
//! stream can be split at the OSC markers OmniPTY's shell integration emits.
//! Alacritty's parser drops unknown OSCs before anyone can see them; here
//! each recognised marker ends a parser slice, and the cursor is sampled in
//! between, so a marker's grid row is exact rather than "somewhere in the
//! chunk". Images take the same route: the scanner lifts them out of the
//! stream and they are placed between two parser slices, at the cursor.
//! The write channel, resize, shutdown, and child-exit handling keep the
//! same semantics as alacritty's loop — `TerminalSession::drop`'s
//! SIGHUP → join → SIGKILL teardown depends on them.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite};
use alacritty_terminal::vte::ansi;
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use super::graphics::{GraphicsEvent, GraphicsState};
use super::osc::Marker;
use super::scan::{Action, Scanner};
use super::session::TermSize;

/// The poll keys alacritty's `Pty::register` uses (private constants in
/// `tty::unix`): the master fd, and the SIGCHLD pipe.
const PTY_READ_WRITE_TOKEN: usize = 0;
const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// Linux: once nothing has the slave side open (the shell, and everything
/// it started, has gone), the master reports a hangup, and reading or
/// writing it fails with EIO. That's the end of the output, not an I/O
/// fault, and the child's exit status is still on its way through the
/// SIGCHLD pipe. alacritty's own loop treats it the same way.
fn is_hangup(err: &io::Error) -> bool {
    cfg!(target_os = "linux") && err.raw_os_error() == Some(libc::EIO)
}

/// A PTY failure that ends this pane's I/O. Was only an `eprintln!`, which
/// goes nowhere when OmniPTY wasn't started from a terminal. Linux hangups
/// never get here: `is_hangup` is checked first.
fn pty_failed(what: &'static str, err: &io::Error) {
    eprintln!("omnipty: {what}: {err}");
    crate::telemetry::report(what, err);
}

/// Max bytes to read from the PTY before forcing terminal synchronisation.
const READ_BUFFER_SIZE: usize = 0x10_0000;
/// Max bytes to parse while holding the terminal lock in one go.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// Messages the loop accepts.
#[derive(Debug)]
pub enum Msg {
    /// Bytes to write to the PTY.
    Input(Cow<'static, [u8]>),
    /// Stop the loop; the PTY (and with it the child) is dropped afterwards.
    Shutdown,
    /// Propagate a new window size to the PTY.
    Resize(TermSize),
    /// `images.enabled` changed.
    ImagesEnabled(bool),
    /// The main thread dropped these images.
    ForgetImages(Vec<u32>),
}

/// What the loop reports back to the main thread, multiplexed on one
/// channel so ordering between output, markers and images is preserved: an
/// image's events always arrive before the `Wakeup` that repaints its cells.
#[derive(Debug)]
pub enum SessionEvent {
    Term(AlacEvent),
    Marker(Marker),
    Graphics(GraphicsEvent),
}

/// What parsing a chunk produced besides changes to the `Term`.
#[derive(Debug, Default)]
pub struct Outputs {
    /// Markers and graphics events, in stream order.
    pub events: Vec<SessionEvent>,
    /// Answers to queries, to write back to the PTY.
    pub replies: Vec<u8>,
}

impl Outputs {
    pub fn graphics(&mut self, event: GraphicsEvent) {
        self.events.push(SessionEvent::Graphics(event));
    }
}

/// Run one chunk of PTY output through the scanner, the VT parser and the
/// graphics state. Parsing happens in slices that end at each marker or
/// image, so the cursor read in between is exact. The sequences themselves
/// are strings the parser ignores, so they move nothing.
///
/// One gap: inside a synchronised update (`CSI ? 2026 h`) the parser holds
/// the bytes back, so a marker's row is sampled where the cursor was when
/// the update began. Images end the update early instead (see
/// `GraphicsState::handle`).
pub fn feed<L: EventListener>(
    term: &mut Term<L>,
    parser: &mut ansi::Processor,
    scanner: &mut Scanner,
    graphics: &mut GraphicsState,
    chunk: &[u8],
) -> Outputs {
    let mut out = Outputs::default();
    for action in scanner.scan(chunk) {
        match action {
            Action::Parse(range) => parser.advance(term, &chunk[range]),
            Action::Marker(kind) => {
                let grid = term.grid();
                let cursor = grid.cursor.point;
                out.events.push(SessionEvent::Marker(Marker {
                    kind,
                    row: grid.history_size() + cursor.line.0.max(0) as usize,
                    column: cursor.column.0,
                    alt_screen: term.mode().contains(TermMode::ALT_SCREEN),
                }));
            }
            Action::Graphics(command) => graphics.handle(term, parser, command, &mut out),
            Action::Reply(reply) => graphics.reply(term, reply, &mut out.replies),
        }
    }
    out
}

/// Handle for sending messages to the loop; cloneable, wakes the poller.
#[derive(Clone)]
pub struct LoopSender {
    tx: Sender<Msg>,
    poller: Arc<Poller>,
}

impl LoopSender {
    pub fn send(&self, msg: Msg) -> io::Result<()> {
        self.tx
            .send(msg)
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "event loop gone"))?;
        self.poller.notify()
    }

    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let bytes = bytes.into();
        // A zero-length write hangs the loop's writer state machine.
        if bytes.is_empty() {
            return;
        }
        let _ = self.send(Msg::Input(bytes));
    }
}

pub struct EventLoop<L: EventListener> {
    poll: Arc<Poller>,
    pty: tty::Pty,
    rx: PeekableReceiver<Msg>,
    tx: Sender<Msg>,
    term: Arc<FairMutex<Term<L>>>,
    listener: L,
    events: Box<dyn Fn(SessionEvent) + Send>,
    scanner: Scanner,
    graphics: GraphicsState,
}

impl<L: EventListener + Send + 'static> EventLoop<L> {
    pub fn new(
        term: Arc<FairMutex<Term<L>>>,
        listener: L,
        pty: tty::Pty,
        graphics: GraphicsState,
        events: impl Fn(SessionEvent) + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        Ok(Self {
            poll: Arc::new(Poller::new()?),
            pty,
            rx: PeekableReceiver::new(rx),
            tx,
            term,
            listener,
            events: Box::new(events),
            scanner: Scanner::new(),
            graphics,
        })
    }

    pub fn sender(&self) -> LoopSender {
        LoopSender {
            tx: self.tx.clone(),
            poller: self.poll.clone(),
        }
    }

    /// Returns false when a shutdown was requested.
    fn drain_channel(&mut self, state: &mut State) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input(input) => state.write_list.push_back(input),
                Msg::Resize(size) => {
                    self.graphics.set_cell(size.cell_pixels());
                    // Not alacritty's `on_resize`: its `WindowSize` holds a
                    // whole-pixel cell, and columns times a rounded 16.8
                    // misstates the width by a cell or more.
                    let winsize = size.winsize();
                    unsafe {
                        libc::ioctl(
                            self.pty.file().as_raw_fd(),
                            libc::TIOCSWINSZ,
                            &winsize as *const libc::winsize,
                        )
                    };
                }
                Msg::ImagesEnabled(enabled) => self.graphics.set_enabled(enabled),
                Msg::ForgetImages(ids) => self.graphics.forget(&ids),
                Msg::Shutdown => return false,
            }
        }
        true
    }

    fn pty_read(&mut self, state: &mut State, buf: &mut [u8]) -> io::Result<()> {
        let mut unprocessed = 0;
        let mut processed = 0;
        // Hung up with output read but not yet parsed: parse it before
        // reporting the hangup (the next read fails the same way).
        let mut hangup_pending = false;

        // Reserve the next terminal lock for PTY reading.
        let _lease = Some(self.term.lease());
        let mut terminal = None;

        loop {
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        if unprocessed == 0 {
                            break;
                        }
                    }
                    _ if is_hangup(&err) && unprocessed > 0 => hangup_pending = true,
                    _ => return Err(err),
                },
            }

            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.term.try_lock_unfair() {
                    // Nothing more is coming, so don't wait for a full buffer.
                    None if hangup_pending || unprocessed >= READ_BUFFER_SIZE => {
                        self.term.lock_unfair()
                    }
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            let out = feed(
                &mut **terminal,
                &mut state.parser,
                &mut self.scanner,
                &mut self.graphics,
                &buf[..unprocessed],
            );
            for event in out.events {
                (self.events)(event);
            }
            // Straight onto the write queue, not round the main thread the
            // way alacritty's own answers go. A program that asks "kitty
            // graphics?" and then for DA1 takes whichever answer comes
            // first as the verdict, and ours must win that race.
            if !out.replies.is_empty() {
                state.write_list.push_back(out.replies.into());
            }

            processed += unprocessed;
            unprocessed = 0;

            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        // Queue a redraw unless everything parsed was inside a synchronised
        // update (DCS 2026), which repaints when it ends.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.listener.send_event(AlacEvent::Wakeup);
        }
        Ok(())
    }

    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();
        'write_many: while let Some(mut current) = state.take_current() {
            loop {
                match self.pty.writer().write(current.remaining()) {
                    Ok(0) => {
                        state.writing = Some(current);
                        break 'write_many;
                    }
                    Ok(n) => {
                        current.written += n;
                        if current.finished() {
                            state.goto_next();
                            break;
                        }
                    }
                    Err(err) => {
                        state.writing = Some(current);
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The shell hung up (see `is_hangup`). Take the master out of the
    /// poller: level-triggered, it would wake the loop continuously until
    /// the SIGCHLD arrives. Paint what was parsed, then wait for the exit.
    fn hang_up(&mut self, hung_up: &mut bool) {
        if !*hung_up {
            let _ = self.poll.delete(self.pty.file());
            *hung_up = true;
            self.listener.send_event(AlacEvent::Wakeup);
        }
    }

    /// Run the loop on its own thread. The returned handle yields once the
    /// loop has exited and the PTY has been dropped (which waits on the
    /// child), so callers join it off the main thread.
    pub fn spawn(mut self) -> JoinHandle<()> {
        alacritty_terminal::thread::spawn_named("PTY reader", move || {
            let mut state = State::default();
            let mut buf = vec![0u8; READ_BUFFER_SIZE];
            let mut hung_up = false;

            let poll_opts = PollMode::Level;
            let mut interest = PollingEvent::readable(0);
            if let Err(err) = unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                pty_failed("pty registration failed", &err);
                return;
            }

            let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());

            'event_loop: loop {
                // Wake for a synchronised-update timeout even with no I/O.
                let timeout = state
                    .parser
                    .sync_timeout()
                    .sync_timeout()
                    .map(|t| t.saturating_duration_since(Instant::now()));

                events.clear();
                if let Err(err) = self.poll.wait(&mut events, timeout) {
                    match err.kind() {
                        ErrorKind::Interrupted => continue,
                        _ => {
                            pty_failed("pty poll failed", &err);
                            break 'event_loop;
                        }
                    }
                }

                if events.is_empty() && self.rx.peek().is_none() {
                    state.parser.stop_sync(&mut *self.term.lock());
                    self.listener.send_event(AlacEvent::Wakeup);
                    continue;
                }

                if !self.drain_channel(&mut state) {
                    break;
                }

                for event in events.iter() {
                    match event.key {
                        PTY_CHILD_EVENT_TOKEN => {
                            if let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                                // The shell can exit with its last output still
                                // unread, and for a failing command that output
                                // is the error message. Read it before the pane
                                // hears about the exit (alacritty: drain_on_exit).
                                let _ = self.pty_read(&mut state, &mut buf);
                                if let Some(status) = status {
                                    self.listener.send_event(AlacEvent::ChildExit(status));
                                }
                                self.term.lock().exit();
                                self.listener.send_event(AlacEvent::Wakeup);
                                break 'event_loop;
                            }
                        }
                        PTY_READ_WRITE_TOKEN => {
                            if hung_up {
                                continue;
                            }
                            if event.is_interrupt() {
                                // epoll's HUP (kqueue never sets it): the same
                                // hangup, reported before a read failed. Take
                                // whatever output is left, then wait.
                                let _ = self.pty_read(&mut state, &mut buf);
                                self.hang_up(&mut hung_up);
                                continue;
                            }
                            if event.readable
                                && let Err(err) = self.pty_read(&mut state, &mut buf)
                            {
                                if is_hangup(&err) {
                                    self.hang_up(&mut hung_up);
                                    continue;
                                }
                                pty_failed("pty read failed", &err);
                                break 'event_loop;
                            }
                            if event.writable
                                && let Err(err) = self.pty_write(&mut state)
                            {
                                if is_hangup(&err) {
                                    self.hang_up(&mut hung_up);
                                    continue;
                                }
                                pty_failed("pty write failed", &err);
                                break 'event_loop;
                            }
                        }
                        _ => {}
                    }
                }

                // After a hangup the master is out of the poller: reregistering
                // would fail and end the loop before the exit status arrives.
                let needs_write = state.needs_write();
                if !hung_up && needs_write != interest.writable {
                    interest.writable = needs_write;
                    if let Err(err) = self.pty.reregister(&self.poll, interest, poll_opts) {
                        pty_failed("pty reregistration failed", &err);
                        break 'event_loop;
                    }
                }
            }

            // After a hangup this fails on the master, which is already out,
            // and leaves the SIGCHLD pipe registered until the Pty drops and
            // closes it, just below. Harmless.
            let _ = self.pty.deregister(&self.poll);
            // `self` (and the Pty, which waits on the child) drops here, on
            // this thread.
        })
    }
}

struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

impl Writing {
    fn remaining(&self) -> &[u8] {
        &self.source[self.written..]
    }

    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

#[derive(Default)]
struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
}

impl State {
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    fn goto_next(&mut self) {
        self.writing = self
            .write_list
            .pop_front()
            .map(|source| Writing { source, written: 0 });
    }

    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }
}

struct PeekableReceiver<T> {
    rx: Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }
        self.peeked.as_ref()
    }

    fn recv(&mut self) -> Option<T> {
        if self.peeked.is_some() {
            return self.peeked.take();
        }
        match self.rx.try_recv() {
            // Every sender dropped: treat like a shutdown rather than spin.
            Err(TryRecvError::Disconnected) => Some(Self::disconnected()),
            res => res.ok(),
        }
    }

    fn disconnected() -> T
    where
        T: Sized,
    {
        unreachable!("the loop keeps its own sender alive")
    }
}
