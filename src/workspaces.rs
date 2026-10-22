//! Workspace persistence: layout, directories, and startup commands,
//! restored with fresh shells.
//!
//! A workspace marked `persist` survives app restarts the way an iTerm2/kitty
//! session does — the shape (tabs, splits) and each pane's working directory
//! come back, with new shells spawned in those directories. Running programs
//! cannot survive a full quit (tmux gets away with it only because its server
//! never exits), so a pane can instead declare a *startup command* that is
//! re-run on restore — the tmuxinator model.
//!
//! On disk: `{ "version": 3, "workspaces": [...] }`. Every leaf is a
//! `SavedPane` object. Version 2 (v0.4.0) wrote leaves as bare directory
//! strings; version 1 (v0.3.x) was a bare array with no split ratios.
//! `load` reads all three, and never lets a *format* change reach the
//! `.corrupt` path — that is reserved for files that are genuinely broken.
//!
//! **This file is executable content**: a startup command is a shell
//! command run at launch. It is written 0600 for that reason.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::panes::Node;
pub use crate::startup::OnExit;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SavedWorkspace {
    pub name: String,
    pub active_tab: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SavedTab {
    /// The split tree with each leaf holding that pane's saved state.
    pub layout: Node<SavedPane>,
    /// Index into `layout.leaves()` of the focused pane.
    pub active: usize,
    /// A user-set tab name, overriding the automatic title. Absent in
    /// files written before v0.4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One pane, as saved: where its shell starts and what, if anything, it
/// runs first. Plain data so a later project-file source (`.omnipty.toml`)
/// can populate it too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedPane {
    pub cwd: PathBuf,
    /// Run on restore. None means "just a shell".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// What to do when the command exits.
    #[serde(default, skip_serializing_if = "is_default_on_exit")]
    pub on_exit: OnExit,
}

fn is_default_on_exit(on_exit: &OnExit) -> bool {
    *on_exit == OnExit::default()
}

impl SavedPane {
    pub fn shell(cwd: PathBuf) -> Self {
        Self {
            cwd,
            command: None,
            on_exit: OnExit::default(),
        }
    }

    /// The startup command, as the app holds it.
    pub fn startup(&self) -> Option<crate::startup::StartupCommand> {
        self.command
            .as_ref()
            .filter(|c| !c.trim().is_empty())
            .map(|c| crate::startup::StartupCommand {
                command: c.clone(),
                on_exit: self.on_exit,
            })
    }
}

/// A leaf as any version ever wrote it. `untagged` tries variants top to
/// bottom: a `PathBuf` deserialises from a string and refuses an object, so
/// `Legacy` must stay first.
#[derive(Deserialize)]
#[serde(untagged)]
enum SavedPaneCompat {
    /// v1 and v2: a bare path string.
    Legacy(PathBuf),
    /// v3+: the full record.
    Current(SavedPane),
}

impl From<SavedPaneCompat> for SavedPane {
    fn from(compat: SavedPaneCompat) -> Self {
        match compat {
            SavedPaneCompat::Legacy(cwd) => SavedPane::shell(cwd),
            SavedPaneCompat::Current(pane) => pane,
        }
    }
}

/// The current on-disk format version. Bump when the shape changes in a way
/// `parse` needs to branch on.
pub const FORMAT_VERSION: u32 = 3;

#[derive(Serialize)]
struct SavedFile<'a> {
    version: u32,
    workspaces: &'a [SavedWorkspace],
}

// The read side is separate from the write side so the compat enum never
// leaks into what we serialise.
#[derive(Deserialize)]
struct RawFile {
    version: u32,
    workspaces: Vec<RawWorkspace>,
}

