//! Decoders for the OSC sequences OmniPTY consumes itself: the OSC 133 prompt
//! markers the shell integration emits, OSC 7 (cwd), and OSC 9 / 777
//! (desktop notifications).
//!
//! `alacritty_terminal` drops unknown OSCs inside its parser, so `scan.rs`
//! finds them on the raw byte stream in the PTY thread and hands each
//! payload here.

use std::path::PathBuf;

/// What a marker means, with any payload decoded.
#[derive(Debug, Clone, PartialEq)]
pub enum MarkerKind {
    /// OSC 133;A — the shell is about to draw a prompt.
    PromptStart,
    /// OSC 133;B — the prompt is drawn; what follows is user input.
    InputStart,
    /// OSC 133;C — a command is starting. OmniPTY's integration adds the typed
    /// line as `cmdline=…`.
    CommandStart { cmdline: Option<String> },
    /// OSC 133;D;<exit> — the command finished. `None` when the exit code is
    /// missing or unparseable.
    CommandEnd { exit: Option<i32> },
    /// OSC 7 — the shell's working directory, local hosts only.
    Cwd(PathBuf),
    /// OSC 9 / OSC 777;notify — a program asked for a desktop notification.
    Notify { title: Option<String>, body: String },
}

/// A marker with where it landed: the cursor's absolute row
/// (history + screen line) and column at the moment it was parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub kind: MarkerKind,
    pub row: usize,
    pub column: usize,
    pub alt_screen: bool,
}

/// Decode an OSC payload (everything between `ESC ]` and the terminator)
/// into a marker, or `None` for sequences that aren't ours.
pub fn parse_payload(payload: &[u8]) -> Option<MarkerKind> {
    let text = std::str::from_utf8(payload).ok()?;
    let (code, rest) = match text.split_once(';') {
        Some((code, rest)) => (code, rest),
        None => (text, ""),
    };
    match code {
        "133" => parse_133(rest),
        "7" => parse_cwd(rest).map(MarkerKind::Cwd),
        "9" => {
            // ConEmu/Windows Terminal use `9;4;…` for progress bars; only a
            // plain message is a notification.
            if rest.is_empty() || rest.starts_with("4;") {
                return None;
            }
            Some(MarkerKind::Notify {
                title: None,
                body: rest.to_string(),
            })
        }
        "777" => {
            let mut parts = rest.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            let title = parts.next().unwrap_or("").to_string();
            let body = parts.next().unwrap_or("").to_string();
            if title.is_empty() && body.is_empty() {
                return None;
            }
            Some(MarkerKind::Notify {
                title: (!title.is_empty()).then_some(title),
                body,
            })
        }
        _ => None,
    }
}

fn parse_133(rest: &str) -> Option<MarkerKind> {
    let mut parts = rest.split(';');
    let kind = parts.next()?;
    match kind {
        "A" => Some(MarkerKind::PromptStart),
        "B" => Some(MarkerKind::InputStart),
        "C" => {
            // Our integration appends `cmdline=<text>`; the text may itself
            // contain semicolons, so take everything after the key.
            let cmdline = rest
                .strip_prefix("C")
                .and_then(|r| r.split_once("cmdline="))
                .map(|(_, line)| line.trim().to_string())
                .filter(|s| !s.is_empty());
            Some(MarkerKind::CommandStart { cmdline })
        }
        "D" => {
            let exit = parts.next().and_then(|s| s.trim().parse::<i32>().ok());
            Some(MarkerKind::CommandEnd { exit })
        }
        _ => None,
    }
}

/// `file://host/path` → the path, for hosts that mean this machine. A
/// remote shell's cwd (over ssh) is real but not somewhere the file tree
/// can go, so it is ignored.
pub(crate) fn parse_cwd(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let (host, path) = {
        let ix = rest.find('/')?;
        (&rest[..ix], &rest[ix..])
    };
    if !host_is_local(host) {
        return None;
    }
    let decoded = percent_decode(path);
    (!decoded.is_empty()).then(|| PathBuf::from(decoded))
}

fn host_is_local(host: &str) -> bool {
    if host.is_empty() || host == "localhost" {
        return true;
    }
    let Ok(mine) = hostname() else { return false };
    let short = |h: &str| h.split('.').next().unwrap_or(h).to_ascii_lowercase();
    short(host) == short(&mine)
}

fn hostname() -> Result<String, ()> {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return Err(());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..end].to_vec()).map_err(|_| ())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_accepts_local_hosts_and_decodes_percent_escapes() {
        assert_eq!(
            parse_payload(b"7;file:///Users/x/a%20b"),
            Some(MarkerKind::Cwd(PathBuf::from("/Users/x/a b")))
        );
        assert_eq!(
            parse_payload(b"7;file://localhost/tmp"),
            Some(MarkerKind::Cwd(PathBuf::from("/tmp")))
        );
        let mine = hostname().unwrap();
        assert_eq!(
            parse_payload(format!("7;file://{mine}/tmp").as_bytes()),
            Some(MarkerKind::Cwd(PathBuf::from("/tmp")))
        );
        assert_eq!(parse_payload(b"7;file://build-box.example.com/srv"), None);
        assert_eq!(parse_payload(b"7;/no/scheme"), None);
    }

    #[test]
    fn notifications_from_osc_9_and_777() {
        assert_eq!(
            parse_payload(b"9;Build finished"),
            Some(MarkerKind::Notify {
                title: None,
                body: "Build finished".into()
            })
        );
        assert_eq!(
            parse_payload(b"9;4;1;50"),
            None,
            "progress reports are not notifications"
        );
        assert_eq!(
            parse_payload(b"777;notify;Deploy;All green"),
            Some(MarkerKind::Notify {
                title: Some("Deploy".into()),
                body: "All green".into()
            })
        );
        assert_eq!(parse_payload(b"777;other;x;y"), None);
    }
}
