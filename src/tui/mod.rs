//! brigd's TUI: a sidebar of spaces (the tree.rs folder tree) and, for the
//! selected thread, one or two side-by-side panes, each a tab bar of agent terminals
//! (runner.rs PTYs drawn with tui-term) and read-only output.md viewers.

use crate::runner::{self, LiveAgent};
use crate::tree::{self, Kind, Node};
use crate::Res;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self as ct, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};
use tui_term::widget::PseudoTerminal;

mod markdown;
mod theme;
use markdown::{diff_lines, md_text};
use theme::{init_theme, next_theme, pal, stage_color};

const SIDE_W: u16 = 40;
const MIN_SIDE_W: u16 = 12;
/// Sidebar columns per tree level.
const INDENT: usize = 3;
const REFRESH: Duration = Duration::from_millis(700);
/// Braille spinner for running steps; the frame comes from the clock (run redraws every ≤40ms).
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Run-log lines the FLOWPLAN view shows under the plan.
const LOG_TAIL: usize = 15;

/// (space, node path): what a fold toggle remembers.
type NodeKey = (PathBuf, PathBuf);

struct Row {
    depth: usize,
    kind: Kind,
    label: String,
    path: PathBuf,
    space: PathBuf,
    /// Agents: the thread they belong to.
    thread: String,
    /// Has children, so it shows a fold arrow.
    folder: bool,
    open: bool,
    stage: Option<usize>,
    worktree: Option<String>,
}

enum Tab {
    Term {
        thread: String,
        agent: String,
        stage: Option<usize>,
        live: Arc<LiveAgent>,
    },
    /// `rel`: a Diff node's file path (relative to the agent's cwd); `text` is then its diff.
    View {
        path: PathBuf,
        title: String,
        text: String,
        scroll: u16,
        rel: Option<String>,
    },
}

/// The right-click popup: an agent row's Terminate, a thread row's Delete / Archive.
/// Item `i` fills line `rect.y + i`; `sel` is the one Enter picks.
struct Menu {
    rect: Rect,
    items: Vec<Act>,
    sel: usize,
}

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Terminate { thread: String, agent: String },
    Delete(String),
    Archive(String),
}

impl Act {
    fn label(&self) -> &'static str {
        match self {
            Act::Terminate { .. } => " Terminate ",
            Act::Delete(_) => " Delete    ",
            Act::Archive(_) => " Archive   ",
        }
    }
}

#[derive(Default)]
struct Tabs {
    list: Vec<Tab>,
    active: usize,
    // From the last draw: the pane's tab bar, its body, and each shown tab's (x0, x1, index).
    bar: Rect,
    body: Rect,
    spans: Vec<(u16, u16, usize)>,
}

/// A thread's panes, left to right (1 or 2, never 0), and the focused one.
struct Panes {
    list: Vec<Tabs>,
    focus: usize,
}

impl Default for Panes {
    fn default() -> Self {
        Panes {
            list: vec![Tabs::default()],
            focus: 0,
        }
    }
}

impl Panes {
    /// (pane, tab) of the first tab matching `f`: an open tab is focused, never shown twice.
    fn find(&self, f: impl Fn(&Tab) -> bool) -> Option<(usize, usize)> {
        self.list
            .iter()
            .enumerate()
            .find_map(|(j, t)| Some((j, t.list.iter().position(&f)?)))
    }
}

/// `n` equal columns of `main` with a 1-column gap: each pane's (tab bar, body).
fn pane_rects(main: Rect, n: usize) -> Vec<(Rect, Rect)> {
    let cols = Layout::horizontal(vec![Constraint::Fill(1); n.max(1)])
        .spacing(1)
        .split(main);
    cols.iter()
        .map(|&c| {
            (
                Rect {
                    height: c.height.min(1),
                    ..c
                },
                Rect {
                    y: c.y + c.height.min(1),
                    height: c.height.saturating_sub(1),
                    ..c
                },
            )
        })
        .collect()
}

/// Tabs per tab bar; opening one more closes the oldest opened (its agent keeps running).
const MAX_TABS: usize = 5;
/// Width of the new-thread window.
const NEWT_W: u16 = 60;

fn push_tab(tabs: &mut Tabs, tab: Tab) {
    tabs.list.push(tab);
    while tabs.list.len() > MAX_TABS {
        tabs.list.remove(0);
    }
    tabs.active = tabs.list.len() - 1;
}

/// What the planner and execute threads of a '+' new thread report to the run loop.
enum Bg {
    /// (thread, repo): planned, waiting for the y/n confirm.
    Planned(String, String),
    /// (thread, message): planning failed.
    Failed(String, String),
    Finished(String, Result<(), String>),
}

/// The '+' new-thread window: dir, name and task fields; once submitted, read-only while planning.
#[derive(Debug, Default, PartialEq)]
struct Newt {
    /// dir, name, task.
    fields: [String; 3],
    /// Index into `fields` of the focused one.
    focus: usize,
    err: Option<String>,
    /// Enter on the task field passed validation: asking "Use plan-review-go?".
    ask: bool,
    /// Submitted: the planner thread runs and the fields are read-only.
    planning: bool,
    /// Esc while planning: the box is gone but planning goes on.
    hidden: bool,
}

/// `s` cut into rows of at most `w` display columns (and at each '\n'); always at least one row.
fn chunk(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let (mut rows, mut row, mut used) = (vec![], String::new(), 0);
    for c in s.chars() {
        let cw = if c == '\n' {
            0
        } else {
            Span::raw(c.to_string()).width()
        };
        if c == '\n' || (used + cw > w && used > 0) {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        if c != '\n' {
            row.push(c);
            used += cw;
        }
    }
    rows.push(row);
    rows
}

#[derive(Default)]
struct App {
    tree: Vec<Node>,
    rows: Vec<Row>,
    toggled: HashSet<NodeKey>,
    sel: usize,
    offset: usize,
    focus_main: bool,
    /// The selected row's space (repo): highlighted, and new terminals' cwd.
    space: PathBuf,
    /// The thread whose tabs show.
    thread: String,
    tabs: BTreeMap<String, Panes>,
    /// (thread, agent) -> running / blocked / done / failed / off
    agent_status: HashMap<(String, String), &'static str>,
    /// thread dir -> state.json status
    thread_status: HashMap<PathBuf, String>,
    blocked: HashSet<(String, String)>,
    prefix: bool,
    /// Ctrl-t was pressed: the next key acts on the highlighted thread.
    thread_prefix: bool,
    confirm_quit: bool,
    /// A thread waiting for y to be deleted.
    confirm_delete: Option<String>,
    quit: bool,
    msg: Option<(String, Instant)>,
    ticks: u32,
    menu: Option<Menu>,
    /// Threads whose agents are paused (their dot is filled). In memory only.
    paused: HashSet<String>,
    /// Shell tab registry key -> the thread its $BRIGD_THREAD names (strays run inside these).
    shell_thread: HashMap<(String, String), String>,
    /// Pause dot rects from the last draw.
    pause_btns: Vec<(Rect, String)>,
    /// '+' rects from the last draw -> the space (repo) a new thread goes in.
    plus_btns: Vec<(Rect, PathBuf)>,
    /// The sidebar header's '＋' (also in `plus_btns`): opens the window on the dir field.
    header_plus: Rect,
    /// The new-thread window. Takes every key unless hidden.
    newt: Option<Newt>,
    /// A planned (thread, repo) waiting for y to run, n to be deleted.
    confirm_run: Option<(String, String)>,
    /// Planned threads waiting behind `confirm_run`.
    queued: Vec<(String, String)>,
    /// Threads whose reserved dir the planner thread still owns.
    planning: HashSet<String>,
    /// Created on first use; background threads send on clones of the sender.
    bg: Option<(mpsc::Sender<Bg>, mpsc::Receiver<Bg>)>,
    // Layout from the last draw, for mouse hit tests.
    screen: Rect,
    side: Rect,
    side_inner: Rect,
    /// Divider column from the last draw.
    divider: Rect,
    /// Sidebar width set by dragging the divider; None = SIDE_W.
    side_w: Option<u16>,
    /// A divider drag is in progress.
    resizing: bool,
    /// Right of the divider: every pane with its tab bar.
    main: Rect,
    hint: Rect,
    /// Left-drag text selection: (rect it started in, anchor, cursor once dragged).
    drag: Option<(Rect, Position, Option<Position>)>,
    /// The last frame while dragging, to read the selected text from.
    buf: Buffer,
    /// Selected text waiting for the main loop to put on the clipboard.
    clip: Option<String>,
    /// Open FLOWPLAN tab path -> its thread's flowmap.json and state.json, which the view draws from.
    plans: HashMap<PathBuf, (crate::store::Flow, crate::store::State)>,
    /// (FLOWPLAN path, agent) whose task shows in full. In memory only.
    expanded: HashSet<(PathBuf, String)>,
    /// Task "..." (or expanded task line) rects from the last draw -> the (FLOWPLAN path, agent) it toggles.
    task_btns: Vec<(Rect, (PathBuf, String))>,
}

/// Opens the TUI over every thread in ~/.brigd/threads until the user quits.
/// Quitting kills every live agent.
pub fn run(open: Option<&str>) -> Res<()> {
    init_theme();
    setup()?;
    let res = (|| -> Res<()> {
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        term.clear()?;
        let mut app = App::default();
        // `brigd resume --bg` hands threads to the TUI whose pid is here.
        _ = fs::write(
            crate::launch::tui_pid_path(),
            std::process::id().to_string(),
        );
        app.refresh();
        if let Some(t) = open {
            app.open_flowplan(t);
        }
        // The tree is rebuilt off the UI thread (git and disk reads take 40ms+): keys never wait on it.
        let (tx, trees) = mpsc::channel();
        std::thread::spawn(move || {
            let root = crate::store::root();
            while tx.send(tree::build(&root)).is_ok() {
                std::thread::sleep(REFRESH);
            }
        });
        while !app.quit {
            term.draw(|f| app.draw(f))?;
            if event::poll(Duration::from_millis(40))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m),
                    Event::Paste(s) => app.on_paste(&s),
                    _ => {} // Resize: the next draw sees the new body size and resizes every PTY
                }
                if let Some(s) = app.clip.take() {
                    copy_to_clipboard(&s);
                }
            }
            while let Some(m) = app.bg.as_ref().and_then(|(_, rx)| rx.try_recv().ok()) {
                app.on_bg(m);
            }
            if let Some(t) = trees.try_iter().last() {
                app.refresh_with(t);
                app.claim_queued();
                if app.ticks.is_multiple_of(7) {
                    app.reconcile_running();
                }
            }
        }
        if crate::launch::tui_pid() == Some(std::process::id()) {
            _ = fs::remove_file(crate::launch::tui_pid_path());
        }
        live_agents().iter().for_each(|a| a.kill());
        // A plan still running dies with brigd: free the names it reserved.
        app.planning
            .iter()
            .for_each(|n| _ = fs::remove_dir_all(crate::store::thread_dir(n)));
        Ok(())
    })();
    teardown();
    res
}

fn setup() -> io::Result<()> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A panic in an execute/supervise thread must not tear down a still-running UI.
        if std::thread::current().name() == Some("main") {
            teardown();
        }
        prev(info);
    }));
    ct::enable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
}