#[derive(Deserialize)]
struct RawWorkspace {
    name: String,
    active_tab: usize,
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
struct RawTab {
    layout: Node<SavedPaneCompat>,
    active: usize,
    #[serde(default)]
    title: Option<String>,
}

impl From<RawWorkspace> for SavedWorkspace {
    fn from(raw: RawWorkspace) -> Self {
        SavedWorkspace {
            name: raw.name,
            active_tab: raw.active_tab,
            tabs: raw
                .tabs
                .into_iter()
                .map(|t| {
                    let mut layout = t.layout.map(&mut |leaf| match leaf {
                        SavedPaneCompat::Legacy(cwd) => SavedPane::shell(cwd.clone()),
                        SavedPaneCompat::Current(pane) => pane.clone(),
                    });
                    layout.normalise();
                    SavedTab {
                        layout,
                        active: t.active,
                        title: t.title,
                    }
                })
                .collect(),
        }
    }
}

// `Node::map` needs `T: PartialEq + Clone`; the compat enum is only ever
// mapped away, so these are structural and never called.
impl Clone for SavedPaneCompat {
    fn clone(&self) -> Self {
        match self {
            SavedPaneCompat::Legacy(p) => SavedPaneCompat::Legacy(p.clone()),
            SavedPaneCompat::Current(p) => SavedPaneCompat::Current(p.clone()),
        }
    }
}

impl PartialEq for SavedPaneCompat {
    fn eq(&self, other: &Self) -> bool {
        SavedPane::from(self.clone()) == SavedPane::from(other.clone())
    }
}

pub fn state_path() -> Option<PathBuf> {
    Some(crate::paths::cache_dir().join("workspaces.json"))
}

/// Read any format version we've ever written. Returns the version the file
/// was written in alongside the workspaces. Split ratios are normalised on
/// the way in so a v1 file (no ratios) or a hand-edited one (nonsense
/// ratios) yields even splits instead of a broken layout.
fn parse(text: &str) -> Result<(u32, Vec<SavedWorkspace>), serde_json::Error> {
    let (version, raw) = match serde_json::from_str::<RawFile>(text) {
        Ok(file) => (file.version, file.workspaces),
        // v1: a bare array.
        Err(versioned_err) => {
            let raw = serde_json::from_str::<Vec<RawWorkspace>>(text).map_err(|_| versioned_err)?;
            (1, raw)
        }
    };
    Ok((version, raw.into_iter().map(SavedWorkspace::from).collect()))
}

/// Before the first write in a newer format, keep a copy of the old file
/// as `workspaces.json.v<N>.bak`. Written exactly once: a later save never
/// touches it, so "I lost my workspaces on upgrade" is a one-line fix.
fn backup_older_format(path: &Path, version: u32) -> Option<PathBuf> {
    if version >= FORMAT_VERSION {
        return None;
    }
    let backup = path.with_extension(format!("json.v{version}.bak"));
    if backup.exists() {
        return None;
    }
    std::fs::copy(path, &backup).ok()?;
    restrict_permissions(&backup);
    Some(backup)
}

/// Owner-only: the file holds commands that run at launch.
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

pub fn load() -> Vec<SavedWorkspace> {
    let Some(path) = state_path() else {
        return Vec::new();
    };
    load_from(&path)
}

pub fn load_from(path: &Path) -> Vec<SavedWorkspace> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let saved: Vec<SavedWorkspace> = match parse(&text) {
        Ok((version, saved)) => {
            if backup_older_format(path, version).is_some() {
                crate::telemetry::breadcrumb(
                    "workspaces",
                    format!("upgrading format v{version} → v{FORMAT_VERSION}"),
                );
            }
            saved
        }
        Err(e) => {
            // Never the text: it holds commands and paths. Where it broke is
            // enough to build a fixture from. Not `e.to_string()` either:
            // serde_json quotes values from the file in its messages.
            crate::telemetry::report(
                "workspaces.json didn't parse",
                format_args!(
                    "{:?} error at line {} column {} (this build writes v{FORMAT_VERSION})",
                    e.classify(),
                    e.line(),
                    e.column()
                ),
            );
            // Never silently discard a file we failed to parse: the next
            // auto-save would see "no pinned workspaces" and delete it,
            // turning one bad write into permanent data loss. Move it aside
            // so it stays recoverable.
            let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
            return Vec::new();
        }
    };
    // Drop anything structurally hollow so a hostile edit can't wedge startup.
    saved
        .into_iter()
        .filter(|ws| !ws.tabs.is_empty() && ws.tabs.iter().all(|t| t.layout.len() > 0))
        .collect()
}

pub fn save(workspaces: &[SavedWorkspace]) {
    let Some(path) = state_path() else { return };
    save_to(&path, workspaces);
}

