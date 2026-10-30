//! Go To: the workspace › tab › pane tree behind `cmd-shift-g`.
//!
//! Pure functions over a plain snapshot, no GPUI, so the ranking and the
//! "which row starts selected" rule are unit-testable. The app builds the
//! snapshot, owns the overlay's state, and renders the rows.

use std::path::{Path, PathBuf};

use crate::app::PaneId;
use crate::palette::Matcher;

#[derive(Clone)]
pub struct WorkspaceSnap {
    pub name: String,
    /// The workspace on screen.
    pub active: bool,
    pub tabs: Vec<TabSnap>,
}

#[derive(Clone)]
pub struct TabSnap {
    /// What the tab bar shows: the user's name for it, or the automatic one.
    pub title: String,
    /// Its workspace's active tab.
    pub active: bool,
    pub panes: Vec<PaneSnap>,
}

#[derive(Clone)]
pub struct PaneSnap {
    pub id: PaneId,
    pub cwd: Option<PathBuf>,
    /// `ForegroundProcess::label()` when it isn't a shell.
    pub program: Option<String>,
    pub osc_title: String,
    pub last_command: Option<String>,
    /// Its tab's focused pane.
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Workspace(usize),
    Tab(usize, usize),
    Pane(usize, usize, PaneId),
}

