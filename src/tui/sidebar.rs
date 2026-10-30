//! The sidebar: the tree flattened into rows, folding, selection, scrolling, hit-testing and row rendering.

use super::theme::{pal, stage_color};
use super::App;
use crate::runner;
use crate::store::Status;
use crate::tree::{self, Kind, Node};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Sidebar columns per tree level.
const INDENT: usize = 3;

/// (space, node path): what a fold toggle remembers.
pub(super) type NodeKey = (PathBuf, PathBuf);

pub(super) struct Row {
    pub(super) depth: usize,
    pub(super) kind: Kind,
    pub(super) label: String,
    pub(super) path: PathBuf,
    pub(super) space: PathBuf,
    /// Agents: the thread they belong to.
    pub(super) thread: String,
    /// Has children, so it shows a fold arrow.
    pub(super) folder: bool,
    pub(super) open: bool,
    pub(super) stage: Option<usize>,
    pub(super) worktree: Option<String>,
}

pub(super) fn flatten(
    n: &Node,
    depth: usize,
    space: &Path,
    thread: &str,
    toggled: &HashSet<NodeKey>,
    out: &mut Vec<Row>,
) {
    let folder = !n.children.is_empty();
    let default_open = matches!(n.kind, Kind::Space | Kind::Folder);
    let open = folder && default_open != toggled.contains(&(space.into(), n.path.clone()));
    let thread = if n.kind == Kind::Thread {
        n.label.as_str()
    } else {
        thread
    };
    out.push(Row {
        depth,
        kind: n.kind.clone(),
        label: n.label.clone(),
        path: n.path.clone(),
        space: space.into(),
        thread: thread.into(),
        folder,
        open,
        stage: n.stage,
        worktree: n.worktree.clone(),
    });
    if open {
        n.children
            .iter()
            .for_each(|c| flatten(c, depth + 1, space, thread, toggled, out));
    }
}

/// The rows that fit in `h` sidebar lines from row `offset`, as (line, row index).
/// A blank line goes above each space and each thread but a space's first.
pub(super) fn layout(rows: &[Row], offset: usize, h: usize) -> Vec<(usize, usize)> {
    let mut y = 0;
    let mut out = vec![];
    for (i, r) in rows.iter().enumerate().skip(offset) {
        if i > offset
            && (r.kind == Kind::Space || r.kind == Kind::Thread && rows[i - 1].kind != Kind::Space)
        {
            y += 1;
        }
        if y >= h {
            break;
        }
        out.push((y, i));
        y += row_height(r);
    }
    out
}

/// Lines a row takes: the master row is a three-line banner, the rest one.
pub(super) fn row_height(r: &Row) -> usize {
    if r.kind == Kind::Master {
        3
    } else {
        1
    }
}

/// The smallest offset that still shows the last row in `h` lines.
pub(super) fn max_offset(rows: &[Row], h: usize) -> usize {
    (0..rows.len())
        .find(|&o| {
            layout(rows, o, h)
                .last()
                .is_some_and(|&(_, i)| i + 1 == rows.len())
        })
        .unwrap_or(0)
}

/// The sidebar row under a click, and whether the click hit its fold arrow.
pub(super) fn hit(
    inner: Rect,
    offset: usize,
    rows: &[Row],
    col: u16,
    row: u16,
) -> Option<(usize, bool)> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let dy = (row - inner.y) as usize;
    let i = layout(rows, offset, inner.height as usize)
        .into_iter()
        .find(|&(y, i)| (y..y + row_height(&rows[i])).contains(&dy))?
        .1;
    let r = &rows[i];
    let arrow_x = (r.depth * INDENT) as u16;
    let x = col - inner.x;
    Some((i, r.folder && x >= arrow_x && x < arrow_x + 2))
}

impl Status {
    /// An agent or terminal row's status glyph and its style.
    fn glyph(&self) -> (&'static str, Style) {
        match self {
            Status::Term => ("$ ", Style::new().fg(pal().green)),
            Status::Running => ("● ", Style::new().fg(pal().green)),
            Status::Idle => ("◦ ", Style::new().fg(pal().green)),
            Status::Blocked => (
                "! ",
                Style::new()
                    .fg(pal().red)
                    .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
            ),
            Status::Done => ("✓ ", Style::new().fg(pal().blue)),
            Status::Failed => ("✗ ", Style::new().fg(pal().red)),
            _ => ("○ ", Style::new().fg(pal().overlay0)),
        }
    }
}

