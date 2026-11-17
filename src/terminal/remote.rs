//! The session-server protocol: what the GUI and `omnipty --server` say to
//! each other, and the client side of saying it.
//!
//! A shell can only outlive the window that started it if some other
//! process owns its PTY, so with `workspaces.persist_sessions` every pane's
//! shell is spawned by a detached server (`crate::server`) and the GUI talks
//! to it over a Unix socket. One connection per pane: a few lines of JSON to
//! set up, then a stream of length-prefixed frames both ways — the GUI
//! sends what it would have written to the PTY, the server sends what it
//! read from it. The server keeps the last few MiB of output per pane, so a
//! GUI that attaches later gets it replayed and rebuilds the screen.
//!
//! The protocol version is in the socket's name (`server-v1.sock`): a GUI
//! and a server that disagree never meet, and an old server finishes its
//! panes and exits on its own.
//!
//! windows-port: `Stream` becomes a `File` on a named pipe
//! (`\\.\pipe\omnipty-v1-<SID>`), `connect`/`start_server` grow their
//! arms, `fdpass` stays Unix-only (the attach reply's pid is enough there).

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::session::TermSize;

/// Bumped when the shape of anything below changes incompatibly; it names
/// the socket, so two versions never share one.
pub const PROTOCOL: u32 = 1;

pub type Stream = std::os::unix::net::UnixStream;

/// Where the socket and its lock live: the cache directory, unless that
/// path is too long for a socket. macOS caps `sun_path` at 104 bytes (Linux
/// at 108), and a deep home directory would otherwise fail the bind — in a
/// detached process with no stderr, which is to say silently. The per-user
/// temp directory is short on both platforms.
pub fn runtime_dir() -> PathBuf {
    let cache = crate::paths::cache_dir();
    if cache.as_os_str().len() + 20 < 100 {
        return cache;
    }
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("omnipty-{uid}"))
}

/// `~/.cache/omnipty/server-v1.sock`.
pub fn endpoint() -> PathBuf {
    runtime_dir().join(format!("server-v{PROTOCOL}.sock"))
}

/// The lock the server takes before binding, so two GUIs starting at once
/// end up with one server.
pub fn lock_path() -> PathBuf {
    runtime_dir().join(format!("server-v{PROTOCOL}.lock"))
}

/// One line of JSON from the GUI; the server answers each with one
/// `Response` line. After `Attached`, the same connection carries frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "req", rename_all = "snake_case")]
pub enum Request {
    /// Start a shell. `id` is the pane's `OMNIPTY_SESSION`; the GUI picks it
    /// because the shell's own environment has to carry it.
    Spawn {
        id: String,
        program: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: HashMap<String, String>,
        size: TermSize,
        /// Keep the shell running when its GUI goes away.
        persist: bool,
    },
    /// Take over a pane's I/O. Refused if someone else already has it.
    Attach {
        id: String,
    },
    Persist {
        id: String,
        value: bool,
    },
    /// End a pane's shell; unknown ids are fine (it already exited).
    Kill {
        id: String,
    },
    /// End every detached pane whose id is not in `keep`: a shell no saved
    /// workspace refers to any more would otherwise run forever.
    Prune {
        keep: Vec<String>,
    },
    /// End every shell and exit.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "res", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Spawned {
        pid: u32,
    },
    /// On Unix the PTY master fd rides along with this line (see `fdpass`),
    /// for `tcgetpgrp`-based cwd and foreground-process lookups.
    Attached {
        pid: u32,
    },
    Err {
        message: String,
    },
}

/// What flows on an attached connection. GUI → server: `Input`, `Resize`.
/// Server → GUI: `Replay` (buffered history, once, first), `Output`,
/// `Exited`. Either side closing the stream is a detach.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Input(Vec<u8>),
    Resize(TermSize),
    Replay(Vec<u8>),
    Output(Vec<u8>),
    Exited(i32),
}

const TAG_INPUT: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_REPLAY: u8 = 3;
const TAG_OUTPUT: u8 = 4;
const TAG_EXITED: u8 = 5;
const HEADER: usize = 5;