pub fn save_to(path: &Path, workspaces: &[SavedWorkspace]) {
    if workspaces.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = SavedFile {
        version: FORMAT_VERSION,
        workspaces,
    };
    let Ok(json) = serde_json::to_string_pretty(&file) else {
        return;
    };
    // Write-then-rename so a crash or kill mid-write can never leave a
    // truncated file behind (fs::write truncates before writing). The temp
    // file is created owner-only so the commands inside are never readable
    // by anyone else, even for an instant.
    let tmp = path.with_extension("json.tmp");
    let written = {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| f.write_all(json.as_bytes()))
    };
    if written.is_ok() {
        restrict_permissions(&tmp);
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::{Axis, Node};

    fn has_startup_commands(ws: &SavedWorkspace) -> bool {
        ws.tabs
            .iter()
            .any(|t| t.layout.leaves().iter().any(|p| p.startup().is_some()))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("omnipty-ws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> Vec<SavedWorkspace> {
        vec![SavedWorkspace {
            name: "workspace 1".into(),
            active_tab: 1,
            tabs: vec![
                SavedTab {
                    layout: Node::Leaf(SavedPane::shell(PathBuf::from("/tmp"))),
                    active: 0,
                    title: Some("build".into()),
                },
                SavedTab {
                    layout: Node::Split {
                        axis: Axis::Horizontal,
                        children: vec![
                            Node::Leaf(SavedPane {
                                cwd: PathBuf::from("/Users/x/dev"),
                                command: Some("cargo watch -x check".into()),
                                on_exit: OnExit::Restart,
                            }),
                            Node::Split {
                                axis: Axis::Vertical,
                                children: vec![
                                    Node::Leaf(SavedPane {
                                        cwd: PathBuf::from("/Users/x/dev/a"),
                                        command: Some("npm run dev".into()),
                                        on_exit: OnExit::Close,
                                    }),
                                    Node::Leaf(SavedPane::shell(PathBuf::from("/Users/x/dev/b"))),
                                ],
                                ratios: vec![0.25, 0.75],
                            },
                        ],
                        ratios: vec![0.6, 0.4],
                    },
                    active: 2,
                    title: None,
                },
            ],
        }]
    }

    #[test]
    fn v3_round_trips_through_json() {
        let ws = sample();
        let file = SavedFile {
            version: FORMAT_VERSION,
            workspaces: &ws,
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"version\":3"));
        let (version, back) = parse(&json).unwrap();
        assert_eq!(version, 3);
        assert_eq!(
            ws, back,
            "ratios, titles, commands and on_exit must survive the round trip"
        );
        assert!(
            !json.contains("\"title\":null"),
            "unset titles stay out of the file"
        );
        // A plain shell pane is still compact: no command, no on_exit noise.
        assert!(json.contains(r#"{"leaf":{"cwd":"/tmp"}}"#), "{json}");
        assert!(json.contains(r#""on_exit":"restart""#));
        assert!(has_startup_commands(&ws[0]));
    }

    /// A file written by v0.3.2 — before split ratios, a version field, or
    /// pane records. `load()` renames anything it can't parse to `.corrupt`,
    /// so a regression here silently wipes every pinned workspace on
    /// upgrade. Never regenerate this fixture from current code.
    #[test]
    fn v0_3_2_file_still_loads() {
        let text = include_str!("../tests/fixtures/workspaces-v0.3.2.json");
        let (version, saved) = parse(text).expect("v0.3.2 workspaces.json must parse");
        assert_eq!(version, 1);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].name, "workspace 1");
        assert_eq!(saved[0].active_tab, 1);
        assert_eq!(saved[0].tabs.len(), 2);
        let layout = &saved[0].tabs[1].layout;
        assert_eq!(layout.len(), 4);
        assert_eq!(
            layout
                .leaves()
                .iter()
                .map(|p| p.cwd.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/Users/x/dev"),
                PathBuf::from("/Users/x/dev/a"),
                PathBuf::from("/Users/x/dev/b"),
                PathBuf::from("/Users/x/dev/c"),
            ]
        );
        assert!(
            layout
                .leaves()
                .iter()
                .all(|p| p.command.is_none() && p.on_exit == OnExit::Shell)
        );
        assert_eq!(
            saved[1].tabs[0].layout,
            Node::Leaf(SavedPane::shell(PathBuf::from("/Users/x/notes")))
        );
        assert!(saved[0].tabs.iter().all(|t| t.title.is_none()));
        assert!(!has_startup_commands(&saved[0]));
        // No ratios in the file → even splits after normalisation.
        match layout {
            Node::Split {
                ratios, children, ..
            } => {
                assert_eq!(ratios, &vec![0.5, 0.5]);
                match &children[1] {
                    Node::Split { ratios, .. } => {
                        assert_eq!(ratios.len(), 3);
                        assert!((ratios[0] - 1.0 / 3.0).abs() < 1e-5);
                    }
                    other => panic!("expected a split, got {other:?}"),
                }
            }
            other => panic!("expected a split, got {other:?}"),
        }
    }

    /// A file written by v0.4.0 — versioned, with ratios and tab titles, but
    /// leaves still bare directory strings. Same rule: never regenerate it.
    #[test]
    fn v0_4_0_file_still_loads() {
        let text = include_str!("../tests/fixtures/workspaces-v0.4.0.json");
        let (version, saved) = parse(text).expect("v0.4.0 workspaces.json must parse");
        assert_eq!(version, 2);
        assert_eq!(saved.len(), 1);
        let ws = &saved[0];
        assert_eq!(ws.tabs[0].title.as_deref(), Some("build"));
        assert_eq!(ws.tabs[1].title, None);
        let layout = &ws.tabs[1].layout;
        assert_eq!(
            layout.leaves(),
            vec![
                SavedPane::shell(PathBuf::from("/Users/x/dev")),
                SavedPane::shell(PathBuf::from("/Users/x/dev/a")),
                SavedPane::shell(PathBuf::from("/Users/x/dev/b")),
            ]
        );
        match layout {
            Node::Split { ratios, .. } => assert_eq!(ratios, &vec![0.6, 0.4]),
            other => panic!("expected a split, got {other:?}"),
        }
    }

    #[test]
    fn hand_edited_ratios_are_repaired_not_rejected() {
        let json = r#"{"version":3,"workspaces":[{"name":"w","active_tab":0,"tabs":[{"layout":{"split":{"axis":"horizontal","children":[{"leaf":{"cwd":"/a"}},{"leaf":"/b"}],"ratios":[9.0]}},"active":0}]}]}"#;
        let (_, saved) = parse(json).unwrap();
        match &saved[0].tabs[0].layout {
            Node::Split {
                ratios, children, ..
            } => {
                assert_eq!(ratios, &vec![0.5, 0.5]);
                // Mixed leaf shapes in one file are fine too.
                assert_eq!(
                    children[1],
                    Node::Leaf(SavedPane::shell(PathBuf::from("/b")))
                );
            }
            other => panic!("expected a split, got {other:?}"),
        }
    }

    #[test]
    fn unknown_on_exit_is_an_error_not_a_default() {
        let json = r#"{"version":3,"workspaces":[{"name":"w","active_tab":0,"tabs":[{"layout":{"leaf":{"cwd":"/a","command":"x","on_exit":"explode"}},"active":0}]}]}"#;
        assert!(parse(json).is_err());
    }

    #[test]
    fn corrupt_file_is_moved_aside_and_older_formats_never_are() {
        let dir = temp_dir("corrupt");
        let path = dir.join("workspaces.json");
        // Simulates a truncated write: load must move the file aside rather
        // than treating it as empty (which a later save would turn into
        // silent deletion of the user's pinned workspaces).
        std::fs::write(&path, "[{\"name\": \"trunc").unwrap();
        assert!(load_from(&path).is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.corrupt").exists());

        // A v1 file is a format we know, not corruption.
        std::fs::write(
            &path,
            include_str!("../tests/fixtures/workspaces-v0.3.2.json"),
        )
        .unwrap();
        let saved = load_from(&path);
        assert_eq!(saved.len(), 2);
        assert!(
            path.exists(),
            "an older format must never be moved to .corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_is_written_exactly_once() {
        let dir = temp_dir("backup");
        let path = dir.join("workspaces.json");
        let v2 = include_str!("../tests/fixtures/workspaces-v0.4.0.json");
        std::fs::write(&path, v2).unwrap();

        let saved = load_from(&path);
        let backup = path.with_extension("json.v2.bak");
        assert!(backup.exists(), "first load of an older format backs it up");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), v2);

        // Save in the new format, then load again: the backup is untouched.
        save_to(&path, &saved);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("\"version\": 3"));
        let again = load_from(&path);
        assert_eq!(again, saved);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            v2,
            "a later save must not overwrite the backup"
        );
        assert!(
            !path.with_extension("json.v3.bak").exists(),
            "the current format is never backed up"
        );

        // Even a second v2 file (say, restored by hand) doesn't clobber it.
        std::fs::write(&path, v2.replace("build", "other")).unwrap();
        load_from(&path);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), v2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let path = dir.join("workspaces.json");
        save_to(&path, &sample());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "workspaces.json holds commands and must be private"
        );
        // Saving with nothing pinned removes the file.
        save_to(&path, &[]);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_converts_leaves_both_ways() {
        let ids: Node<u64> = Node::split_even(Axis::Horizontal, vec![Node::Leaf(1), Node::Leaf(2)]);
        let panes = ids.map(&mut |id| SavedPane {
            cwd: PathBuf::from(format!("/dir{id}")),
            command: (*id == 2).then(|| "make".to_string()),
            on_exit: OnExit::Close,
        });
        let leaves = panes.leaves();
        assert_eq!(leaves[0].cwd, PathBuf::from("/dir1"));
        assert_eq!(leaves[1].startup().unwrap().command, "make");
        assert_eq!(leaves[0].startup(), None);
        let mut counter = 10u64;
        let back = panes.map(&mut |_| {
            counter += 1;
            counter
        });
        assert_eq!(back.leaves(), vec![11, 12]);
    }

    #[test]
    fn blank_commands_count_as_no_command() {
        let pane = SavedPane {
            cwd: "/".into(),
            command: Some("   ".into()),
            on_exit: OnExit::Restart,
        };
        assert_eq!(pane.startup(), None);
    }
}