impl App {
    /// Re-flattens the tree (after a refresh or a fold), keeping the selected node. The
    /// master row is pinned first, above every space.
    pub(super) fn reflow(&mut self) {
        let keep = self
            .rows
            .get(self.sel)
            .map(|r| (r.space.clone(), r.path.clone()));
        let dir = runner::master_dir();
        let mut rows = vec![Row {
            depth: 0,
            kind: Kind::Master,
            label: runner::MASTER.into(),
            path: dir.clone(),
            space: dir,
            thread: runner::MASTER.into(),
            folder: false,
            open: false,
            stage: None,
            worktree: None,
        }];
        for s in &self.tree {
            flatten(s, 0, &s.path, "", &self.toggled, &mut rows);
        }
        self.rows = rows;
        if let Some(i) = keep.and_then(|k| {
            self.rows
                .iter()
                .position(|r| (&r.space, &r.path) == (&k.0, &k.1))
        }) {
            self.sel = i;
        }
        self.sel = self.sel.min(self.rows.len().saturating_sub(1));
        if self.space.as_os_str().is_empty() {
            if let Some(r) = self.rows.iter().find(|r| r.kind == Kind::Space) {
                self.space = r.space.clone();
            }
        }
        if self.thread.is_empty() {
            if let Some(r) = self.rows.iter().find(|r| r.kind == Kind::Thread) {
                self.thread = r.label.clone();
            }
        }
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, self.rows.len() as isize - 1) as usize;
        self.follow(self.sel);
        let h = (self.side_inner.height as usize).max(1);
        self.offset = self.offset.min(self.sel);
        while self.offset < self.sel
            && !layout(&self.rows, self.offset, h)
                .iter()
                .any(|&(_, i)| i == self.sel)
        {
            self.offset += 1;
        }
    }

    /// Row `i`'s space and (unless it is a space row) thread become the shown ones.
    fn follow(&mut self, i: usize) {
        let r = &self.rows[i];
        if r.kind == Kind::Master {
            self.thread = r.thread.clone(); // its tab pane is keyed "master"; keep the shown space
            return;
        }
        self.space = r.space.clone();
        if !r.thread.is_empty() {
            self.thread = r.thread.clone();
        }
    }

    fn toggle(&mut self, i: usize) {
        let key = (self.rows[i].space.clone(), self.rows[i].path.clone());
        if !self.toggled.remove(&key) {
            self.toggled.insert(key);
        }
        self.reflow();
    }

    /// Click or Enter on row `i`: open an agent's terminal, an output.md viewer or a file's diff; fold anything else.
    pub(super) fn activate(&mut self, i: usize, arrow: bool) {
        if i >= self.rows.len() {
            return;
        }
        self.follow(i);
        let r = &self.rows[i];
        match r.kind {
            Kind::Output | Kind::Diff if !arrow => {
                let (path, thread) = (r.path.clone(), r.thread.clone());
                self.open_view(thread, path);
            }
            Kind::Agent if !arrow => {
                let (thread, agent, stage) =
                    (r.thread.clone(), tree::agent_id(&r.path, &r.label), r.stage);
                if r.folder && !r.open {
                    self.toggle(i);
                }
                self.open_agent(thread.clone(), &thread, &agent, stage);
            }
            Kind::Master => {
                self.master = true; // the run loop respawns it from now on
                self.open_agent(runner::MASTER.into(), runner::MASTER, runner::MASTER, None);
            }
            Kind::Terminal => {
                let (key, name) = (r.thread.clone(), r.label.clone());
                if let Some((a, _)) = self
                    .live_shells()
                    .into_iter()
                    .find(|(a, t)| a.agent == name && *t == key)
                {
                    self.open_agent(key, &a.thread, &name, None);
                }
            }
            _ if r.folder => self.toggle(i),
            _ => {}
        }
    }

    pub(super) fn side_key(&mut self, k: KeyEvent) {
        let sel = self.rows.get(self.sel).map(|r| (r.folder, r.open));
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Enter => self.activate(self.sel, false),
            KeyCode::Right | KeyCode::Char('l') if sel == Some((true, false)) => {
                self.toggle(self.sel)
            }
            KeyCode::Left | KeyCode::Char('h') if sel == Some((true, true)) => {
                self.toggle(self.sel)
            }
            KeyCode::Tab => self.focus_main = self.active_tab().is_some(),
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    pub(super) fn row_line(&self, r: &Row, selected: bool, width: usize) -> Line<'static> {
        let arrow = match (r.folder, r.open) {
            (false, _) => "  ",
            (true, true) => "▾ ",
            (true, false) => "▸ ",
        };
        let mut spans = vec![Span::raw(format!(
            "{}{arrow}",
            " ".repeat(r.depth * INDENT)
        ))];
        let mut label = Style::new();
        match r.kind {
            Kind::Agent | Kind::Terminal => {
                // A terminal shows "$" unless claude runs in it (refresh puts that status in agent_status).
                let st = self
                    .agent_status
                    .get(&(r.thread.clone(), r.label.clone()))
                    .cloned()
                    .unwrap_or(if r.kind == Kind::Terminal {
                        Status::Term
                    } else {
                        Status::Off
                    });
                let (g, style) = st.glyph();
                label = if st == Status::Blocked {
                    style
                } else if r.kind == Kind::Terminal {
                    label.fg(pal().text)
                } else {
                    label.fg(stage_color(r.stage))
                };
                spans.push(Span::styled(g, style));
                if let Some(s) = r.stage {
                    spans.push(Span::styled(
                        format!("{}·", s + 1),
                        Style::new().fg(stage_color(r.stage)),
                    ));
                }
            }
            Kind::Output => label = label.fg(pal().sky),
            Kind::Diff => label = label.fg(pal().yellow),
            Kind::Space => label = label.fg(pal().mauve).add_modifier(Modifier::BOLD),
            Kind::Thread => label = label.fg(pal().text).add_modifier(Modifier::BOLD),
            _ => label = label.fg(pal().subtext0),
        }
        let slash = if matches!(r.kind, Kind::Output | Kind::Diff | Kind::Terminal) {
            ""
        } else {
            "/"
        };
        spans.push(Span::styled(format!("{}{slash}", r.label), label));
        // Worktree name (agents) and +add -del (diffs) sit flush right, one column of padding.
        let mut right = vec![];
        if let Some(wt) = &r.worktree {
            right.push(Span::styled(wt.clone(), Style::new().fg(pal().overlay0)));
        }
        if let Some((add, del)) = (r.kind == Kind::Diff)
            .then(|| crate::diffs::diff_stat(&r.path))
            .flatten()
        {
            right.push(Span::styled(
                format!("+{add}"),
                Style::new().fg(pal().green),
            ));
            right.push(Span::raw(" "));
            right.push(Span::styled(format!("-{del}"), Style::new().fg(pal().red)));
        }
        if !right.is_empty() {
            let used: usize = spans
                .iter()
                .chain(&right)
                .map(|s| s.content.chars().count())
                .sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + 1).max(1))));
            spans.extend(right);
        }
        if r.kind == Kind::Thread {
            let st = self.thread_status.get(&r.path).map_or("", |s| s.as_str());
            spans.push(Span::styled(
                format!(" {st}"),
                Style::new().fg(pal().overlay0),
            ));
        }
        let line = Line::from(spans);
        match (selected, self.focus_main) {
            (true, false) => line.style(Style::new().add_modifier(Modifier::REVERSED)),
            (true, true) => line.style(Style::new().bg(pal().surface1)),
            // The space (repo) whose tabs show.
            _ if r.kind == Kind::Space && r.space == self.space => {
                line.style(Style::new().bg(pal().surface0))
            }
            _ => line,
        }
    }

    /// The master row's three-line banner: a brick block (half-block courses whose
    /// mortar joints stagger) with the live status glyph and the label centered on the
    /// middle course. Selected, the bricks light up yellow, like a hit '?' block.
    pub(super) fn master_banner(&self, selected: bool, width: usize) -> Vec<Line<'static>> {
        let live = runner::live(runner::MASTER, runner::MASTER)
            .filter(|a| a.alive())
            .map(|a| a.status());
        let (g, gs) = super::agent_glyph_status(live, Status::Pending).glyph();
        brick_banner(g, gs, selected, width)
    }
}