impl Target {
    fn workspace(self) -> usize {
        match self {
            Target::Workspace(w) | Target::Tab(w, _) | Target::Pane(w, _, _) => w,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub target: Target,
    /// The pane ⏎ lands on: a workspace's active tab's active pane, a
    /// tab's active pane, the pane itself. Pane ids outlive the indices in
    /// `target`, which can shift while the overlay is open.
    pub pane: Option<PaneId>,
    /// 0, 1, 2 — for the indent.
    pub depth: u8,
    pub label: String,
    /// Right-hand dim text: a pane's program, a tab's pane count.
    pub detail: Option<String>,
    /// Char positions into `label`.
    pub highlights: Vec<usize>,
    /// `None`: shown only because a relative matched.
    pub score: Option<i32>,
    /// On the path to the focused pane: the active workspace, its active
    /// tab, and that tab's focused pane. The "where am I" marks.
    pub current: bool,
}

#[derive(Debug, Default)]
pub struct Rows {
    pub rows: Vec<Row>,
    /// Index of the row ⏎ should hit with no further input.
    pub selected: usize,
    pub matched_panes: usize,
    pub total_panes: usize,
}

/// Best score across `candidates`, each paying its penalty, with the index
/// of the field that won and its match positions.
fn score_fields(
    matcher: &mut Matcher,
    candidates: &[(&str, i32)],
) -> Option<(i32, usize, Vec<usize>)> {
    let mut best: Option<(i32, usize, Vec<usize>)> = None;
    for (ix, (text, penalty)) in candidates.iter().enumerate() {
        let Some(m) = matcher.score(text) else {
            continue;
        };
        let score = m.score - penalty;
        if best.as_ref().is_none_or(|b| score > b.0) {
            best = Some((score, ix, m.positions));
        }
    }
    best
}

/// Every workspace, tab, and pane that matches `query`, or has a relative
/// that does, in display order: the tree never reorders by score.
pub fn build_rows(query: &str, snapshot: &[WorkspaceSnap]) -> Rows {
    build_rows_with_home(query, snapshot, crate::app::home_dir().as_deref())
}

fn build_rows_with_home(query: &str, snapshot: &[WorkspaceSnap], home: Option<&Path>) -> Rows {
    let query = query.trim();
    let mut matcher = Matcher::new(query);
    let mut out = Rows::default();

    for (wix, ws) in snapshot.iter().enumerate() {
        let ws_match = score_fields(&mut matcher, &[(&ws.name, 0)]);
        let mut ws_rows: Vec<Row> = Vec::new();
        let mut ws_dest = None;

        for (tix, tab) in ws.tabs.iter().enumerate() {
            out.total_panes += tab.panes.len();
            let tab_match = score_fields(&mut matcher, &[(&tab.title, 0)]);
            let tab_dest = tab.panes.iter().find(|p| p.active).or(tab.panes.first());
            if tab.active || ws_dest.is_none() {
                ws_dest = tab_dest.map(|p| p.id);
            }
            let tab_kept = ws_match.is_some() || tab_match.is_some();

            let mut pane_rows: Vec<Row> = Vec::new();
            for pane in &tab.panes {
                let label = match &pane.cwd {
                    Some(cwd) => crate::app::pretty_path(cwd, home),
                    None if !pane.osc_title.is_empty() => pane.osc_title.clone(),
                    None => "shell".into(),
                };
                let base = pane.cwd.as_deref().map(|c| crate::app::dir_name(c, home));
                // Fields best-first; later ones pay a small penalty so a
                // basename hit outranks the same characters deep in a path.
                let mut fields: Vec<(&str, i32)> = Vec::new();
                if let Some(b) = &base {
                    fields.push((b, 0));
                }
                fields.push((&label, 3));
                if let Some(p) = &pane.program {
                    fields.push((p, 1));
                }
                if !pane.osc_title.is_empty() {
                    fields.push((&pane.osc_title, 4));
                }
                if let Some(c) = &pane.last_command {
                    fields.push((c, 4));
                }
                let pane_match = score_fields(&mut matcher, &fields).map(|(score, ix, hl)| {
                    // Only a hit on the displayed text is highlighted: the
                    // basename is the label's tail, the full path is the
                    // label itself, and the rest are hidden fields.
                    let highlights = match (ix, &base) {
                        (0, Some(b)) => match label.rfind(b.as_str()) {
                            Some(at) => {
                                let offset = label[..at].chars().count();
                                hl.into_iter().map(|p| p + offset).collect()
                            }
                            None => Vec::new(),
                        },
                        (1, Some(_)) | (0, None) => hl,
                        _ => Vec::new(),
                    };
                    (score, highlights)
                });
                if tab_kept || pane_match.is_some() {
                    let (score, highlights) = match pane_match {
                        Some((s, h)) => (Some(s), h),
                        None => (None, Vec::new()),
                    };
                    pane_rows.push(Row {
                        target: Target::Pane(wix, tix, pane.id),
                        pane: Some(pane.id),
                        depth: 2,
                        label,
                        detail: pane.program.clone().or_else(|| pane.last_command.clone()),
                        highlights,
                        score,
                        current: ws.active && tab.active && pane.active,
                    });
                }
            }

            if tab_kept || !pane_rows.is_empty() {
                let (score, highlights) = match tab_match {
                    Some((s, _, h)) => (Some(s), h),
                    None => (None, Vec::new()),
                };
                ws_rows.push(Row {
                    target: Target::Tab(wix, tix),
                    pane: tab_dest.map(|p| p.id),
                    depth: 1,
                    label: tab.title.clone(),
                    detail: (tab.panes.len() > 1).then(|| format!("{} panes", tab.panes.len())),
                    highlights,
                    score,
                    current: ws.active && tab.active,
                });
                ws_rows.append(&mut pane_rows);
            }
        }

        if ws_match.is_none() && ws_rows.is_empty() {
            continue;
        }
        let (score, highlights) = match ws_match {
            Some((s, _, h)) => (Some(s), h),
            None => (None, Vec::new()),
        };
        out.rows.push(Row {
            target: Target::Workspace(wix),
            pane: ws_dest,
            depth: 0,
            label: ws.name.clone(),
            detail: None,
            highlights,
            score,
            current: ws.active,
        });
        out.rows.append(&mut ws_rows);
    }

    out.matched_panes = out
        .rows
        .iter()
        .filter(|r| matches!(r.target, Target::Pane(..)) && r.score.is_some())
        .count();
    // With no query every score is 0 and the current pane is the answer.
    // Otherwise the highest score wins; ties go to the deeper row (a pane
    // is a more precise destination than its tab), then the current
    // workspace, then the earlier row.
    out.selected = if query.is_empty() {
        // The last marked row is the deepest: the pane itself.
        out.rows.iter().rposition(|r| r.current).unwrap_or(0)
    } else {
        out.rows
            .iter()
            .enumerate()
            .filter_map(|(ix, r)| {
                let here = snapshot[r.target.workspace()].active;
                Some((r.score?, r.depth, here, std::cmp::Reverse(ix)))
            })
            .max()
            .map(|(_, _, _, ix)| ix.0)
            .unwrap_or(0)
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: PaneId, cwd: &str) -> PaneSnap {
        PaneSnap {
            id,
            cwd: Some(PathBuf::from(cwd)),
            program: None,
            osc_title: String::new(),
            last_command: None,
            active: false,
        }
    }

    fn tab(title: &str, panes: Vec<PaneSnap>) -> TabSnap {
        TabSnap {
            title: title.into(),
            active: false,
            panes,
        }
    }

    fn ws(name: &str, tabs: Vec<TabSnap>) -> WorkspaceSnap {
        WorkspaceSnap {
            name: name.into(),
            active: false,
            tabs,
        }
    }

    fn rows(query: &str, snap: &[WorkspaceSnap]) -> Rows {
        build_rows_with_home(query, snap, Some(Path::new("/home/u")))
    }

    fn sample() -> Vec<WorkspaceSnap> {
        let mut api_pane = pane(1, "/home/u/dev/api-platform/api");
        api_pane.program = Some("cargo watch".into());
        api_pane.active = true;
        let mut tests_pane = pane(2, "/home/u/dev/api-platform/api/tests");
        tests_pane.last_command = Some("cargo watch -x check".into());
        let mut ssh_pane = pane(3, "/home/u");
        ssh_pane.program = Some("ssh prod-web".into());
        let mut api_tab = tab("api", vec![api_pane, tests_pane]);
        api_tab.active = true;
        let mut api_ws = ws(
            "api-platform",
            vec![
                api_tab,
                tab("web", vec![pane(5, "/home/u/dev/api-platform/web")]),
            ],
        );
        api_ws.active = true;
        vec![
            api_ws,
            ws(
                "personal",
                vec![
                    tab("~", vec![ssh_pane]),
                    // Renamed "deploy"; its auto title would be "myscripts".
                    tab("deploy", vec![pane(4, "/home/u/dev/myscripts")]),
                ],
            ),
            ws(
                "other",
                vec![tab("scripts", vec![pane(6, "/home/u/scripts")])],
            ),
        ]
    }

    fn targets(r: &Rows) -> Vec<Target> {
        r.rows.iter().map(|r| r.target).collect()
    }

    #[test]
    fn ancestors_ride_along_unscored_in_pre_order() {
        let r = rows("tests", &sample());
        assert_eq!(
            targets(&r),
            [
                Target::Workspace(0),
                Target::Tab(0, 0),
                Target::Pane(0, 0, 2)
            ]
        );
        assert_eq!(r.rows[0].score, None);
        assert_eq!(r.rows[1].score, None);
        assert!(r.rows[2].score.is_some());
        assert_eq!(r.selected, 2);
        assert_eq!((r.matched_panes, r.total_panes), (1, 6));
        // Every row knows the pane it lands on.
        assert_eq!(
            r.rows[0].pane,
            Some(1),
            "workspace → active tab's active pane"
        );
        assert_eq!(r.rows[1].pane, Some(1));
        assert_eq!(r.rows[2].pane, Some(2));
    }

    #[test]
    fn descendants_ride_along() {
        let r = rows("personal", &sample());
        assert_eq!(
            targets(&r),
            [
                Target::Workspace(1),
                Target::Tab(1, 0),
                Target::Pane(1, 0, 3),
                Target::Tab(1, 1),
                Target::Pane(1, 1, 4),
            ]
        );
        assert_eq!(r.selected, 0);
        // Only the workspace matched; the two panes listed under it
        // are there for context, not counted.
        assert_eq!(r.matched_panes, 0);
        // No active tab flagged: fall back to the first tab's first pane.
        assert_eq!(r.rows[0].pane, Some(3));
    }

    #[test]
    fn best_match_is_selected_wherever_it_is_and_order_is_kept() {
        // "myscripts" in workspace 1 is a mid-word hit; "scripts" in
        // workspace 2 is exact and wins, but still renders after it.
        let r = rows("scripts", &sample());
        assert_eq!(r.rows[r.selected].target, Target::Pane(2, 0, 6), "{r:?}");
        let ws1 = r.rows.iter().position(|r| r.target == Target::Workspace(1));
        let ws2 = r.rows.iter().position(|r| r.target == Target::Workspace(2));
        assert!(ws1.unwrap() < ws2.unwrap());
    }

    #[test]
    fn ties_go_deeper_then_to_the_current_workspace() {
        let snap = vec![ws("api", vec![tab("api", vec![pane(1, "/home/u/api")])])];
        let r = rows("api", &snap);
        assert_eq!(r.rows[r.selected].target, Target::Pane(0, 0, 1), "{r:?}");

        let mut snap = vec![ws("a", vec![tab("t", vec![pane(1, "/x/api")])]); 2];
        snap[1].active = true;
        let r = rows("api", &snap);
        assert_eq!(r.rows[r.selected].target, Target::Pane(1, 0, 1), "{r:?}");
    }

    #[test]
    fn basename_beats_path() {
        let snap = vec![ws(
            "w",
            vec![tab(
                "t",
                vec![
                    pane(1, "/home/u/dev/api-platform/docs"),
                    pane(2, "/home/u/dev/api"),
                ],
            )],
        )];
        let r = rows("api", &snap);
        let sel = &r.rows[r.selected];
        assert_eq!(sel.target, Target::Pane(0, 0, 2), "{r:?}");
        // The basename hit lights up the path's tail.
        assert_eq!(sel.label, "~/dev/api");
        assert_eq!(sel.highlights, vec![6, 7, 8]);
        // The path-only hit is highlighted where it lands in the path.
        assert_eq!(r.rows[2].highlights, vec![6, 7, 8]);
    }

    #[test]
    fn basename_highlight_survives_a_trailing_slash() {
        let snap = vec![ws("w", vec![tab("t", vec![pane(1, "/srv/api/")])])];
        let r = rows("api", &snap);
        let sel = &r.rows[r.selected];
        assert_eq!(sel.label, "/srv/api/");
        assert_eq!(sel.highlights, vec![5, 6, 7]);
    }

    #[test]
    fn custom_title_beats_auto_title() {
        let r = rows("dep", &sample());
        let tabs: Vec<_> = r
            .rows
            .iter()
            .filter(|r| matches!(r.target, Target::Tab(..)) && r.score.is_some())
            .map(|r| r.target)
            .collect();
        assert_eq!(tabs, [Target::Tab(1, 1)]);
    }

    #[test]
    fn program_and_last_command_are_searchable_without_highlights() {
        let r = rows("ssh prod", &sample());
        let sel = &r.rows[r.selected];
        assert_eq!(sel.target, Target::Pane(1, 0, 3), "{r:?}");
        assert!(sel.highlights.is_empty());
        assert_eq!(sel.detail.as_deref(), Some("ssh prod-web"));

        let r = rows("cargo w", &sample());
        let hits: Vec<_> = r
            .rows
            .iter()
            .filter(|r| r.score.is_some())
            .map(|r| r.target)
            .collect();
        assert_eq!(hits, [Target::Pane(0, 0, 1), Target::Pane(0, 0, 2)]);
        assert!(r.rows.iter().all(|r| r.highlights.is_empty()));
    }

    #[test]
    fn empty_query_lists_everything_with_the_current_pane_selected() {
        let r = rows("", &sample());
        assert_eq!(r.rows.len(), 3 + 5 + 6);
        assert_eq!(r.rows[r.selected].target, Target::Pane(0, 0, 1));
        let marked: Vec<_> = r
            .rows
            .iter()
            .filter(|r| r.current)
            .map(|r| r.target)
            .collect();
        assert_eq!(
            marked,
            [
                Target::Workspace(0),
                Target::Tab(0, 0),
                Target::Pane(0, 0, 1)
            ]
        );
        assert_eq!(r.matched_panes, r.total_panes);
        assert_eq!(r.rows[1].detail.as_deref(), Some("2 panes"));
    }

    #[test]
    fn no_matches() {
        let r = rows("zzzz", &sample());
        assert!(r.rows.is_empty());
        assert_eq!(r.selected, 0);
        assert_eq!(r.matched_panes, 0);
    }
}