fn teardown() {
    let _ = crossterm::execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = ct::disable_raw_mode();
}

/// OSC 52 for terminals that take it, pbcopy (in the background) for those that don't.
fn copy_to_clipboard(s: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(s.as_bytes())).and_then(|_| out.flush());
    if cfg!(target_os = "macos") {
        let s = s.to_string();
        std::thread::spawn(move || {
            let _ = Command::new("pbcopy")
                .stdin(Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    c.stdin.take().unwrap().write_all(s.as_bytes())?;
                    c.wait()
                });
        });
    }
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    b.chunks(3)
        .flat_map(|c| {
            let n = c
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &x)| n | ((x as u32) << (16 - 8 * i)));
            (0..4).map(move |i| {
                if i <= c.len() {
                    T[((n >> (18 - 6 * i)) & 63) as usize] as char
                } else {
                    '='
                }
            })
        })
        .collect()
}

/// The cells from `a` to `b` in reading order, like a terminal selection, clamped to `r`.
fn sel_rows(r: Rect, a: Position, b: Position) -> Vec<(u16, std::ops::RangeInclusive<u16>)> {
    let clamp = |p: Position| Position {
        x: p.x.clamp(r.x, r.right() - 1),
        y: p.y.clamp(r.y, r.bottom() - 1),
    };
    let (a, b) = (clamp(a), clamp(b));
    let (a, b) = if (a.y, a.x) <= (b.y, b.x) {
        (a, b)
    } else {
        (b, a)
    };
    (a.y..=b.y)
        .map(|y| {
            (
                y,
                if y == a.y { a.x } else { r.x }..=if y == b.y { b.x } else { r.right() - 1 },
            )
        })
        .collect()
}

// ponytail: wide chars read as their cell symbols; a CJK glyph may gain a trailing space.
fn sel_text(buf: &Buffer, r: Rect, a: Position, b: Position) -> String {
    let line = |(y, xs): (u16, std::ops::RangeInclusive<u16>)| {
        xs.filter_map(|x| buf.cell((x, y)))
            .map(|c| c.symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    sel_rows(r, a, b)
        .into_iter()
        .map(line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn live_agents() -> Vec<Arc<LiveAgent>> {
    let all: Vec<Arc<LiveAgent>> = runner::registry()
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect();
    all.into_iter().filter(|a| a.alive()).collect()
}

/// The tabs `first..end` shown in `avail` columns, a ‹ / › marker costing one column each when tabs
/// are hidden on that side. With `follow`, `first` moves just far enough that that tab is fully shown.
fn tab_window(widths: &[u16], first: usize, follow: Option<usize>, avail: u16) -> (usize, usize) {
    let n = widths.len();
    let end_from = |first: usize| {
        let room = avail.saturating_sub((first > 0) as u16);
        let mut used = 0;
        let mut end = first;
        while end < n && used + widths[end] <= room {
            used += widths[end];
            end += 1;
        }
        // Not everything fits: the › marker takes a column, so drop what no longer fits.
        while end < n && end > first && used + 1 > room {
            end -= 1;
            used -= widths[end];
        }
        end
    };
    let mut first = first.min(n.saturating_sub(1));
    if let Some(a) = follow.filter(|&a| a < n) {
        first = first.min(a);
        while first < a && end_from(first) <= a {
            first += 1;
        }
    }
    (first, end_from(first))
}

/// While the process lives its hook status wins (a done agent the user types into is live
/// again): running / blocked / idle. Once it exits: done / failed from state.json, else off.
fn agent_glyph_status(live: Option<&str>, saved: &str) -> &'static str {
    match live {
        Some("blocked") => "blocked",
        Some("idle") => "idle",
        Some(_) => "running",
        None if saved == "done" => "done",
        None if saved == "failed" => "failed",
        None => "off",
    }
}

/// The flow as an npx-style step tree ending at crate::flow::LOG_MARK; also the plain FLOWPLAN
/// text (crate::flow::flow_text). `status(agent)`: pending / running / blocked / done / failed.
/// A task shows its first TASK_WORDS words and a "..." unless `expanded(agent)`. Each task
/// line pushes (line index, clickable columns, agent) to `hits`: the "..." when collapsed,
/// the whole line when expanded.
pub fn plan_lines(
    flow: &crate::store::Flow,
    status: impl Fn(&str) -> &'static str,
    expanded: impl Fn(&str) -> bool,
    hits: &mut Vec<(usize, Range<u16>, String)>,
    frame: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(pal().overlay0);
    let guide = |s: &str| Span::styled(s.to_string(), dim);
    let mut out = vec![];
    for (i, l) in wrap(&flow.goal, width.saturating_sub(3).max(20))
        .into_iter()
        .enumerate()
    {
        let lead = if i == 0 {
            Span::styled("◆  ", Style::new().fg(pal().mauve))
        } else {
            guide("│  ")
        };
        out.push(Line::from(vec![
            lead,
            Span::styled(l, Style::new().fg(pal().text).add_modifier(Modifier::BOLD)),
        ]));
    }
    let w = flow
        .stages
        .iter()
        .flatten()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    for (i, stage) in flow.stages.iter().enumerate() {
        let c = stage_color(Some(i));
        let sts: Vec<&str> = stage.iter().map(|a| status(&a.name)).collect();
        let st = if sts.iter().all(|s| *s == "done") {
            "done"
        } else if sts.contains(&"failed") {
            "failed"
        } else if sts.iter().any(|s| matches!(*s, "running" | "blocked")) {
            "running"
        } else {
            "pending"
        };
        let n = if stage.len() == 1 {
            "1 agent".to_string()
        } else {
            format!("{} parallel", stage.len())
        };
        out.push(Line::from(guide("│")));
        out.push(Line::from(vec![
            step(st, frame),
            Span::raw("  "),
            Span::styled(
                format!("stage {}", i + 1),
                Style::new().fg(c).add_modifier(Modifier::BOLD),
            ),
            guide(&format!(" · {n}")),
        ]));
        for (j, (a, st)) in stage.iter().zip(&sts).enumerate() {
            let last = j + 1 == stage.len();
            let wt = a
                .worktree
                .as_deref()
                .filter(|w| !w.is_empty())
                .map_or("main".into(), |w| format!("wt {w}"));
            let meta = format!("{} · {} · {wt}", a.model, a.effort);
            out.push(Line::from(vec![
                guide(if last { "│  └─ " } else { "│  ├─ " }),
                step(st, frame),
                Span::styled(format!(" {:<w$}  ", a.name), Style::new().fg(c)),
                guide(&meta),
            ]));
            let tw = width.saturating_sub(8).max(20);
            let lead = if last { "│       " } else { "│  │    " };
            let words: Vec<&str> = a.task.split_whitespace().collect();
            if words.len() > TASK_WORDS && !expanded(&a.name) {
                let ws = wrap(&words[..TASK_WORDS].join(" "), tw);
                let n = ws.len();
                for (k, w) in ws.into_iter().enumerate() {
                    let mut l = vec![guide(lead)];
                    if k + 1 == n {
                        let x = (lead.chars().count() + w.chars().count() + 1) as u16;
                        hits.push((out.len(), x..x + 3, a.name.clone()));
                        l.push(Span::styled(w + " ", dim.add_modifier(Modifier::ITALIC)));
                        l.push(Span::styled(
                            "...",
                            Style::new().fg(c).add_modifier(Modifier::BOLD),
                        ));
                    } else {
                        l.push(Span::styled(w, dim.add_modifier(Modifier::ITALIC)));
                    }
                    out.push(Line::from(l));
                }
                continue;
            }
            // Blank task lines are dropped, like wrap() drops them in the goal.
            for l in a.task.lines().filter(|l| !l.trim().is_empty()) {
                for w in wrap(l, tw) {
                    if words.len() > TASK_WORDS {
                        hits.push((out.len(), 0..u16::MAX, a.name.clone()));
                    }
                    out.push(Line::from(vec![
                        guide(lead),
                        Span::styled(w, dim.add_modifier(Modifier::ITALIC)),
                    ]));
                }
            }
        }
    }
    out.push(Line::from(guide("│")));
    out.push(Line::from(guide(crate::flow::LOG_MARK)));
    out
}

type Nodes = Vec<(String, String)>;
type Edges = Vec<(usize, usize, String)>;

/// A mermaid flowchart (flow.md): its nodes as (id, label) and edges as (from, to, label), in source order.
/// None unless it opens with `flowchart`/`graph` and has a node. Handles `A[x] --> B{y}`, chains and `-->|label|`.
fn parse_flow(src: &str) -> Option<(Nodes, Edges)> {
    let body = src
        .split("```mermaid")
        .nth(1)
        .map_or(src, |s| s.split("```").next().unwrap_or(s));
    let mut lines = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    let head = lines.next()?;
    if !(head.starts_with("flowchart") || head.starts_with("graph")) {
        return None;
    }
    let (mut nodes, mut edges) = (Vec::<(String, String)>::new(), vec![]);
    let mut node = |tok: &str| -> Option<usize> {
        let tok = tok.trim();
        let i = tok.find(['[', '(', '{', '>']).unwrap_or(tok.len());
        let id = tok[..i].trim();
        if id.is_empty() || id.contains(char::is_whitespace) {
            return None;
        }
        let label = tok[i..].trim_matches(|c| "[](){}>\"/\\ ".contains(c));
        let k = nodes.iter().position(|n| n.0 == id).unwrap_or_else(|| {
            nodes.push((id.into(), id.into()));
            nodes.len() - 1
        });
        if !label.is_empty() {
            nodes[k].1 = label.into();
        }
        Some(k)
    };
    for l in lines {
        if [
            "style ",
            "classDef ",
            "class ",
            "click ",
            "linkStyle ",
            "subgraph",
            "end",
            "direction ",
        ]
        .iter()
        .any(|k| l.starts_with(k))
        {
            continue;
        }
        let mut parts = l.split("-->");
        let mut from: Vec<usize> = parts
            .next()?
            .split('&')
            .map(&mut node)
            .collect::<Option<_>>()?;
        for p in parts {
            let (label, rest) = match p
                .trim_start()
                .strip_prefix('|')
                .and_then(|r| r.split_once('|'))
            {
                Some((lab, rest)) => (lab.trim(), rest),
                None => ("", p),
            };
            let to: Vec<usize> = rest.split('&').map(&mut node).collect::<Option<_>>()?;
            for &f in &from {
                edges.extend(to.iter().map(|&t| (f, t, label.to_string())));
            }
            from = to;
        }
    }
    (!nodes.is_empty()).then_some((nodes, edges))
}

/// Nodes grouped into steps: a node sits one step below its deepest parent. A loop's
/// back edge is dropped: when none is left without parents the first unplaced node starts.
fn flow_steps(n: usize, edges: &[(usize, usize, String)]) -> Vec<Vec<usize>> {
    let mut indeg = vec![0; n];
    for e in edges.iter().filter(|e| e.0 != e.1) {
        indeg[e.1] += 1;
    }
    let (mut depth, mut placed, mut queue) = (vec![0; n], vec![false; n], vec![]);
    for _ in 0..n {
        let next = queue
            .pop()
            .or_else(|| (0..n).find(|&v| !placed[v] && indeg[v] == 0));
        let Some(u) = next.or_else(|| (0..n).find(|&v| !placed[v])) else {
            break;
        };
        placed[u] = true;
        for e in edges.iter().filter(|e| e.0 == u && !placed[e.1]) {
            depth[e.1] = depth[e.1].max(depth[u] + 1);
            indeg[e.1] -= 1;
            if indeg[e.1] == 0 && !queue.contains(&e.1) {
                queue.push(e.1);
            }
        }
    }
    let mut steps = vec![vec![]; n];
    for v in 0..n {
        steps[depth[v]].push(v);
    }
    steps.retain(|s| !s.is_empty());
    steps
}

/// flow.md's mermaid flowchart as a FLOWPLAN-style step tree; None (→ raw markdown) if it won't parse.
/// A node whose parents aren't exactly the step above names them (`← a, b`), so branches stay readable.
/// ponytail: O(nodes × edges) per frame; fine for the ≤10-node flows agents are asked for.
fn flow_lines(text: &str) -> Option<Vec<Line<'static>>> {
    let (nodes, edges) = parse_flow(text)?;
    let steps = flow_steps(nodes.len(), &edges);
    let dim = Style::new().fg(pal().overlay0);
    let mut out = vec![];
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            out.push(Line::styled("│", dim));
        }
        let mut prev = if i > 0 { steps[i - 1].clone() } else { vec![] };
        prev.sort();
        for (j, &v) in s.iter().enumerate() {
            let lead = match (s.len(), j + 1 == s.len()) {
                (1, _) => "",
                (_, true) => "└─ ",
                _ => "├─ ",
            };
            let ins: Vec<_> = edges.iter().filter(|e| e.1 == v && e.0 != v).collect();
            let mut ps: Vec<usize> = ins.iter().map(|e| e.0).collect();
            ps.sort();
            ps.dedup();
            let mut l = vec![Span::styled(lead, dim), step("done", 0), Span::raw("  ")];
            l.push(Span::styled(
                nodes[v].1.clone(),
                Style::new().fg(stage_color(Some(i))),
            ));
            if !ps.is_empty() && ps != prev {
                let names: Vec<&str> = ps.iter().map(|&p| nodes[p].1.as_str()).collect();
                l.push(Span::styled(format!("  ← {}", names.join(", ")), dim));
            }
            if let Some(e) = ins.iter().find(|e| !e.2.is_empty()) {
                l.push(Span::styled(format!("  · {}", e.2), dim));
            }
            out.push(Line::from(l));
        }
    }
    Some(out)
}