/// `[tag][u32 BE length][payload]`.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let (tag, payload): (u8, Vec<u8>) = match frame {
        Frame::Input(bytes) => (TAG_INPUT, bytes.clone()),
        Frame::Resize(size) => {
            let mut p = Vec::with_capacity(16);
            p.extend_from_slice(&(size.columns as u16).to_be_bytes());
            p.extend_from_slice(&(size.screen_lines as u16).to_be_bytes());
            p.extend_from_slice(&size.cell_width.to_be_bytes());
            p.extend_from_slice(&size.cell_height.to_be_bytes());
            p.extend_from_slice(&size.scale.to_be_bytes());
            (TAG_RESIZE, p)
        }
        Frame::Replay(bytes) => (TAG_REPLAY, bytes.clone()),
        Frame::Output(bytes) => (TAG_OUTPUT, bytes.clone()),
        Frame::Exited(status) => (TAG_EXITED, status.to_be_bytes().to_vec()),
    };
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.push(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

/// A frame header without copying the payload: for the common case of
/// forwarding a chunk of PTY output, the header goes first and the chunk
/// follows as its own write.
pub fn header(tag_of: &Frame, len: usize) -> [u8; HEADER] {
    let tag = match tag_of {
        Frame::Input(_) => TAG_INPUT,
        Frame::Resize(_) => TAG_RESIZE,
        Frame::Replay(_) => TAG_REPLAY,
        Frame::Output(_) => TAG_OUTPUT,
        Frame::Exited(_) => TAG_EXITED,
    };
    let len = (len as u32).to_be_bytes();
    [tag, len[0], len[1], len[2], len[3]]
}

/// Reassembles frames from a byte stream that arrives in arbitrary pieces.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next whole frame, if one has arrived. A bad tag is a protocol
    /// error: the stream is garbage from here on.
    pub fn next(&mut self) -> io::Result<Option<Frame>> {
        if self.buf.len() < HEADER {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if self.buf.len() < HEADER + len {
            return Ok(None);
        }
        let tag = self.buf[0];
        let payload: Vec<u8> = self.buf.drain(..HEADER + len).skip(HEADER).collect();
        let frame = match tag {
            TAG_INPUT => Frame::Input(payload),
            TAG_RESIZE => {
                if payload.len() != 16 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bad resize frame",
                    ));
                }
                let u16_at = |i: usize| u16::from_be_bytes([payload[i], payload[i + 1]]);
                let f32_at = |i: usize| {
                    f32::from_be_bytes([payload[i], payload[i + 1], payload[i + 2], payload[i + 3]])
                };
                Frame::Resize(TermSize {
                    columns: u16_at(0) as usize,
                    screen_lines: u16_at(2) as usize,
                    cell_width: f32_at(4),
                    cell_height: f32_at(8),
                    scale: f32_at(12),
                })
            }
            TAG_REPLAY => Frame::Replay(payload),
            TAG_OUTPUT => Frame::Output(payload),
            TAG_EXITED => {
                if payload.len() != 4 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "bad exit frame"));
                }
                Frame::Exited(i32::from_be_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]))
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame tag {other}"),
                ));
            }
        };
        Ok(Some(frame))
    }
}