/// See `App::master_banner`; split out so tests need no live master session.
pub(super) fn brick_banner(
    glyph: &str,
    glyph_style: Style,
    selected: bool,
    width: usize,
) -> Vec<Line<'static>> {
    const BRICK_W: usize = 6; // 5 columns of brick, then a joint
    let brick = if selected { pal().yellow } else { pal().peach };
    let mortar = pal().crust;
    let joint = |x: usize, shift: usize| (x + shift) % BRICK_W == BRICK_W - 1;
    // Top line: course A over a mortar seam; bottom: a seam under course C. Their joints
    // line up with each other and sit half a brick off the middle course's.
    let course = |half: &'static str, shift: usize| {
        Line::from(
            (0..width)
                .map(|x| {
                    if joint(x, shift) {
                        Span::styled(" ", Style::new().bg(mortar))
                    } else {
                        Span::styled(half, Style::new().fg(brick).bg(mortar))
                    }
                })
                .collect::<Vec<_>>(),
        )
    };
    let gw = glyph.chars().count();
    let label = ["? MASTER · start a task ?", "? MASTER ?", "MASTER", ""]
        .into_iter()
        .find(|l| gw + l.chars().count() + 2 <= width)
        .unwrap_or("");
    let lw = if label.is_empty() {
        0
    } else {
        gw + label.chars().count()
    };
    let x0 = width.saturating_sub(lw) / 2;
    let on_brick = Style::new()
        .fg(mortar)
        .bg(brick)
        .add_modifier(Modifier::BOLD);
    let wall = |x: usize| {
        let mortar = Style::new().bg(mortar);
        Span::styled(
            " ",
            if joint(x, BRICK_W / 2) {
                mortar
            } else {
                on_brick
            },
        )
    };
    let mut mid: Vec<_> = (0..x0).map(wall).collect();
    if lw > 0 {
        mid.push(Span::styled(
            glyph.to_string(),
            glyph_style.fg(mortar).bg(brick),
        ));
        mid.push(Span::styled(label, on_brick));
    }
    mid.extend((x0 + lw..width).map(wall));
    vec![course("▀", 0), Line::from(mid), course("▄", 0)]
}