/// Words a collapsed FLOWPLAN task shows before its "...".
const TASK_WORDS: usize = 10;

/// A step's glyph: pending ◇, running spinner, blocked !, done ✔, failed ✖.
fn step(st: &str, frame: usize) -> Span<'static> {
    match st {
        "done" => Span::styled("✔", Style::new().fg(pal().green)),
        "failed" => Span::styled("✖", Style::new().fg(pal().red)),
        "blocked" => Span::styled("!", Style::new().fg(pal().red).add_modifier(Modifier::BOLD)),
        "running" => Span::styled(SPIN[frame % SPIN.len()], Style::new().fg(pal().sky)),
        _ => Span::styled("◇", Style::new().fg(pal().overlay0)),
    }
}

/// Word-wraps `s` to `w` columns (a longer word gets a line of its own).
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    for word in s.split_whitespace() {
        let cur = out.last().map_or(0, |l| l.chars().count());
        if cur > 0 && cur + 1 + word.chars().count() > w {
            out.push(String::new());
        }
        let l = out.last_mut().unwrap();
        if !l.is_empty() {
            l.push(' ');
        }
        l.push_str(word);
    }
    out
}

fn flatten(
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
fn layout(rows: &[Row], offset: usize, h: usize) -> Vec<(usize, usize)> {
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
        y += 1;
    }
    out
}

/// The smallest offset that still shows the last row in `h` lines.
fn max_offset(rows: &[Row], h: usize) -> usize {
    (0..rows.len())
        .find(|&o| {
            layout(rows, o, h)
                .last()
                .is_some_and(|&(_, i)| i + 1 == rows.len())
        })
        .unwrap_or(0)
}

/// The sidebar row under a click, and whether the click hit its fold arrow.
fn hit(inner: Rect, offset: usize, rows: &[Row], col: u16, row: u16) -> Option<(usize, bool)> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let dy = (row - inner.y) as usize;
    let i = layout(rows, offset, inner.height as usize)
        .into_iter()
        .find(|&(y, _)| y == dy)?
        .1;
    let r = &rows[i];
    let arrow_x = (r.depth * INDENT) as u16;
    let x = col - inner.x;
    Some((i, r.folder && x >= arrow_x && x < arrow_x + 2))
}

/// A key as the bytes a terminal would send. `app_cursor` = DECCKM (arrows as ESC O x).
fn key_bytes(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let (ctrl, alt) = (
        k.modifiers.contains(KeyModifiers::CONTROL),
        k.modifiers.contains(KeyModifiers::ALT),
    );
    let arrow = |c: u8| {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let mut out = match k.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![c.to_ascii_lowercase() as u8 - b'a' + 1]
        }
        KeyCode::Char(' ' | '@') if ctrl => vec![0],
        KeyCode::Char('[') if ctrl => vec![0x1b],
        KeyCode::Char('\\') if ctrl => vec![0x1c],
        KeyCode::Char(']') if ctrl => vec![0x1d],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow(b'A'),
        KeyCode::Down => arrow(b'B'),
        KeyCode::Right => arrow(b'C'),
        KeyCode::Left => arrow(b'D'),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + n - 1],
        KeyCode::F(n @ 5..=12) => {
            format!("\x1b[{}~", [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]).into_bytes()
        }
        _ => vec![],
    };
    if alt && !out.is_empty() && k.code != KeyCode::Esc {
        out.insert(0, 0x1b);
    }
    out
}

impl App {
    fn flash(&mut self, m: impl Into<String>) {
        self.msg = Some((m.into(), Instant::now()));
    }

    /// Re-reads the tree and statuses from disk and the registry.
    fn refresh(&mut self) {
        self.refresh_with(tree::build(&crate::store::root()));
    }