/// One JSON line out.
pub fn write_json<T: Serialize>(stream: &mut impl Write, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

/// One JSON line in. `None` at a clean EOF.
pub fn read_json<T: for<'de> Deserialize<'de>>(reader: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Passing an open file descriptor to another process over a Unix socket
/// (`SCM_RIGHTS`). The receiver gets its own descriptor for the same open
/// file, which is how the GUI ends up holding the PTY master the server
/// reads: `tcgetpgrp` wants the fd itself, and nothing else about the PTY
/// is needed on the GUI side.
pub mod fdpass {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::net::UnixStream;

    /// Send `data` (at least one byte — macOS drops a control message that
    /// rides on an empty payload) together with a copy of `fd`.
    pub fn send_fd(stream: &UnixStream, fd: RawFd, data: &[u8]) -> io::Result<()> {
        assert!(!data.is_empty(), "SCM_RIGHTS needs a data byte to ride on");
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
        }
        let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Receive into `buf`; returns how many data bytes arrived and the
    /// descriptor, which is made close-on-exec (macOS has no
    /// `MSG_CMSG_CLOEXEC`, so there is a window in which a concurrent spawn
    /// could inherit it; the server's panes are spawned by the server, not
    /// here, so nothing of ours does).
    pub fn recv_fd(stream: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, OwnedFd)> {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut control = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        let got = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        if got < 0 {
            return Err(io::Error::last_os_error());
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::other("control message truncated"));
        }
        let fd = unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null()
                || (*cmsg).cmsg_level != libc::SOL_SOCKET
                || (*cmsg).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(io::Error::other("no descriptor in the message"));
            }
            std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd)
        };
        if fd < 0 {
            return Err(io::Error::other("bad descriptor received"));
        }
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
        Ok((got as usize, unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

// --- Client side -----------------------------------------------------------

/// Whether new panes should go through the server at all. Off when the
/// config says so; the window decides the rest (a `-e` window spawns
/// locally).
pub fn connect() -> io::Result<Stream> {
    Stream::connect(endpoint())
}

/// Connect, starting the server if nothing is listening. Gives up after a
/// second, and the caller then spawns in-process as before: a server that
/// won't start must never cost the user a shell.
pub fn connect_or_start() -> io::Result<Stream> {
    if let Ok(stream) = connect() {
        return Ok(stream);
    }
    start_server()?;
    let mut last = None;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        match connect() {
            Ok(stream) => return Ok(stream),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("session server did not start")))
}

/// `omnipty --server`, detached: its own session, no stdio, so it survives
/// this process ending however it ends.
fn start_server() -> io::Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

fn ask(stream: &mut Stream, request: &Request) -> io::Result<Response> {
    write_json(stream, request)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    read_json(&mut reader)?.ok_or_else(|| io::Error::other("session server hung up"))
}

fn expect_ok(response: Response) -> io::Result<()> {
    match response {
        Response::Ok => Ok(()),
        Response::Err { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
    }
}

/// A spawn over `stream` (one the caller got from `connect_or_start`);
/// the connection is dropped afterwards, the pane is attached separately.
pub fn spawn_on(mut stream: Stream, request: Request) -> io::Result<u32> {
    match ask(&mut stream, &request)? {
        Response::Spawned { pid } => Ok(pid),
        Response::Err { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
    }
}

/// What an attach hands back: the stream now carrying frames, any frame
/// bytes that arrived with the reply, the shell's pid, and (Unix) the PTY
/// master.
pub struct Attached {
    pub stream: Stream,
    pub leftover: Vec<u8>,
    pub pid: u32,
    pub master: std::os::fd::OwnedFd,
}

/// Attach to an existing pane over `stream`. `Err` when the pane is gone —
/// the caller starts a fresh shell instead.
pub fn attach_on(mut stream: Stream, id: &str) -> io::Result<Attached> {
    write_json(&mut stream, &Request::Attach { id: id.to_string() })?;
    // The reply line and the descriptor arrive in one message; read it raw
    // rather than through a BufReader, which would swallow frame bytes that
    // may already follow it in the same read.
    let mut buf = vec![0u8; 64 * 1024];
    let (n, master) = fdpass::recv_fd(&stream, &mut buf)?;
    let line_end = buf[..n]
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| io::Error::other("attach reply not terminated"))?;
    let response: Response = serde_json::from_slice(&buf[..line_end])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let leftover = buf[line_end + 1..n].to_vec();
    match response {
        Response::Attached { pid } => Ok(Attached {
            stream,
            leftover,
            pid,
            master,
        }),
        Response::Err { message } => Err(io::Error::other(message)),
        other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
    }
}

/// A request whose only answer is `Ok`.
pub fn request_on(mut stream: Stream, request: Request) -> io::Result<()> {
    expect_ok(ask(&mut stream, &request)?)
}

pub fn persist(id: &str, value: bool) -> io::Result<()> {
    request_on(
        connect()?,
        Request::Persist {
            id: id.to_string(),
            value,
        },
    )
}

pub fn kill(id: &str) -> io::Result<()> {
    request_on(connect()?, Request::Kill { id: id.to_string() })
}

/// End every shell the server holds and the server itself. Nothing to do
/// when no server is running.
pub fn shutdown() -> io::Result<()> {
    if_running(|stream| request_on(stream, Request::Shutdown))
}

/// End every detached shell not in `keep` (see `Request::Prune`). Nothing to
/// do when no server is running; never starts one.
pub fn prune(keep: Vec<String>) -> io::Result<()> {
    if_running(|stream| request_on(stream, Request::Prune { keep }))
}

fn if_running(f: impl FnOnce(Stream) -> io::Result<()>) -> io::Result<()> {
    match connect() {
        Ok(stream) => f(stream),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> TermSize {
        TermSize {
            columns: 120,
            screen_lines: 40,
            cell_width: 8.4,
            cell_height: 16.8,
            scale: 2.0,
        }
    }

    /// Every frame kind survives encoding, and the decoder reassembles
    /// frames from one byte at a time.
    #[test]
    fn frames_round_trip_through_a_fragmented_stream() {
        let frames = vec![
            Frame::Input(b"ls\r".to_vec()),
            Frame::Resize(size()),
            Frame::Replay(vec![0u8; 70_000]),
            Frame::Output(b"hello".to_vec()),
            Frame::Exited(-3),
            Frame::Input(Vec::new()),
        ];
        let wire: Vec<u8> = frames.iter().flat_map(encode).collect();
        let mut decoder = Decoder::default();
        let mut out = Vec::new();
        for byte in wire {
            decoder.push(&[byte]);
            while let Some(frame) = decoder.next().unwrap() {
                out.push(frame);
            }
        }
        assert_eq!(out, frames);
        // A header built separately matches the encoder's.
        let full = encode(&Frame::Output(b"abc".to_vec()));
        assert_eq!(&full[..HEADER], &header(&Frame::Output(Vec::new()), 3));
    }

    #[test]
    fn a_bad_tag_is_an_error() {
        let mut decoder = Decoder::default();
        decoder.push(&[99, 0, 0, 0, 0]);
        assert!(decoder.next().is_err());
    }

    #[test]
    fn requests_are_one_json_line() {
        let req = Request::Spawn {
            id: "1-2-3".into(),
            program: "/bin/sh".into(),
            args: vec!["-l".into()],
            cwd: Some("/tmp".into()),
            env: HashMap::from([("A".to_string(), "b".to_string())]),
            size: size(),
            persist: true,
        };
        let mut wire = Vec::new();
        write_json(&mut wire, &req).unwrap();
        assert_eq!(wire.iter().filter(|&&b| b == b'\n').count(), 1);
        let back: Request = read_json(&mut &wire[..]).unwrap().unwrap();
        assert_eq!(back, req);
        assert!(read_json::<Request>(&mut &b""[..]).unwrap().is_none());
    }

    /// A descriptor crosses a socket and is a working handle on the other
    /// side: what the server does with the PTY master at attach.
    #[test]
    fn a_descriptor_crosses_a_socketpair() {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        let (a, b) = Stream::pair().unwrap();
        let mut pipe = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let (read_end, write_end) = unsafe {
            (
                std::fs::File::from(std::os::fd::OwnedFd::from_raw_fd(pipe[0])),
                std::fs::File::from(std::os::fd::OwnedFd::from_raw_fd(pipe[1])),
            )
        };
        fdpass::send_fd(&a, write_end.as_raw_fd(), b"hello\n").unwrap();
        drop(write_end);
        let mut buf = [0u8; 64];
        let (n, fd) = fdpass::recv_fd(&b, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello\n");
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "received fd must be close-on-exec"
        );
        let mut received = std::fs::File::from(fd);
        received.write_all(b"through").unwrap();
        drop(received);
        let mut text = String::new();
        let mut read_end = read_end;
        read_end.read_to_string(&mut text).unwrap();
        assert_eq!(text, "through");
    }
}