    /// `refresh` with a tree already built (by the worker in `run`).
    fn refresh_with(&mut self, tree: Vec<Node>) {
        self.ticks += 1;
        self.tree = tree;
        let terms: Vec<_> = self
            .live_shells()
            .into_iter()
            .map(|(a, t)| (t, a.agent.clone()))
            .collect();
        add_terminals(&mut self.tree, &terms);
        let reg = runner::registry().lock().unwrap().clone();
        let (mut agents, mut threads) = (HashMap::new(), HashMap::new());
        for t in self.tree.iter().flat_map(|s| &s.children) {
            let state: serde_json::Value = fs::read_to_string(t.path.join("state.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            threads.insert(
                t.path.clone(),
                state["status"].as_str().unwrap_or("").to_string(),
            );
            for a in t.children.iter().filter(|a| a.kind == Kind::Agent) {
                let key = (t.label.clone(), a.label.clone());
                let saved = state["agents"][&a.label]["status"].as_str().unwrap_or("");
                let live = reg.get(&key).filter(|l| l.alive()).map(|l| l.status());
                agents.insert(key, agent_glyph_status(live.as_deref(), saved));
            }
            // A terminal running claude (a stray whose pid lives) gets that claude's status; else it stays "$".
            for d in tree::stray_dirs(&t.path) {
                let (st, term) = (
                    stray_status(&d),
                    fs::read_to_string(d.join("term")).unwrap_or_default(),
                );
                if !matches!(st, "off" | "failed") && !term.is_empty() {
                    agents.insert((t.label.clone(), term.trim().to_string()), st);
                }
            }
        }
        let blocked: HashSet<_> = agents
            .iter()
            .filter(|(_, s)| **s == "blocked")
            .map(|(k, _)| k.clone())
            .collect();
        if blocked.difference(&self.blocked).next().is_some() {
            let _ = io::stdout()
                .write_all(b"\x07")
                .and_then(|_| io::stdout().flush());
        }
        (self.agent_status, self.thread_status, self.blocked) = (agents, threads, blocked);

        // Tabs follow a restart of their agent (execute or resume registers a new LiveAgent).
        let diff_tick = self.ticks.is_multiple_of(4); // git diff open diff tabs each ~3s, not every refresh
        for tab in self
            .tabs
            .values_mut()
            .flat_map(|p| &mut p.list)
            .flat_map(|t| &mut t.list)
        {
            match tab {
                Tab::Term {
                    thread,
                    agent,
                    live,
                    ..
                } => {
                    if let Some(new) = reg
                        .get(&(thread.clone(), agent.clone()))
                        .filter(|n| n.alive() && !Arc::ptr_eq(n, live))
                    {
                        *live = new.clone();
                    }
                }
                Tab::View {
                    path,
                    text,
                    rel: Some(_),
                    ..
                } => {
                    if let Some((_, t)) = diff_tick.then(|| crate::diffs::diff_view(path)).flatten()
                    {
                        *text = t;
                    }
                }
                Tab::View { path, text, .. } => {
                    *text = fs::read_to_string(&*path).unwrap_or_else(|e| format!("({e})"))
                }
            }
        }
        self.load_plans();
        self.resize_all();
        // supervise mirrors subagents only for agents execute runs; this covers ones resumed from the sidebar.
        if self.ticks.is_multiple_of(7) {
            let home = crate::store::home();
            let mut runs: Vec<(PathBuf, PathBuf)> = live_agents()
                .into_iter()
                .filter(|a| !a.dir.as_os_str().is_empty())
                .map(|a| (a.dir.clone(), a.cwd.clone()))
                .collect();
            // Strays running in a terminal tab (they keep a pid file while alive).
            for d in self
                .tree
                .iter()
                .flat_map(|s| &s.children)
                .flat_map(|t| tree::stray_dirs(&t.path))
                .filter(|d| d.join("pid").exists())
            {
                runs.extend(
                    fs::read_to_string(d.join("cwd")).map(|c| (d.clone(), PathBuf::from(c.trim()))),
                );
            }
            for (dir, cwd) in runs {
                crate::transcript::sync_agent(&home, &cwd, &dir);
            }
        }
        self.reflow();
    }

    /// Reloads flowmap.json + state.json for every open FLOWPLAN tab.
    fn load_plans(&mut self) {
        let paths = self
            .tabs
            .values()
            .flat_map(|p| &p.list)
            .flat_map(|t| &t.list)
            .filter_map(|t| match t {
                Tab::View {
                    path, rel: None, ..
                } if path.ends_with("FLOWPLAN") => Some(path.clone()),
                _ => None,
            });
        self.plans = paths
            .filter_map(|p| {
                Some((
                    p.clone(),
                    crate::store::load(&p.parent()?.file_name()?.to_string_lossy()).ok()?,
                ))
            })
            .collect();
    }

    /// A FLOWPLAN tab's view: the step tree with live statuses, then the run log's tail.
    fn plan_view(
        &self,
        path: &Path,
        text: &str,
        width: usize,
        hits: &mut Vec<(usize, Range<u16>, String)>,
    ) -> Option<Vec<Line<'static>>> {
        let (flow, state) = self.plans.get(path)?;
        // Saved done/failed wins (a done agent idles on, live); else the live hook status.
        let status = |a: &str| match state.agents.get(a).map(|s| s.status.as_str()) {
            Some("done") => "done",
            Some("failed") => "failed",
            _ => match self
                .agent_status
                .get(&(state.thread.clone(), a.to_string()))
                .copied()
            {
                Some("running" | "idle") => "running",
                Some("blocked") => "blocked",
                _ => "pending",
            },
        };
        let frame = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() / 80) as usize;
        let expanded = |a: &str| self.expanded.contains(&(path.to_path_buf(), a.to_string()));
        let mut lines = plan_lines(flow, status, expanded, hits, frame, width);
        // The log follows LOG_MARK; a FLOWPLAN from before the marker starts its log at the first "──" line.
        let all: Vec<&str> = text.lines().collect();
        let start = all
            .iter()
            .position(|l| l.trim_end() == crate::flow::LOG_MARK)
            .map(|i| i + 1)
            .or_else(|| all.iter().position(|l| l.starts_with("──")));
        let start = start
            .unwrap_or(all.len())
            .max(all.len().saturating_sub(LOG_TAIL));
        lines.extend(
            all[start..]
                .iter()
                .map(|l| Line::styled(l.to_string(), Style::new().fg(pal().overlay0))),
        );
        Some(lines)
    }

    /// Re-flattens the tree (after a refresh or a fold), keeping the selected node.
    fn reflow(&mut self) {
        let keep = self
            .rows
            .get(self.sel)
            .map(|r| (r.space.clone(), r.path.clone()));
        let mut rows = Vec::new();
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
            if let Some(r) = self.rows.first() {
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
    fn activate(&mut self, i: usize, arrow: bool) {
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

    /// Opens registry agent (thread, agent) as a tab in brigd thread `key`'s focused pane
    /// (or focuses the pane that already has it).
    fn open_agent(&mut self, key: String, thread: &str, agent: &str, stage: Option<usize>) {
        let p = self.tabs.entry(key.clone()).or_default();
        let found = p.find(
            |t| matches!(t, Tab::Term { thread: t, agent: a, .. } if t == thread && a == agent),
        );
        if let Some((j, _)) = found {
            p.focus = j;
        }
        if let Some((j, i)) = found
            .filter(|&(j, i)| matches!(&p.list[j].list[i], Tab::Term { live, .. } if live.alive()))
        {
            p.list[j].active = i;
            self.focus_main = true;
            return;
        }
        // Not live (brigd restarted, or it exited): reopen its session.
        match runner::resume_agent(thread, agent) {
            Ok(live) => {
                let b = self.pane_body(&key);
                let _ = live.resize(b.height.max(1), b.width.max(1));
                let tab = Tab::Term {
                    thread: thread.into(),
                    agent: agent.into(),
                    stage,
                    live,
                };
                let p = self.tabs.entry(key).or_default();
                let tabs = &mut p.list[p.focus];
                match found {
                    Some((_, i)) => (tabs.list[i], tabs.active) = (tab, i),
                    None => push_tab(tabs, tab),
                }
                self.focus_main = true;
            }
            Err(e) => self.flash(format!("{thread}/{agent}: {e}")),
        }
    }

    /// Opens the login shell as a new tab in the shown thread, in its space. `claude` typed
    /// in it becomes a stray agent of that thread: the shell gets $BRIGD_THREAD and the
    /// ~/.brigd/bin shim first on PATH.
    fn new_term(&mut self) {
        let space = self.space.clone();
        let brigd_thread = Some(self.thread.clone()).filter(|t| !t.is_empty());
        let cwd = if space.as_os_str().is_empty() {
            env::current_dir().unwrap_or_default()
        } else {
            space.clone()
        };
        // Registry key (space, name): names are unique per space, and a closed-but-running tab keeps its name.
        let thread = space.to_string_lossy().into_owned();
        let reg = runner::registry().lock().unwrap().clone();
        let taken = self
            .tabs
            .values()
            .flat_map(|p| &p.list)
            .flat_map(|t| &t.list)
            .filter_map(|t| match t {
                Tab::Term {
                    thread: t, agent, ..
                } if *t == thread => Some(agent.as_str()),
                _ => None,
            });
        // ponytail: names of every live shell are taken too, so the terminal folder can find a shell by thread + name
        let is_shell = |k: &(String, String)| k.0 == thread || self.shell_thread.contains_key(k);
        let name = fresh_name(
            taken.chain(
                reg.iter()
                    .filter(|(k, a)| is_shell(k) && a.alive())
                    .map(|(k, _)| k.1.as_str()),
            ),
        );
        // No dir: a shell has no hooks/status/session files (status() reads as "starting", never shown).
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = portable_pty::CommandBuilder::new(&shell);
        if let Some(t) = &brigd_thread {
            cmd.env("BRIGD_THREAD", t);
            cmd.env("BRIGD_TERM", &name); // stray() records it, so the terminal row can show this shell's claude
        }
        if let (Ok(bin), Ok(shim)) = (env::current_exe(), crate::stray::ensure_shim()) {
            let path = env::var_os("PATH").unwrap_or_default();
            if let Ok(p) =
                env::join_paths([shim.clone()].into_iter().chain(env::split_paths(&path)))
            {
                cmd.env("PATH", p);
            }
            cmd.env("BRIGD_BIN", bin);
            if shell.ends_with("zsh") {
                // zsh rcs often prepend PATH; the wrapper rc sources the user's, then puts the shim first again.
                cmd.env("BRIGD_SHIM", &shim);
                cmd.env("BRIGD_ZDOTDIR", env::var_os("ZDOTDIR").unwrap_or_default());
                cmd.env("ZDOTDIR", crate::store::root().join("zsh"));
            }
        }
        match LiveAgent::spawn(cmd, &thread, &name, &cwd, Path::new("")).map(runner::register) {
            Ok(live) => {
                let b = self.pane_body(&self.thread);
                let _ = live.resize(b.height.max(1), b.width.max(1));
                if let Some(t) = brigd_thread {
                    self.shell_thread.insert((thread.clone(), name.clone()), t);
                }
                push_tab(
                    self.pane(),
                    Tab::Term {
                        thread,
                        agent: name,
                        stage: None,
                        live,
                    },
                );
                self.focus_main = true;
            }
            Err(e) => self.flash(format!("{name}: {e}")),
        }
    }

    /// Selects and opens a thread's FLOWPLAN viewer.
    fn open_flowplan(&mut self, thread: &str) {
        let plan = crate::store::thread_dir(thread).join("FLOWPLAN");
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| r.kind == Kind::Output && r.path == plan)
        {
            self.sel = i;
            self.activate(i, false);
        }
    }

    fn open_view(&mut self, thread: String, path: PathBuf) {
        let p = self.tabs.entry(thread).or_default();
        if let Some((j, i)) = p.find(|t| matches!(t, Tab::View { path: p, .. } if *p == path)) {
            (p.focus, p.list[j].active) = (j, i);
        } else {
            let title = path
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = match path.file_name().and_then(|f| f.to_str()) {
                Some("FLOWPLAN") => format!("{title} FLOWPLAN"),
                Some("flow.md") => format!("{title} flow"),
                _ => title,
            };
            let tab = match crate::diffs::diff_view(&path) {
                Some((rel, text)) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let agent = path
                        .ancestors()
                        .find(|a| a.ends_with(".diff"))
                        .and_then(|a| a.parent()?.file_name())
                        .unwrap_or_default()
                        .to_string_lossy();
                    Tab::View {
                        title: format!("± {agent} {name}"),
                        text,
                        scroll: 0,
                        rel: Some(rel),
                        path,
                    }
                }
                None => {
                    let text = fs::read_to_string(&path).unwrap_or_else(|e| format!("({e})"));
                    Tab::View {
                        path,
                        title: format!("≡ {title}"),
                        text,
                        scroll: 0,
                        rel: None,
                    }
                }
            };
            push_tab(&mut p.list[p.focus], tab);
            self.load_plans();
        }
        self.focus_main = true;
    }

    /// The shown thread's focused pane.
    fn pane(&mut self) -> &mut Tabs {
        let p = self.tabs.entry(self.thread.clone()).or_default();
        &mut p.list[p.focus]
    }

    /// Thread `key`'s focused pane body as the next draw lays it out, so a new PTY starts at its real size.
    fn pane_body(&self, key: &str) -> Rect {
        let (n, focus) = self
            .tabs
            .get(key)
            .map_or((1, 0), |p| (p.list.len(), p.focus));
        pane_rects(self.main, n)[focus].1
    }

    fn cur(&mut self) -> Option<&mut Tab> {
        let p = self.tabs.get_mut(&self.thread)?;
        let t = &mut p.list[p.focus];
        t.list.get_mut(t.active)
    }

    fn cycle(&mut self, d: isize) {
        let t = self.pane();
        if !t.list.is_empty() {
            t.active = (t.active as isize + d).rem_euclid(t.list.len() as isize) as usize;
            self.focus_main = true;
        }
    }

    /// Closes the current tab (its agent keeps running), and its pane once empty unless it is the only one.
    fn close_tab(&mut self) {
        let Some(p) = self.tabs.get_mut(&self.thread) else {
            return;
        };
        let t = &mut p.list[p.focus];
        if !t.list.is_empty() {
            t.list.remove(t.active);
            t.active = t.active.min(t.list.len().saturating_sub(1));
        }
        if t.list.is_empty() && p.list.len() > 1 {
            p.list.remove(p.focus);
            p.focus = 0;
        }
        self.focus_main = !p.list[p.focus].list.is_empty();
    }

    /// A second pane for the shown thread, holding only a new terminal.
    fn split(&mut self) {
        let p = self.tabs.entry(self.thread.clone()).or_default();
        if p.list.len() >= 2 {
            return self.flash("max 2 panes");
        }
        p.list.push(Tabs::default());
        p.focus = 1;
        self.new_term();
        if self.cur().is_none() {
            self.close_tab(); // the shell failed to start: drop the empty pane
        }
    }

    fn other_pane(&mut self) {
        if let Some(p) = self.tabs.get_mut(&self.thread) {
            p.focus = (p.focus + 1) % p.list.len();
            self.focus_main = !p.list[p.focus].list.is_empty();
        }
    }

    /// Sizes every live agent not on screen to a one-pane body (draw sizes the shown ones), so panes don't fight over SIGWINCH.
    fn resize_all(&self) {
        let body = pane_rects(self.main, 1)[0].1;
        if body.width == 0 || body.height == 0 {
            return;
        }
        let shown: Vec<&Arc<LiveAgent>> = self
            .tabs
            .get(&self.thread)
            .into_iter()
            .flat_map(|p| &p.list)
            .filter_map(|t| match t.list.get(t.active) {
                Some(Tab::Term { live, .. }) => Some(live),
                _ => None,
            })
            .collect();
        for a in live_agents()
            .into_iter()
            .filter(|a| !shown.iter().any(|s| Arc::ptr_eq(s, a)))
        {
            if a.parser.lock().unwrap().screen().size() != (body.height, body.width) {
                let _ = a.resize(body.height, body.width);
            }
        }
    }

    fn ask_quit(&mut self) {
        if live_agents().is_empty() {
            self.quit = true;
        } else {
            self.confirm_quit = true;
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        let Some(Tab::Term { live, .. }) = self.cur() else {
            return;
        };
        let live = live.clone();
        {
            let mut p = live.parser.lock().unwrap();
            if p.screen().scrollback() > 0 {
                p.screen_mut().set_scrollback(0);
            }
        }
        if let Err(e) = live.write(bytes) {
            self.flash(format!("{}: {e}", live.agent));
        }
    }

    /// PAUSE: ESC to every live agent of the thread (flow agents, and the shell tabs strays
    /// run in). RESUME: "continue" + Enter to the same set.
    fn toggle_pause(&mut self, thread: &str) {
        let resume = self.paused.contains(thread);
        let strays = tree::stray_dirs(&crate::store::thread_dir(thread))
            .iter()
            .any(|d| !matches!(stray_status(d), "off" | "done" | "failed"));
        let mut targets = live_agents()
            .into_iter()
            .filter(|a| a.thread == thread)
            .collect::<Vec<_>>();
        if strays {
            // ponytail: a shell tab is matched to the thread by $BRIGD_THREAD, not to the exact claude inside it
            targets.extend(
                self.live_shells()
                    .into_iter()
                    .filter(|(_, t)| t == thread)
                    .map(|(a, _)| a),
            );
        }
        let bytes: &[u8] = if resume { b"continue\r" } else { b"\x1b" };
        targets.iter().for_each(|a| {
            let _ = a.write(bytes);
        });
        if resume {
            self.paused.remove(thread);
        } else {
            self.paused.insert(thread.into());
        }
    }

    /// Live shell tabs, each with the thread its $BRIGD_THREAD names.
    fn live_shells(&self) -> Vec<(Arc<LiveAgent>, String)> {
        let reg = runner::registry().lock().unwrap().clone();
        reg.iter()
            .filter(|(_, a)| a.alive())
            .filter_map(|(k, a)| Some((a.clone(), self.shell_thread.get(k)?.clone())))
            .collect()
    }

    /// Kills the agent, reverts its changes, keeps its output (crate::engine::terminate), and says how it went.
    fn terminate(&mut self, thread: &str, agent: &str) {
        match crate::engine::terminate(thread, agent) {
            Ok(n) => self.flash(format!(
                "terminated {agent}: reverted {n} file(s), output kept"
            )),
            Err(e) => self.flash(format!("terminate {agent}: {e}")),
        }
        self.refresh();
    }

    fn pick(&mut self, act: Act) {
        match act {
            Act::Terminate { thread, agent } => self.terminate(&thread, &agent),
            Act::Delete(t) => self.confirm_delete = Some(t),
            Act::Archive(t) => self.archive(&t),
        }
    }

    /// Stops the thread's agents, strays and terminals and closes its tabs (Delete / Archive come next).
    fn stop_thread(&mut self, thread: &str) {
        crate::engine::stop_thread(thread);
        self.live_shells()
            .into_iter()
            .filter(|(_, t)| t == thread)
            .for_each(|(a, _)| a.kill());
        self.tabs.remove(thread);
        if self.thread == thread {
            self.thread.clear(); // not a thread any more: reflow re-picks the first one
            self.reflow();
        }
        self.focus_main &= self.cur().is_some();
        self.paused.remove(thread);
    }

    fn open_newt(&mut self, repo: PathBuf) {
        // A hidden box still planning is tracked in `newt`: replacing it would lose its inputs on failure.
        if let Some(n) = self.newt.as_mut().filter(|n| n.planning) {
            n.hidden = false;
        } else {
            self.newt = Some(Newt {
                fields: [repo.display().to_string(), String::new(), String::new()],
                focus: 1,
                ..Default::default()
            });
        }
    }

    /// The window with the dir field focused, for a thread in any directory.
    fn open_newt_anywhere(&mut self, repo: PathBuf) {
        self.open_newt(repo);
        if let Some(n) = self.newt.as_mut().filter(|n| !n.planning) {
            n.focus = 0;
        }
    }

    /// Where a new thread goes by default: the highlighted row's space, else the shown one, else the cwd.
    fn new_dir(&self) -> PathBuf {
        let space = self.rows.get(self.sel).map_or(&self.space, |r| &r.space);
        if space.as_os_str().is_empty() {
            env::current_dir().unwrap_or_default()
        } else {
            space.clone()
        }
    }

    fn archive(&mut self, thread: &str) {
        let to = crate::store::archive_dir(&crate::store::root(), thread);
        if to.exists() {
            return self.flash(format!("archive {thread}: {} already exists", to.display()));
        }
        self.stop_thread(thread);
        match crate::store::archive_thread(&crate::store::root(), thread) {
            Ok(to) => self.flash(format!(
                "archived {thread} to {} (its worktrees stay put)",
                to.display()
            )),
            Err(e) => self.flash(format!("archive {thread}: {e}")),
        }
        self.refresh();
    }

    fn delete(&mut self, thread: &str) {
        self.stop_thread(thread);
        match crate::store::delete_thread(&crate::store::root(), thread) {
            Ok(()) => self.flash(format!("deleted {thread}")),
            Err(e) => self.flash(format!("delete {thread}: {e}")),
        }
        self.refresh();
    }

    fn tx(&mut self) -> mpsc::Sender<Bg> {
        self.bg.get_or_insert_with(mpsc::channel).0.clone()
    }

    /// The dir field as a path: `~` expands to $HOME.
    fn newt_path(typed: &str) -> PathBuf {
        match (typed, typed.strip_prefix("~/"), env::var_os("HOME")) {
            ("~", _, Some(h)) => PathBuf::from(h),
            (_, Some(rest), Some(h)) => PathBuf::from(h).join(rest),
            _ => PathBuf::from(typed),
        }
    }

    /// The first failing check of the new-thread fields (focus index, message); reserves nothing.
    fn newt_check(fields: &[String; 3]) -> Option<(usize, String)> {
        let typed = fields[0].trim();
        match fs::canonicalize(Self::newt_path(typed))
            .ok()
            .filter(|p| p.is_dir())
        {
            None => Some((0, format!("no such directory: {typed}"))),
            Some(_) => crate::store::check_new_thread(fields[1].trim())
                .err()
                .map(|e| (1, e.to_string()))
                .or_else(|| {
                    fields[2]
                        .trim()
                        .is_empty()
                        .then(|| (2, "task is empty".into()))
                }),
        }
    }

    /// Enter in the new-thread window: resolves the dir, reserves the name, then plans off the UI thread.
    /// `prg`: the user answered yes to "Use plan-review-go?".
    fn submit_newt(&mut self, fields: [String; 3], prg: bool) {
        let path = Self::newt_path(fields[0].trim());
        let (name, mut task) = (fields[1].trim().to_string(), fields[2].trim().to_string());
        if prg {
            task = crate::with_prg(&task);
        }
        let dir = crate::store::thread_dir(&name);
        // Only the first failing check shows; create_dir reserves the name and tree::build hides
        // the dir until flowmap.json exists.
        let failed = Self::newt_check(&fields).or_else(|| {
            fs::create_dir_all(crate::store::root().join("threads"))
                .and_then(|_| fs::create_dir(&dir))
                .err()
                .map(|e| (1, e.to_string()))
        });
        if let Some((focus, err)) = failed {
            self.newt = Some(Newt {
                fields,
                focus,
                err: Some(err),
                ..Default::default()
            });
            return;
        }
        let repo = fs::canonicalize(&path).unwrap_or(path);
        self.newt = Some(Newt {
            fields: [repo.display().to_string(), name.clone(), task.clone()],
            focus: 2,
            planning: true,
            ..Default::default()
        });
        self.planning.insert(name.clone());
        self.flash(format!("planning {name}…"));
        let tx = self.tx();
        // ponytail: no cancel; quitting brigd mid-plan orphans the `claude -p` planner.
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| {
                crate::flow::plan(&task, &repo).and_then(|flow| {
                    crate::store::create_thread(
                        &crate::store::root(),
                        &name,
                        &repo.to_string_lossy(),
                        &flow,
                    )?;
                    Ok(fs::write(
                        dir.join("FLOWPLAN"),
                        crate::flow::flow_text(&flow),
                    )?)
                })
            })
            .unwrap_or_else(|_| Err("planner panicked".into()));
            let _ = tx.send(match r {
                Ok(()) => Bg::Planned(name, repo.to_string_lossy().into_owned()),
                Err(e) => {
                    let _ = fs::remove_dir_all(&dir);
                    Bg::Failed(name, e.to_string())
                }
            });
        });
    }

    fn on_bg(&mut self, m: Bg) {
        match m {
            Bg::Failed(name, e) => {
                self.planning.remove(&name);
                match &mut self.newt {
                    Some(n) if n.planning && n.fields[1] == name => {
                        (n.planning, n.hidden, n.err) = (false, false, Some(e))
                    }
                    _ => self.flash(format!("plan {name}: {e}")),
                }
            }
            Bg::Planned(name, repo) => {
                self.planning.remove(&name);
                if self
                    .newt
                    .as_ref()
                    .is_some_and(|n| n.planning && n.fields[1] == name)
                {
                    self.newt = None;
                }
                self.refresh();
                self.open_flowplan(&name);
                if self.confirm_run.is_some() {
                    self.queued.push((name, repo));
                } else {
                    self.confirm_run = Some((name, repo));
                }
            }
            Bg::Finished(name, Ok(())) => self.flash(format!("thread {name} done")),
            Bg::Finished(name, Err(e)) => self.flash(format!("thread {name}: {e}")),
        }
    }

    /// Runs threads `brigd resume --bg` queued, if this TUI owns tui.pid. Takes over
    /// tui.pid when its owner is gone, so one open TUI always answers.
    fn claim_queued(&mut self) {
        let me = std::process::id();
        let owner = crate::launch::tui_pid() == Some(me)
            || (!crate::launch::tui_alive()
                && fs::write(crate::launch::tui_pid_path(), me.to_string()).is_ok());
        let queued: Vec<String> = self
            .thread_status
            .iter()
            .filter_map(|(p, st)| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                crate::engine::claimable(st, owner, crate::engine::executing(&name)).then_some(name)
            })
            .collect();
        for name in queued {
            // Claim on disk before spawning so the next tick never sees it queued again.
            // The status cache may be a tick old: only claim what is still queued on disk.
            let claimed = crate::store::load(&name).and_then(|(_, mut st)| {
                if st.status != "queued" {
                    return Ok(false);
                }
                st.status = "running".into();
                st.updated = crate::store::now(); // reconcile treats a stale `updated` as a dead run
                crate::store::write_json(&crate::store::thread_dir(&name).join("state.json"), &st)
                    .map(|_| true)
            });
            match claimed {
                Ok(false) => {}
                Ok(true) => {
                    self.flash(format!("running queued thread {name}"));
                    self.run_planned(name);
                }
                Err(e) => self.flash(format!("thread {name}: {e}")),
            }
        }
    }

    /// Fails running threads whose claude is gone (see `crate::engine::reconcile`).
    fn reconcile_running(&self) {
        for name in self
            .thread_status
            .iter()
            .filter(|(_, st)| *st == "running")
            .filter_map(|(p, _)| Some(p.file_name()?.to_string_lossy().into_owned()))
        {
            if let Ok((_, mut st)) = crate::store::load(&name) {
                let _ = crate::engine::reconcile(&name, &mut st);
            }
        }
    }

    /// y on a planned thread: the same execute as --flow / resume, off the UI thread.
    fn run_planned(&mut self, name: String) {
        match crate::store::load(&name) {
            Ok((flow, state)) => {
                let tx = self.tx();
                std::thread::spawn(move || {
                    let r = crate::engine::execute(&name, &flow, state).map_err(|e| e.to_string());
                    let _ = tx.send(Bg::Finished(name, r));
                });
            }
            Err(e) => self.flash(e.to_string()),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        // A modal that opened while a prefix was armed must not leave it armed behind it.
        if self.newt.as_ref().is_some_and(|n| !n.hidden)
            || self.confirm_run.is_some()
            || self.menu.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_quit
        {
            (self.prefix, self.thread_prefix) = (false, false);
        }
        // The new-thread window and its run confirm come first: no key may reach anything behind them.
        if let Some(n) = self.newt.as_mut().filter(|n| !n.hidden) {
            if n.planning {
                // Read-only while planning; Esc only hides the box.
                n.hidden = k.code == KeyCode::Esc;
                return;
            }
            n.err = None;
            if n.ask {
                match k.code {
                    KeyCode::Esc => n.ask = false,
                    KeyCode::Char('y' | 'Y') | KeyCode::Char('n' | 'N') | KeyCode::Enter => {
                        let yes = matches!(k.code, KeyCode::Char('y' | 'Y'));
                        let n = self.newt.take().unwrap();
                        self.submit_newt(n.fields, yes);
                    }
                    _ => {}
                }
                return;
            }
            match k.code {
                KeyCode::Esc => self.newt = None,
                KeyCode::Tab | KeyCode::Down => n.focus = (n.focus + 1) % 3,
                KeyCode::Up => n.focus = (n.focus + 2) % 3,
                KeyCode::Enter if n.focus < 2 => n.focus += 1,
                KeyCode::Enter => {
                    if !crate::mentions_prg(&n.fields[2]) && Self::newt_check(&n.fields).is_none() {
                        n.ask = true;
                    } else {
                        let n = self.newt.take().unwrap();
                        self.submit_newt(n.fields, false);
                    }
                }
                KeyCode::Backspace => _ = n.fields[n.focus].pop(),
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    n.fields[n.focus].clear()
                }
                KeyCode::Char(c)
                    if !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    n.fields[n.focus].push(c)
                }
                _ => {}
            }
            return;
        }
        if let Some((name, repo)) = self.confirm_run.take() {
            match k.code {
                KeyCode::Char('y' | 'Y') => self.run_planned(name),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.delete(&name),
                _ => {
                    self.confirm_run = Some((name, repo));
                    return;
                }
            }
            self.confirm_run = self.queued.pop();
            return;
        }
        if let Some(mut m) = self.menu.take() {
            // ↑↓ move, Enter picks; Esc or any other key just closes the menu.
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => m.sel = m.sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => m.sel = (m.sel + 1).min(m.items.len() - 1),
                KeyCode::Enter => return self.pick(m.items.swap_remove(m.sel)),
                _ => return,
            }
            self.menu = Some(m);
            return;
        }
        if let Some(t) = self.confirm_delete.take() {
            if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                self.delete(&t);
            }
            return;
        }
        let ctrl_o = k.code == KeyCode::Char('o') && k.modifiers.contains(KeyModifiers::CONTROL);
        if self.confirm_quit {
            self.confirm_quit = false;
            self.quit = matches!(k.code, KeyCode::Char('y' | 'Y'));
            return;
        }
        if self.prefix {
            self.prefix = false;
            match k.code {
                _ if ctrl_o => self.send(&[0x0f]), // Ctrl-o twice sends one to claude
                KeyCode::Char('s') => self.focus_main = false,
                KeyCode::Char('n') => self.cycle(1),
                KeyCode::Char('p') => self.cycle(-1),
                KeyCode::Char('w') => self.close_tab(),
                KeyCode::Char('t') => self.new_term(),
                KeyCode::Char('v') => self.split(),
                KeyCode::Char('o') => self.other_pane(),
                KeyCode::Char('c') => {
                    self.msg = Some((format!("theme: {}", next_theme()), Instant::now()))
                }
                KeyCode::Char('q') => self.ask_quit(),
                _ => {}
            }
            return;
        }
        if ctrl_o {
            (self.prefix, self.thread_prefix) = (true, false);
            return;
        }
        let ctrl_t = k.code == KeyCode::Char('t') && k.modifiers.contains(KeyModifiers::CONTROL);
        if self.thread_prefix {
            self.thread_prefix = false;
            let row = self.rows.get(self.sel);
            let (thread, repo) = (
                row.map(|r| r.thread.clone()).filter(|t| !t.is_empty()),
                self.new_dir(),
            );
            match (k.code, thread) {
                _ if ctrl_t => self.send(&[0x14]), // Ctrl-t twice sends one to claude
                (KeyCode::Char('d'), Some(t)) => self.confirm_delete = Some(t),
                (KeyCode::Char('a'), Some(t)) => self.archive(&t),
                (KeyCode::Char('d' | 'a'), None) => self.flash("no thread highlighted"),
                (KeyCode::Char('n'), _) => self.open_newt(repo),
                (KeyCode::Char('m'), _) => self.open_newt_anywhere(repo),
                _ => {}
            }
            return;
        }
        if ctrl_t {
            self.thread_prefix = true;
            return;
        }
        // Alt-h/l/t from anywhere; '˙'/'¬'/'†' are what macOS Option-h/l/t types without "Option as Meta".
        match (k.code, k.modifiers.contains(KeyModifiers::ALT)) {
            (KeyCode::Char('h'), true) | (KeyCode::Char('˙'), _) => return self.cycle(-1),
            (KeyCode::Char('l'), true) | (KeyCode::Char('¬'), _) => return self.cycle(1),
            (KeyCode::Char('t'), true) | (KeyCode::Char('†'), _) => return self.new_term(),
            _ => {}
        }
        if !self.focus_main {
            return self.side_key(k);
        }
        match self.cur() {
            Some(Tab::Term { live, .. }) => {
                let app_cursor = live.parser.lock().unwrap().screen().application_cursor();
                self.send(&key_bytes(k, app_cursor));
            }
            Some(Tab::View { scroll, .. }) => {
                let d = match k.code {
                    KeyCode::Up | KeyCode::Char('k') => -1,
                    KeyCode::Down | KeyCode::Char('j') => 1,
                    KeyCode::PageUp => -20,
                    KeyCode::PageDown | KeyCode::Char(' ') => 20,
                    KeyCode::Home | KeyCode::Char('g') => -i32::MAX / 2,
                    KeyCode::End | KeyCode::Char('G') => i32::MAX / 2,
                    KeyCode::Tab | KeyCode::Esc => return self.focus_main = false,
                    _ => 0,
                };
                *scroll = (*scroll as i32 + d).clamp(0, u16::MAX as i32) as u16;
                // draw clamps to the rendered line count
            }
            None => self.focus_main = false,
        }
    }

    fn side_key(&mut self, k: KeyEvent) {
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
            KeyCode::Tab => self.focus_main = self.cur().is_some(),
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if self.newt.as_ref().is_some_and(|n| !n.hidden) || self.confirm_run.is_some() {
            return;
        }
        let at = Position {
            x: m.column,
            y: m.row,
        };
        if matches!(m.kind, MouseEventKind::Down(_)) {
            (self.prefix, self.thread_prefix) = (false, false);
            if let Some(menu) = self.menu.take() {
                // A left click on an item picks it; any other click closes the menu (a right
                // click on another row then opens that row's menu below).
                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    if menu.rect.contains(at) {
                        self.pick(menu.items[(m.row - menu.rect.y) as usize].clone());
                    }
                    return;
                }
            }
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let Some((i, _)) = hit(self.side_inner, self.offset, &self.rows, m.column, m.row)
                else {
                    return;
                };
                let r = &self.rows[i];
                let items = match r.kind {
                    Kind::Agent => vec![Act::Terminate {
                        thread: r.thread.clone(),
                        agent: tree::agent_id(&r.path, &r.label),
                    }],
                    Kind::Thread => {
                        vec![Act::Delete(r.label.clone()), Act::Archive(r.label.clone())]
                    }
                    _ => return,
                };
                self.sel = i;
                let (w, h) = (
                    items.iter().map(|a| a.label().len()).max().unwrap_or(0) as u16,
                    items.len() as u16,
                );
                let x = if self.screen.width == 0 {
                    m.column
                } else {
                    m.column.min(self.screen.right().saturating_sub(w))
                };
                let y = if self.screen.height == 0 {
                    m.row
                } else {
                    m.row.min(self.screen.bottom().saturating_sub(h))
                };
                self.menu = Some(Menu {
                    rect: Rect::new(x, y, w, h),
                    items,
                    sel: 0,
                });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.resizing = self.divider.contains(at);
                // The sidebar is not among these, so a drag starting there selects nothing.
                let panes = self
                    .tabs
                    .get(&self.thread)
                    .into_iter()
                    .flat_map(|p| &p.list)
                    .flat_map(|t| [t.bar, t.body]);
                self.drag = panes
                    .chain([self.hint])
                    .find(|r| r.contains(at))
                    .map(|r| (r, at, None));
                if let Some(repo) = self
                    .plus_btns
                    .iter()
                    .find(|(r, _)| r.contains(at))
                    .map(|(_, p)| p.clone())
                {
                    (self.prefix, self.thread_prefix) = (false, false);
                    if self.header_plus.contains(at) {
                        self.open_newt_anywhere(repo);
                    } else {
                        self.open_newt(repo);
                    }
                } else if let Some(t) = self
                    .pause_btns
                    .iter()
                    .find(|(r, _)| r.contains(at))
                    .map(|(_, t)| t.clone())
                {
                    self.toggle_pause(&t);
                } else if let Some(k) = self
                    .task_btns
                    .iter()
                    .find(|(r, _)| r.contains(at))
                    .map(|(_, k)| k.clone())
                {
                    if !self.expanded.remove(&k) {
                        self.expanded.insert(k);
                    }
                } else if let Some((i, arrow)) =
                    hit(self.side_inner, self.offset, &self.rows, m.column, m.row)
                {
                    self.sel = i;
                    self.activate(i, arrow);
                    self.focus_main = false; // a click shows the item but keeps j/k on the sidebar
                } else if let Some(p) = self.tabs.get_mut(&self.thread) {
                    // A click on a pane focuses it; on one of its tabs, also shows that tab.
                    if let Some(j) = p
                        .list
                        .iter()
                        .position(|t| t.bar.contains(at) || t.body.contains(at))
                    {
                        let t = &mut p.list[j];
                        // (previously active tab, clicked tab) when the click is on the "× ".
                        let mut close = None;
                        if let Some(&(_, x1, i)) = t
                            .spans
                            .iter()
                            .find(|&&(a, b, _)| t.bar.contains(at) && (a..b).contains(&m.column))
                        {
                            if m.column + 2 >= x1 {
                                close = Some((t.active, i));
                            }
                            t.active = i;
                        }
                        if !t.list.is_empty() {
                            (p.focus, self.focus_main) = (j, true);
                        }
                        if let Some((prev, i)) = close {
                            self.close_tab();
                            // A neighbour closed: keep the tab that was showing (i == prev picks its own successor).
                            let t = self.pane();
                            if i != prev {
                                t.active = prev - (i < prev) as usize;
                            }
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.resizing {
                    self.side_w = Some(
                        m.column
                            .saturating_sub(self.side.x)
                            .clamp(MIN_SIDE_W, SIDE_W),
                    );
                } else if let Some(d) = &mut self.drag {
                    d.2 = Some(at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.resizing = false;
                if let Some((r, a, Some(b))) = self.drag.take() {
                    let t = sel_text(&self.buf, r, a, b);
                    if !t.trim().is_empty() {
                        self.flash(format!("copied {} chars", t.chars().count()));
                        self.clip = Some(t);
                    }
                }
            }
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                let back = matches!(
                    m.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft
                );
                let d: i32 = if back { -3 } else { 3 };
                if self.side.contains(at) {
                    let max = max_offset(&self.rows, self.side_inner.height as usize) as i32;
                    self.offset = (self.offset as i32 + d).clamp(0, max.max(0)) as usize;
                } else if let Some(t) = self
                    .tabs
                    .get_mut(&self.thread)
                    .and_then(|p| p.list.iter_mut().find(|t| t.body.contains(at)))
                {
                    match t.list.get_mut(t.active) {
                        Some(Tab::Term { live, .. }) => {
                            let mut p = live.parser.lock().unwrap();
                            let s = p.screen().scrollback() as i32;
                            p.screen_mut().set_scrollback((s - d).max(0) as usize);
                            // wheel up = further back
                        }
                        Some(Tab::View { scroll, .. }) => {
                            *scroll = (*scroll as i32 + d).clamp(0, u16::MAX as i32) as u16
                        }
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn on_paste(&mut self, s: &str) {
        if let Some(n) = self.newt.as_mut().filter(|n| !n.hidden) {
            if !n.planning && !n.ask {
                n.fields[n.focus].push_str(&s.replace(['\r', '\n'], " "));
            }
            return;
        }
        if self.confirm_run.is_some() || !self.focus_main {
            return;
        }
        let Some(Tab::Term { live, .. }) = self.cur() else {
            return;
        };
        let bracketed = live.parser.lock().unwrap().screen().bracketed_paste();
        let bytes = if bracketed {
            format!("\x1b[200~{s}\x1b[201~")
        } else {
            s.replace('\n', "\r")
        };
        self.send(bytes.as_bytes());
    }

    fn row_line(&self, r: &Row, selected: bool, width: usize) -> Line<'static> {
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
                    .copied()
                    .unwrap_or(if r.kind == Kind::Terminal { "$" } else { "off" });
                let (g, style) = match st {
                    "$" => ("$ ", Style::new().fg(pal().green)),
                    "running" => ("● ", Style::new().fg(pal().green)),
                    "idle" => ("◦ ", Style::new().fg(pal().green)),
                    "blocked" => (
                        "! ",
                        Style::new()
                            .fg(pal().red)
                            .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                    ),
                    "done" => ("✓ ", Style::new().fg(pal().blue)),
                    "failed" => ("✗ ", Style::new().fg(pal().red)),
                    _ => ("○ ", Style::new().fg(pal().overlay0)),
                };
                label = if st == "blocked" {
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
            let st = self
                .thread_status
                .get(&r.path)
                .map(String::as_str)
                .unwrap_or("");
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

    /// Pane `j` of the shown thread: its tab bar (the thread name first on pane 0), then the active tab.
    fn draw_pane(&mut self, f: &mut Frame, j: usize, bar: Rect, body: Rect) {
        let p = &self.tabs[&self.thread];
        let (tabs, focused) = (&p.list[j], p.focus == j);
        let mut spans = vec![];
        let pill = if j == 0 {
            spans.push(Span::styled(
                format!(" {} ", self.thread),
                Style::new()
                    .fg(pal().crust)
                    .bg(pal().subtext0)
                    .add_modifier(Modifier::BOLD),
            ));
            self.thread.chars().count() as u16 + 2
        } else {
            0
        };
        let mut titles = vec![];
        for t in &tabs.list {
            titles.push(match t {
                Tab::Term {
                    agent, live, stage, ..
                } if !live.alive() => (
                    format!(" {} (exited) ", tail(agent)),
                    Some(stage_color(*stage)),
                ),
                Tab::Term { agent, stage, .. } => {
                    (format!(" {} ", tail(agent)), Some(stage_color(*stage)))
                }
                Tab::View { title, .. } => (format!(" {title} "), None),
            });
        }
        let widths: Vec<u16> = titles
            .iter()
            .map(|(t, _)| 3 + t.chars().count() as u16 + 2)
            .collect(); // 3 fade cells + title + "× "
        let (first, end) = tab_window(
            &widths,
            0,
            Some(tabs.active),
            bar.width.saturating_sub(pill),
        );
        let mut tab_spans = vec![];
        let marker = |c| Span::styled(c, Style::new().fg(pal().overlay0));
        if first > 0 {
            spans.push(marker("‹"));
        }
        let mut x = bar.x + pill + (first > 0) as u16;
        // ponytail: × is East Asian ambiguous width; a CJK-width terminal draws it 2 wide and the tab bar skews.
        for i in first..end {
            let (title, fg) = &titles[i];
            // Views, shells and strays have no stage color.
            let color = fg.filter(|&c| c != Color::Reset).unwrap_or(pal().lavender);
            let active = tabs.active == i;
            // The focused pane's active tab is filled, the other pane's underlined.
            let (text, bg, mods) = match (active, focused) {
                (true, true) => (pal().crust, color, Modifier::BOLD),
                (true, false) => (color, pal().surface0, Modifier::UNDERLINED | Modifier::BOLD),
                _ => (color, pal().surface0, Modifier::empty()),
            };
            tab_spans.push((x, x + widths[i], i));
            x += widths[i];
            // Smooth left fade: three blank cells whose bg steps from the page bg to the tab bg.
            for k in 1..=3u16 {
                let c = match (pal().mantle, bg) {
                    (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => {
                        let m = |a: u8, b: u8| ((a as u16 * (4 - k) + b as u16 * k) / 4) as u8;
                        Color::Rgb(m(r0, r1), m(g0, g1), m(b0, b1))
                    }
                    _ => bg,
                };
                spans.push(Span::styled(" ", Style::new().bg(c)));
            }
            spans.push(Span::styled(
                format!("{title}× "),
                Style::new().fg(text).bg(bg).add_modifier(mods),
            ));
        }
        if end < titles.len() {
            spans.push(marker("›"));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), bar);

        let cursor = self.focus_main && focused;
        let pad = Rect::new(
            body.x + 2,
            body.y + 1,
            body.width.saturating_sub(2),
            body.height.saturating_sub(1),
        );
        let mut hits = vec![];
        let mut plan = tabs.list.get(tabs.active).and_then(|t| match t {
            Tab::View {
                path,
                text,
                rel: None,
                ..
            } => self
                .plan_view(path, text, pad.width as usize, &mut hits)
                .or_else(|| {
                    path.ends_with("flow.md")
                        .then(|| flow_lines(text))
                        .flatten()
                }),
            _ => None,
        });
        let plan_path = tabs.list.get(tabs.active).and_then(|t| match t {
            Tab::View { path, .. } => Some(path.clone()),
            _ => None,
        });
        let t = &mut self.tabs.get_mut(&self.thread).unwrap().list[j];
        (t.bar, t.body, t.spans) = (bar, body, tab_spans);
        match t.list.get_mut(t.active) {
            Some(Tab::Term { live, .. }) => {
                if live.parser.lock().unwrap().screen().size() != (body.height, body.width) {
                    let _ = live.resize(body.height, body.width);
                }
                let p = live.parser.lock().unwrap();
                let mut pt = PseudoTerminal::new(p.screen());
                if !cursor || p.screen().scrollback() > 0 {
                    let mut c = tui_term::widget::Cursor::default();
                    c.hide();
                    pt = pt.cursor(c);
                }
                f.render_widget(pt, body);
            }
            Some(Tab::View { text, scroll, rel: Some(rel), .. }) => {
                // The diff above, the file's path right-aligned on the body's last row.
                let [d, foot] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(body);
                let p = Paragraph::new(diff_lines(text)).wrap(Wrap { trim: false });
                *scroll = (*scroll).min(p.line_count(d.width).saturating_sub(1).min(u16::MAX as usize) as u16);
                f.render_widget(p.scroll((*scroll, 0)), d);
                f.render_widget(Paragraph::new(rel.as_str()).style(Style::new().fg(pal().overlay0)).right_aligned(), foot);
            }
            Some(Tab::View { scroll, .. }) if plan.is_some() => {
                let lines = plan.take().unwrap_or_default();
                *scroll = (*scroll).min(lines.len().saturating_sub(1) as u16);
                f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), pad);
                // One screen row per line (no wrap): line i sits at pad.y + i - scroll.
                let top = *scroll as usize;
                for (i, xs, a) in hits {
                    if i < top || i - top >= pad.height as usize {
                        continue;
                    }
                    let x = pad.x.saturating_add(xs.start).min(pad.right());
                    let w = xs.end.min(pad.right().saturating_sub(pad.x)).saturating_sub(xs.start);
                    let rect = Rect::new(x, pad.y + (i - top) as u16, w, 1);
                    self.task_btns.push((rect, (plan_path.clone().unwrap_or_default(), a)));
                }
            }
            Some(Tab::View { path, text, scroll, .. }) => {
                let (p, area) = if path.extension().is_some_and(|e| e == "md") { (Paragraph::new(md_text(text, pad.width)), pad) } else { (Paragraph::new(text.as_str()), body) };
                let p = p.wrap(Wrap { trim: false });
                *scroll = (*scroll).min(p.line_count(area.width).saturating_sub(1).min(u16::MAX as usize) as u16);
                f.render_widget(p.scroll((*scroll, 0)), area);
            }
            None => f.render_widget(Paragraph::new("\n  Enter or click an agent to open its claude tab, or an output.md to read it."), body),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [top, hint] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [side, divider, main] = Layout::horizontal([
            Constraint::Length(self.side_w.unwrap_or(SIDE_W)),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(top);
        let border = if self.focus_main {
            pal().overlay0
        } else {
            pal().lavender
        };
        // No box: a dim title row, then the tree inside a one-column margin.
        let inner = Rect::new(
            side.x + 1,
            side.y + 1,
            side.width.saturating_sub(2),
            side.height.saturating_sub(1),
        );
        if main != self.main {
            self.main = main;
            self.resize_all();
        }
        (
            self.screen,
            self.side,
            self.side_inner,
            self.divider,
            self.hint,
        ) = (f.area(), side, inner, divider, hint);
        f.render_widget(
            Paragraph::new("spaces").style(Style::new().fg(pal().overlay0)),
            Rect::new(inner.x, side.y, inner.width, 1),
        );
        f.render_widget(
            Paragraph::new(vec![Line::from("┃"); divider.height as usize])
                .style(Style::new().fg(border)),
            divider,
        );

        let h = inner.height as usize;
        self.offset = self.offset.min(max_offset(&self.rows, h));
        let shown = layout(&self.rows, self.offset, h);
        let mut lines: Vec<Line> = vec![];
        for &(y, i) in &shown {
            lines.resize(y, Line::default());
            lines.push(self.row_line(&self.rows[i], i == self.sel, inner.width as usize));
        }
        if lines.is_empty() {
            f.render_widget(
                Paragraph::new("no threads yet\n\nbrigd <name> \"task\""),
                inner,
            );
        } else {
            f.render_widget(Paragraph::new(lines), inner);
        }

        // At the right end of each visible space row a '+' (new thread), of each thread row
        // its pause dot: ○ running, ● paused (click resumes).
        self.pause_btns.clear();
        self.plus_btns.clear();
        for (y, r) in shown.iter().map(|&(y, i)| (y, &self.rows[i])) {
            let (label, color) = match r.kind {
                Kind::Space => (" + ", pal().green),
                Kind::Thread if self.paused.contains(&r.label) => (" ● ", pal().red),
                Kind::Thread => (" ○ ", pal().red),
                _ => continue,
            };
            let w = label.chars().count() as u16;
            let rect = Rect::new(
                inner.right().saturating_sub(w).max(inner.x),
                inner.y + y as u16,
                w.min(inner.width),
                1,
            );
            f.render_widget(Paragraph::new(label).style(Style::new().fg(color)), rect);
            if r.kind == Kind::Space {
                self.plus_btns.push((rect, r.path.clone()));
            } else {
                self.pause_btns.push((rect, r.label.clone()));
            }
        }

        // The header '＋' (fullwidth: 2 columns) opens a thread in any directory.
        let plus = Span::styled(
            "＋",
            Style::new().fg(pal().green).add_modifier(Modifier::BOLD),
        );
        let w = plus.width() as u16;
        self.header_plus = Rect::new(
            inner.right().saturating_sub(w).max(inner.x),
            side.y,
            w.min(inner.width),
            1,
        );
        f.render_widget(plus, self.header_plus);
        self.plus_btns.push((self.header_plus, self.new_dir()));

        // The thread's panes side by side, a dim "│" between them.
        let rects = pane_rects(
            main,
            self.tabs.entry(self.thread.clone()).or_default().list.len(),
        );
        if let [(l, _), _] = rects[..] {
            f.render_widget(
                Paragraph::new(vec![Line::from("│"); main.height as usize])
                    .style(Style::new().fg(pal().surface1)),
                Rect::new(l.right(), main.y, 1, main.height),
            );
        }
        self.task_btns.clear();
        for (j, (bar, body)) in rects.into_iter().enumerate() {
            self.draw_pane(f, j, bar, body);
        }

        let viewing = matches!(self.cur(), Some(Tab::View { .. }));
        let text = match &self.msg {
            Some((m, t)) if t.elapsed() < Duration::from_secs(5) => Span::styled(m.clone(), Style::new().fg(pal().yellow)),
            _ if self.confirm_delete.is_some() => Span::styled(
                format!("Delete thread {} with its worktrees and brigd/ branches? This cannot be undone. y/N", self.confirm_delete.as_deref().unwrap_or("")),
                Style::new().fg(pal().red).add_modifier(Modifier::BOLD),
            ),
            _ if self.confirm_quit => Span::styled(
                format!("{} agents running; quitting kills them. Quit? y/N", live_agents().len()),
                Style::new().fg(pal().red).add_modifier(Modifier::BOLD),
            ),
            _ if self.prefix => Span::raw("C-o …  s sidebar · n/p next/prev tab · t terminal · w close tab · v split · o pane · c theme · q quit · C-o send C-o"),
            _ if self.thread_prefix => Span::raw("C-t …  d delete · a archive · n new thread · m new thread anywhere · C-t send C-t"),
            _ if !self.focus_main => Span::raw("↑↓/jk move · Enter open/fold · ←→ fold · Tab back to tab · M-h/l tabs · M-t terminal · q quit · wheel scrolls"),
            _ if viewing => Span::raw("↑↓/jk PgUp/PgDn g/G scroll · Tab/Esc sidebar · M-h/l tabs · M-t terminal · C-o w close · C-o q quit"),
            _ => Span::raw("keys go to the tab · C-o s sidebar · M-h/l tabs · M-t terminal · C-o w close (keeps running) · C-o q quit · C-t thread · wheel scrollback"),
        };
        f.render_widget(
            Paragraph::new(
                Line::from(text).style(Style::new().fg(pal().subtext0).bg(pal().mantle)),
            ),
            hint,
        );

        if let Some(m) = &self.menu {
            let r = m.rect.intersection(f.area());
            f.render_widget(Clear, r);
            let lines = m.items.iter().enumerate().map(|(i, a)| {
                let bg = if i == m.sel {
                    pal().red
                } else {
                    pal().surface1
                };
                Line::styled(
                    a.label(),
                    Style::new()
                        .fg(if i == m.sel { pal().crust } else { pal().text })
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )
            });
            f.render_widget(Paragraph::new(lines.collect::<Vec<_>>()), r);
        }

        // The new-thread window, or the run confirm once it is planned, centered on top.
        let a = f.area();
        let modal = if let Some(n) = self.newt.as_ref().filter(|n| !n.hidden) {
            // Fixed width; each field wraps inside it and the box grows to fit.
            let w = NEWT_W.min(a.width);
            let inner = (w as usize).saturating_sub(2);
            let dim = Style::new().fg(pal().overlay0);
            let mut lines = vec![];
            for (i, (label, text)) in ["dir: ", "name: ", "task: "]
                .into_iter()
                .zip(&n.fields)
                .enumerate()
            {
                let cursor = if i == n.focus && !n.planning && !n.ask {
                    "█"
                } else {
                    ""
                };
                let style = if n.planning { dim } else { Style::new() };
                lines.extend(
                    chunk(&format!("{label}{text}{cursor}"), inner)
                        .into_iter()
                        .map(|l| Line::styled(l, style)),
                );
            }
            let (status, style) = if n.planning {
                (
                    format!(
                        "{} planning {}… · Esc hides",
                        SPIN[self.ticks as usize % SPIN.len()],
                        n.fields[1]
                    ),
                    Style::new().fg(pal().yellow),
                )
            } else if n.ask {
                (
                    "Use plan-review-go? (y/N) · Esc edits".into(),
                    Style::new().fg(pal().yellow),
                )
            } else if let Some(e) = &n.err {
                (e.clone(), Style::new().fg(pal().red))
            } else {
                ("Tab/↑↓ switch · Enter next/plans · Esc cancels".into(), dim)
            };
            lines.extend(
                chunk(&status, inner)
                    .into_iter()
                    .map(|l| Line::styled(l, style)),
            );
            Some((" new thread ".to_string(), lines, w))
        } else {
            self.confirm_run.as_ref().map(|(name, repo)| {
                let l = Line::styled(
                    format!("Run thread {name} in {repo}? y / n"),
                    Style::new().add_modifier(Modifier::BOLD),
                );
                let w = (l.width().max(50) as u16 + 4).min(a.width);
                (" run ".into(), vec![l], w)
            })
        };
        if let Some((title, lines, w)) = modal {
            let h = (lines.len() as u16).saturating_add(2).min(a.height);
            let r = Rect::new(a.x + (a.width - w) / 2, a.y + (a.height - h) / 2, w, h);
            f.render_widget(Clear, r);
            // Clamped to the screen: drop the top rows so the cursor row and the status line stay visible.
            let skip = (lines.len() as u16).saturating_sub(h.saturating_sub(2));
            f.render_widget(
                Paragraph::new(lines)
                    .scroll((skip, 0))
                    .wrap(Wrap { trim: false })
                    .block(
                        Block::bordered()
                            .title(title)
                            .border_style(Style::new().fg(pal().lavender)),
                    ),
                r,
            );
        }

        if let Some((r, a, Some(b))) = self.drag {
            self.buf = f.buffer_mut().clone();
            for (x, y) in sel_rows(r, a, b)
                .into_iter()
                .flat_map(|(y, xs)| xs.map(move |x| (x, y)))
            {
                if let Some(c) = f.buffer_mut().cell_mut((x, y)) {
                    c.modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }
}

/// Lists each thread's live terminals (`terms`: (thread, shell name)) in a TERMS folder, after its
/// FLOWPLAN if it has one, else first.
fn add_terminals(tree: &mut [Node], terms: &[(String, String)]) {
    for t in tree.iter_mut().flat_map(|s| &mut s.children) {
        let mut names: Vec<&str> = terms
            .iter()
            .filter(|(th, _)| *th == t.label)
            .map(|(_, n)| n.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        names.sort();
        let dir = t.path.join(".terminal");
        let node = |kind, label: &str, path, children| Node {
            kind,
            label: label.into(),
            path,
            children,
            stage: None,
            worktree: None,
        };
        let kids = names
            .into_iter()
            .map(|n| node(Kind::Terminal, n, dir.join(n), vec![]))
            .collect();
        let at = t
            .children
            .iter()
            .position(|c| c.kind == Kind::Output && c.path.ends_with("FLOWPLAN"))
            .map_or(0, |i| i + 1);
        t.children
            .insert(at, node(Kind::Folder, tree::TERMS, dir, kids));
    }
}

/// A stray tab shows "stray-1", not "stray agents/stray-1".
fn tail(agent: &str) -> &str {
    agent.rsplit('/').next().unwrap_or(agent)
}

/// A stray running in a terminal tab is no LiveAgent: it is alive while the pid it saved
/// (exec kept brigd's) runs; the pid file goes once it is gone. Status from its hooks.
fn stray_status(dir: &Path) -> &'static str {
    // Gone: terminated by the user (failed) or just exited (off).
    let gone = || {
        if dir.join(runner::TERMINATED).exists() {
            "failed"
        } else {
            "off"
        }
    };
    let Ok(pid) = fs::read_to_string(dir.join("pid")) else {
        return gone();
    };
    if !pid.trim().parse().is_ok_and(runner::pid_alive) {
        let _ = fs::remove_file(dir.join("pid"));
        return gone();
    }
    let cwd = fs::read_to_string(dir.join("cwd")).unwrap_or_default();
    agent_glyph_status(Some(&runner::read_status(dir, Path::new(cwd.trim()))), "")
}

/// The first `term-<n>` not in `taken`.
fn fresh_name<'a>(taken: impl Iterator<Item = &'a str>) -> String {
    let taken: HashSet<&str> = taken.collect();
    (1..)
        .map(|n| format!("term-{n}"))
        .find(|n| !taken.contains(n.as_str()))
        .unwrap()
}

#[cfg(test)]
mod tests;
