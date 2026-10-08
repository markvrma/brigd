//! brigd's TUI: a sidebar of spaces (the tree.rs folder tree) and, for the
//! selected thread, one or two side-by-side panes, each a tab bar of agent terminals
//! (runner.rs PTYs drawn with tui-term) and read-only output.md viewers.

use crate::runner::{self, LiveAgent, Res};
use crate::tree::{self, Kind, Node};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self as ct, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color::{self, Rgb}, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs};
use tui_term::widget::PseudoTerminal;

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
    Term { thread: String, agent: String, stage: Option<usize>, live: Arc<LiveAgent> },
    /// `rel`: a Diff node's file path (relative to the agent's cwd); `text` is then its diff.
    View { path: PathBuf, title: String, text: String, scroll: u16, rel: Option<String> },
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
        Panes { list: vec![Tabs::default()], focus: 0 }
    }
}

impl Panes {
    /// (pane, tab) of the first tab matching `f`: an open tab is focused, never shown twice.
    fn find(&self, f: impl Fn(&Tab) -> bool) -> Option<(usize, usize)> {
        self.list.iter().enumerate().find_map(|(j, t)| Some((j, t.list.iter().position(&f)?)))
    }
}

/// `n` equal columns of `main` with a 1-column gap: each pane's (tab bar, body).
fn pane_rects(main: Rect, n: usize) -> Vec<(Rect, Rect)> {
    let cols = Layout::horizontal(vec![Constraint::Fill(1); n.max(1)]).spacing(1).split(main);
    cols.iter().map(|&c| (Rect { height: c.height.min(1), ..c }, Rect { y: c.y + c.height.min(1), height: c.height.saturating_sub(1), ..c })).collect()
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

/// The '+' new-thread window: a name and a task field; once submitted, read-only while planning.
#[derive(Debug, Default, PartialEq)]
struct Newt {
    repo: PathBuf,
    name: String,
    task: String,
    /// Which field has focus: false = name, true = task.
    focus_task: bool,
    err: Option<String>,
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
        let cw = if c == '\n' { 0 } else { Span::raw(c.to_string()).width() };
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
    plans: HashMap<PathBuf, (crate::Flow, crate::State)>,
}

/// Opens the TUI over every thread in ~/.brigd/threads until Mark quits.
/// Quitting kills every live agent.
pub fn run(open: Option<&str>) -> Res<()> {
    init_theme();
    setup()?;
    let res = (|| -> Res<()> {
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        term.clear()?;
        let mut app = App::default();
        app.refresh();
        if let Some(t) = open {
            app.open_flowplan(t);
        }
        let mut last = Instant::now();
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
            if last.elapsed() >= REFRESH {
                app.refresh();
                last = Instant::now();
            }
        }
        live_agents().iter().for_each(|a| a.kill());
        // A plan still running dies with brigd: free the names it reserved.
        app.planning.iter().for_each(|n| _ = fs::remove_dir_all(crate::thread_dir(n)));
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
    crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)
}

fn teardown() {
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen, crossterm::cursor::Show);
    let _ = ct::disable_raw_mode();
}

/// OSC 52 for terminals that take it, pbcopy (in the background) for those that don't.
fn copy_to_clipboard(s: &str) {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(s.as_bytes())).and_then(|_| out.flush());
    if cfg!(target_os = "macos") {
        let s = s.to_string();
        std::thread::spawn(move || {
            let _ = Command::new("pbcopy").stdin(Stdio::piped()).spawn().and_then(|mut c| {
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
            let n = c.iter().enumerate().fold(0u32, |n, (i, &x)| n | ((x as u32) << (16 - 8 * i)));
            (0..4).map(move |i| if i <= c.len() { T[((n >> (18 - 6 * i)) & 63) as usize] as char } else { '=' })
        })
        .collect()
}

/// The cells from `a` to `b` in reading order, like a terminal selection, clamped to `r`.
fn sel_rows(r: Rect, a: Position, b: Position) -> Vec<(u16, std::ops::RangeInclusive<u16>)> {
    let clamp = |p: Position| Position { x: p.x.clamp(r.x, r.right() - 1), y: p.y.clamp(r.y, r.bottom() - 1) };
    let (a, b) = (clamp(a), clamp(b));
    let (a, b) = if (a.y, a.x) <= (b.y, b.x) { (a, b) } else { (b, a) };
    (a.y..=b.y).map(|y| (y, if y == a.y { a.x } else { r.x }..=if y == b.y { b.x } else { r.right() - 1 })).collect()
}

// ponytail: wide chars read as their cell symbols; a CJK glyph may gain a trailing space.
fn sel_text(buf: &Buffer, r: Rect, a: Position, b: Position) -> String {
    let line = |(y, xs): (u16, std::ops::RangeInclusive<u16>)| xs.filter_map(|x| buf.cell((x, y))).map(|c| c.symbol()).collect::<String>().trim_end().to_string();
    sel_rows(r, a, b).into_iter().map(line).collect::<Vec<_>>().join("\n")
}

fn live_agents() -> Vec<Arc<LiveAgent>> {
    let all: Vec<Arc<LiveAgent>> = runner::registry().lock().unwrap().values().cloned().collect();
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

/// While the process lives its hook status wins (a done agent Mark types into is live
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

/// The flow as an npx-style step tree ending at crate::LOG_MARK; also the plain FLOWPLAN
/// text (crate::flow_text). `status(agent)`: pending / running / blocked / done / failed.
pub fn plan_lines(flow: &crate::Flow, status: impl Fn(&str) -> &'static str, frame: usize, width: usize) -> Vec<Line<'static>> {
    let dim = Style::new().fg(pal().overlay0);
    let guide = |s: &str| Span::styled(s.to_string(), dim);
    let mut out = vec![];
    for (i, l) in wrap(&flow.goal, width.saturating_sub(3).max(20)).into_iter().enumerate() {
        let lead = if i == 0 { Span::styled("◆  ", Style::new().fg(pal().mauve)) } else { guide("│  ") };
        out.push(Line::from(vec![lead, Span::styled(l, Style::new().fg(pal().text).add_modifier(Modifier::BOLD))]));
    }
    let w = flow.stages.iter().flatten().map(|a| a.name.len()).max().unwrap_or(0);
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
        let n = if stage.len() == 1 { "1 agent".to_string() } else { format!("{} parallel", stage.len()) };
        out.push(Line::from(guide("│")));
        out.push(Line::from(vec![step(st, frame), Span::raw("  "), Span::styled(format!("stage {}", i + 1), Style::new().fg(c).add_modifier(Modifier::BOLD)), guide(&format!(" · {n}"))]));
        for (j, (a, st)) in stage.iter().zip(&sts).enumerate() {
            let last = j + 1 == stage.len();
            let wt = a.worktree.as_deref().filter(|w| !w.is_empty()).map_or("main".into(), |w| format!("wt {w}"));
            let meta = format!("{} · {} · {wt}", a.model, a.effort);
            out.push(Line::from(vec![guide(if last { "│  └─ " } else { "│  ├─ " }), step(st, frame), Span::styled(format!(" {:<w$}  ", a.name), Style::new().fg(c)), guide(&meta)]));
            out.push(Line::from(vec![guide(if last { "│       " } else { "│  │    " }), Span::styled(crate::first_line(&a.task), dim.add_modifier(Modifier::ITALIC))]));
        }
    }
    out.push(Line::from(guide("│")));
    out.push(Line::from(guide(crate::LOG_MARK)));
    out
}

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

/// One color per role. `THEMES[0]` (Catppuccin Mocha) is the default; hex values are each palette's official ones.
struct Pal {
    name: &'static str,
    mauve: Color,
    pink: Color,
    red: Color,
    peach: Color,
    yellow: Color,
    green: Color,
    teal: Color,
    sky: Color,
    blue: Color,
    lavender: Color,
    text: Color,
    subtext0: Color,
    overlay0: Color,
    surface1: Color,
    surface0: Color,
    mantle: Color,
    crust: Color,
}

const THEMES: &[Pal] = &[
    Pal { name: "catppuccin-mocha", mauve: Rgb(0xcb, 0xa6, 0xf7), pink: Rgb(0xf5, 0xc2, 0xe7), red: Rgb(0xf3, 0x8b, 0xa8), peach: Rgb(0xfa, 0xb3, 0x87), yellow: Rgb(0xf9, 0xe2, 0xaf), green: Rgb(0xa6, 0xe3, 0xa1), teal: Rgb(0x94, 0xe2, 0xd5), sky: Rgb(0x89, 0xdc, 0xeb), blue: Rgb(0x89, 0xb4, 0xfa), lavender: Rgb(0xb4, 0xbe, 0xfe), text: Rgb(0xcd, 0xd6, 0xf4), subtext0: Rgb(0xa6, 0xad, 0xc8), overlay0: Rgb(0x6c, 0x70, 0x86), surface1: Rgb(0x45, 0x47, 0x5a), surface0: Rgb(0x31, 0x32, 0x44), mantle: Rgb(0x18, 0x18, 0x25), crust: Rgb(0x11, 0x11, 0x1b) },
    Pal { name: "catppuccin-latte", mauve: Rgb(0x88, 0x39, 0xef), pink: Rgb(0xea, 0x76, 0xcb), red: Rgb(0xd2, 0x0f, 0x39), peach: Rgb(0xfe, 0x64, 0x0b), yellow: Rgb(0xdf, 0x8e, 0x1d), green: Rgb(0x40, 0xa0, 0x2b), teal: Rgb(0x17, 0x92, 0x99), sky: Rgb(0x04, 0xa5, 0xe5), blue: Rgb(0x1e, 0x66, 0xf5), lavender: Rgb(0x72, 0x87, 0xfd), text: Rgb(0x4c, 0x4f, 0x69), subtext0: Rgb(0x6c, 0x6f, 0x85), overlay0: Rgb(0x9c, 0xa0, 0xb0), surface1: Rgb(0xbc, 0xc0, 0xcc), surface0: Rgb(0xcc, 0xd0, 0xda), mantle: Rgb(0xe6, 0xe9, 0xef), crust: Rgb(0xdc, 0xe0, 0xe8) },
    Pal { name: "dracula", mauve: Rgb(0xbd, 0x93, 0xf9), pink: Rgb(0xff, 0x79, 0xc6), red: Rgb(0xff, 0x55, 0x55), peach: Rgb(0xff, 0xb8, 0x6c), yellow: Rgb(0xf1, 0xfa, 0x8c), green: Rgb(0x50, 0xfa, 0x7b), teal: Rgb(0x8b, 0xe9, 0xfd), sky: Rgb(0x8b, 0xe9, 0xfd), blue: Rgb(0xbd, 0x93, 0xf9), lavender: Rgb(0xbd, 0x93, 0xf9), text: Rgb(0xf8, 0xf8, 0xf2), subtext0: Rgb(0xc0, 0xc4, 0xd6), overlay0: Rgb(0x62, 0x72, 0xa4), surface1: Rgb(0x44, 0x47, 0x5a), surface0: Rgb(0x34, 0x37, 0x46), mantle: Rgb(0x21, 0x22, 0x2c), crust: Rgb(0x19, 0x1a, 0x21) },
    Pal { name: "nord", mauve: Rgb(0xb4, 0x8e, 0xad), pink: Rgb(0xb4, 0x8e, 0xad), red: Rgb(0xbf, 0x61, 0x6a), peach: Rgb(0xd0, 0x87, 0x70), yellow: Rgb(0xeb, 0xcb, 0x8b), green: Rgb(0xa3, 0xbe, 0x8c), teal: Rgb(0x8f, 0xbc, 0xbb), sky: Rgb(0x88, 0xc0, 0xd0), blue: Rgb(0x5e, 0x81, 0xac), lavender: Rgb(0x81, 0xa1, 0xc1), text: Rgb(0xec, 0xef, 0xf4), subtext0: Rgb(0xd8, 0xde, 0xe9), overlay0: Rgb(0x61, 0x6e, 0x88), surface1: Rgb(0x43, 0x4c, 0x5e), surface0: Rgb(0x3b, 0x42, 0x52), mantle: Rgb(0x2e, 0x34, 0x40), crust: Rgb(0x24, 0x29, 0x33) },
    Pal { name: "gruvbox-dark", mauve: Rgb(0xd3, 0x86, 0x9b), pink: Rgb(0xd3, 0x86, 0x9b), red: Rgb(0xfb, 0x49, 0x34), peach: Rgb(0xfe, 0x80, 0x19), yellow: Rgb(0xfa, 0xbd, 0x2f), green: Rgb(0xb8, 0xbb, 0x26), teal: Rgb(0x8e, 0xc0, 0x7c), sky: Rgb(0x83, 0xa5, 0x98), blue: Rgb(0x83, 0xa5, 0x98), lavender: Rgb(0xd3, 0x86, 0x9b), text: Rgb(0xeb, 0xdb, 0xb2), subtext0: Rgb(0xa8, 0x99, 0x84), overlay0: Rgb(0x92, 0x83, 0x74), surface1: Rgb(0x50, 0x49, 0x45), surface0: Rgb(0x3c, 0x38, 0x36), mantle: Rgb(0x28, 0x28, 0x28), crust: Rgb(0x1d, 0x20, 0x21) },
    Pal { name: "tokyo-night", mauve: Rgb(0xbb, 0x9a, 0xf7), pink: Rgb(0xff, 0x00, 0x7c), red: Rgb(0xf7, 0x76, 0x8e), peach: Rgb(0xff, 0x9e, 0x64), yellow: Rgb(0xe0, 0xaf, 0x68), green: Rgb(0x9e, 0xce, 0x6a), teal: Rgb(0x73, 0xda, 0xca), sky: Rgb(0x7d, 0xcf, 0xff), blue: Rgb(0x7a, 0xa2, 0xf7), lavender: Rgb(0x9d, 0x7c, 0xd8), text: Rgb(0xc0, 0xca, 0xf5), subtext0: Rgb(0xa9, 0xb1, 0xd6), overlay0: Rgb(0x56, 0x5f, 0x89), surface1: Rgb(0x41, 0x48, 0x68), surface0: Rgb(0x29, 0x2e, 0x42), mantle: Rgb(0x16, 0x16, 0x1e), crust: Rgb(0x10, 0x10, 0x14) },
    Pal { name: "one-dark", mauve: Rgb(0xc6, 0x78, 0xdd), pink: Rgb(0xc6, 0x78, 0xdd), red: Rgb(0xe0, 0x6c, 0x75), peach: Rgb(0xd1, 0x9a, 0x66), yellow: Rgb(0xe5, 0xc0, 0x7b), green: Rgb(0x98, 0xc3, 0x79), teal: Rgb(0x56, 0xb6, 0xc2), sky: Rgb(0x56, 0xb6, 0xc2), blue: Rgb(0x61, 0xaf, 0xef), lavender: Rgb(0x61, 0xaf, 0xef), text: Rgb(0xab, 0xb2, 0xbf), subtext0: Rgb(0x82, 0x89, 0x97), overlay0: Rgb(0x5c, 0x63, 0x70), surface1: Rgb(0x4b, 0x52, 0x63), surface0: Rgb(0x2c, 0x31, 0x3c), mantle: Rgb(0x21, 0x25, 0x2b), crust: Rgb(0x18, 0x1a, 0x1f) },
    Pal { name: "solarized-dark", mauve: Rgb(0x6c, 0x71, 0xc4), pink: Rgb(0xd3, 0x36, 0x82), red: Rgb(0xdc, 0x32, 0x2f), peach: Rgb(0xcb, 0x4b, 0x16), yellow: Rgb(0xb5, 0x89, 0x00), green: Rgb(0x85, 0x99, 0x00), teal: Rgb(0x2a, 0xa1, 0x98), sky: Rgb(0x2a, 0xa1, 0x98), blue: Rgb(0x26, 0x8b, 0xd2), lavender: Rgb(0x6c, 0x71, 0xc4), text: Rgb(0x93, 0xa1, 0xa1), subtext0: Rgb(0x83, 0x94, 0x96), overlay0: Rgb(0x58, 0x6e, 0x75), surface1: Rgb(0x0d, 0x46, 0x55), surface0: Rgb(0x07, 0x36, 0x42), mantle: Rgb(0x00, 0x2b, 0x36), crust: Rgb(0x00, 0x21, 0x2b) },
    Pal { name: "rose-pine", mauve: Rgb(0xc4, 0xa7, 0xe7), pink: Rgb(0xeb, 0xbc, 0xba), red: Rgb(0xeb, 0x6f, 0x92), peach: Rgb(0xf6, 0xc1, 0x77), yellow: Rgb(0xf6, 0xc1, 0x77), green: Rgb(0x9c, 0xcf, 0xd8), teal: Rgb(0x9c, 0xcf, 0xd8), sky: Rgb(0x9c, 0xcf, 0xd8), blue: Rgb(0x3e, 0x8f, 0xb0), lavender: Rgb(0xc4, 0xa7, 0xe7), text: Rgb(0xe0, 0xde, 0xf4), subtext0: Rgb(0x90, 0x8c, 0xaa), overlay0: Rgb(0x6e, 0x6a, 0x86), surface1: Rgb(0x40, 0x3d, 0x52), surface0: Rgb(0x26, 0x23, 0x3a), mantle: Rgb(0x1f, 0x1d, 0x2e), crust: Rgb(0x19, 0x17, 0x24) },
];

static THEME: AtomicUsize = AtomicUsize::new(0);

fn pal() -> &'static Pal {
    &THEMES[THEME.load(Ordering::Relaxed)]
}

fn theme_index(name: &str) -> usize {
    THEMES.iter().position(|t| t.name == name.trim()).unwrap_or(0)
}

/// $BRIGD_THEME, else ~/.brigd/theme, else catppuccin-mocha.
fn init_theme() {
    let name = env::var("BRIGD_THEME").or_else(|_| fs::read_to_string(crate::root().join("theme"))).unwrap_or_default();
    THEME.store(theme_index(&name), Ordering::Relaxed);
}

fn next_theme() -> &'static str {
    let i = (THEME.load(Ordering::Relaxed) + 1) % THEMES.len();
    THEME.store(i, Ordering::Relaxed);
    let _ = fs::write(crate::root().join("theme"), format!("{}\n", THEMES[i].name)); // ponytail: best effort, lost write just means no persistence
    THEMES[i].name
}

/// Agent rows/tabs are tinted by stage; avoids the status-glyph and selection colors.
fn stage_color(stage: Option<usize>) -> Color {
    let p = pal();
    let c = [p.mauve, p.yellow, p.blue, p.peach, p.pink, p.teal];
    stage.map_or(Color::Reset, |s| c[s % c.len()])
}

fn flatten(n: &Node, depth: usize, space: &Path, thread: &str, toggled: &HashSet<NodeKey>, out: &mut Vec<Row>) {
    let folder = !n.children.is_empty();
    let default_open = matches!(n.kind, Kind::Space | Kind::Thread | Kind::Folder);
    let open = folder && default_open != toggled.contains(&(space.into(), n.path.clone()));
    let thread = if n.kind == Kind::Thread { n.label.as_str() } else { thread };
    out.push(Row { depth, kind: n.kind.clone(), label: n.label.clone(), path: n.path.clone(), space: space.into(), thread: thread.into(), folder, open, stage: n.stage, worktree: n.worktree.clone() });
    if open {
        n.children.iter().for_each(|c| flatten(c, depth + 1, space, thread, toggled, out));
    }
}

/// The rows that fit in `h` sidebar lines from row `offset`, as (line, row index).
/// A blank line goes above each space and each thread but a space's first.
fn layout(rows: &[Row], offset: usize, h: usize) -> Vec<(usize, usize)> {
    let mut y = 0;
    let mut out = vec![];
    for (i, r) in rows.iter().enumerate().skip(offset) {
        if i > offset && (r.kind == Kind::Space || r.kind == Kind::Thread && rows[i - 1].kind != Kind::Space) {
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
    (0..rows.len()).find(|&o| layout(rows, o, h).last().is_some_and(|&(_, i)| i + 1 == rows.len())).unwrap_or(0)
}

/// The sidebar row under a click, and whether the click hit its fold arrow.
fn hit(inner: Rect, offset: usize, rows: &[Row], col: u16, row: u16) -> Option<(usize, bool)> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let dy = (row - inner.y) as usize;
    let i = layout(rows, offset, inner.height as usize).into_iter().find(|&(y, _)| y == dy)?.1;
    let r = &rows[i];
    let arrow_x = (r.depth * INDENT) as u16;
    let x = col - inner.x;
    Some((i, r.folder && x >= arrow_x && x < arrow_x + 2))
}

/// A key as the bytes a terminal would send. `app_cursor` = DECCKM (arrows as ESC O x).
fn key_bytes(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
    let arrow = |c: u8| if app_cursor { vec![0x1b, b'O', c] } else { vec![0x1b, b'[', c] };
    let mut out = match k.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => vec![c.to_ascii_lowercase() as u8 - b'a' + 1],
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
        KeyCode::F(n @ 5..=12) => format!("\x1b[{}~", [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]).into_bytes(),
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
        self.ticks += 1;
        self.tree = tree::build(&crate::root());
        let terms: Vec<_> = self.live_shells().into_iter().map(|(a, t)| (t, a.agent.clone())).collect();
        add_terminals(&mut self.tree, &terms);
        let reg = runner::registry().lock().unwrap().clone();
        let (mut agents, mut threads) = (HashMap::new(), HashMap::new());
        for t in self.tree.iter().flat_map(|s| &s.children) {
            let state: serde_json::Value = fs::read_to_string(t.path.join("state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            threads.insert(t.path.clone(), state["status"].as_str().unwrap_or("").to_string());
            let strays = t.children.iter().filter(|c| c.kind == Kind::Folder).flat_map(|c| &c.children);
            for a in t.children.iter().chain(strays).filter(|a| a.kind == Kind::Agent) {
                let key = (t.label.clone(), tree::agent_id(&a.path, &a.label));
                let saved = state["agents"][&a.label]["status"].as_str().unwrap_or("");
                let st = match reg.get(&key) {
                    l if key.1 != a.label && !l.is_some_and(|l| l.alive()) => stray_status(&a.path),
                    l => agent_glyph_status(l.filter(|l| l.alive()).map(|l| l.status()).as_deref(), saved),
                };
                agents.insert(key, st);
            }
        }
        let blocked: HashSet<_> = agents.iter().filter(|(_, s)| **s == "blocked").map(|(k, _)| k.clone()).collect();
        if blocked.difference(&self.blocked).next().is_some() {
            let _ = io::stdout().write_all(b"\x07").and_then(|_| io::stdout().flush());
        }
        (self.agent_status, self.thread_status, self.blocked) = (agents, threads, blocked);

        // Tabs follow a restart of their agent (execute or resume registers a new LiveAgent).
        let diff_tick = self.ticks.is_multiple_of(4); // git diff open diff tabs each ~3s, not every refresh
        for tab in self.tabs.values_mut().flat_map(|p| &mut p.list).flat_map(|t| &mut t.list) {
            match tab {
                Tab::Term { thread, agent, live, .. } => {
                    if let Some(new) = reg.get(&(thread.clone(), agent.clone())).filter(|n| n.alive() && !Arc::ptr_eq(n, live)) {
                        *live = new.clone();
                    }
                }
                Tab::View { path, text, rel: Some(_), .. } => {
                    if let Some((_, t)) = diff_tick.then(|| tree::diff_view(path)).flatten() {
                        *text = t;
                    }
                }
                Tab::View { path, text, .. } => *text = fs::read_to_string(&*path).unwrap_or_else(|e| format!("({e})")),
            }
        }
        self.load_plans();
        self.resize_all();
        // supervise mirrors subagents only for agents execute runs; this covers ones resumed from the sidebar.
        if self.ticks % 7 == 0 {
            let home = PathBuf::from(env::var("HOME").unwrap_or_default());
            let mut runs: Vec<(PathBuf, PathBuf)> = live_agents().into_iter().filter(|a| !a.dir.as_os_str().is_empty()).map(|a| (a.dir.clone(), a.cwd.clone())).collect();
            // Strays running in a terminal tab (they keep a pid file while alive).
            let folders = self.tree.iter().flat_map(|s| &s.children).flat_map(|t| &t.children).filter(|c| c.kind == Kind::Folder);
            for d in folders.flat_map(|f| &f.children).map(|a| &a.path).filter(|d| d.join("pid").exists()) {
                runs.extend(fs::read_to_string(d.join("cwd")).map(|c| (d.clone(), PathBuf::from(c.trim()))));
            }
            for (dir, cwd) in runs {
                if let Ok(s) = fs::read_to_string(dir.join("session")) {
                    let _ = tree::sync_subagents(&tree::subagents_dir(&home, &cwd, s.trim()), &dir);
                }
            }
        }
        self.reflow();
    }

    /// Reloads flowmap.json + state.json for every open FLOWPLAN tab.
    fn load_plans(&mut self) {
        let paths = self.tabs.values().flat_map(|p| &p.list).flat_map(|t| &t.list).filter_map(|t| match t {
            Tab::View { path, rel: None, .. } if path.ends_with("FLOWPLAN") => Some(path.clone()),
            _ => None,
        });
        self.plans = paths.filter_map(|p| Some((p.clone(), crate::load(&p.parent()?.file_name()?.to_string_lossy()).ok()?))).collect();
    }

    /// A FLOWPLAN tab's view: the step tree with live statuses, then the run log's tail.
    fn plan_view(&self, path: &Path, text: &str, width: usize) -> Option<Vec<Line<'static>>> {
        let (flow, state) = self.plans.get(path)?;
        // Saved done/failed wins (a done agent idles on, live); else the live hook status.
        let status = |a: &str| match state.agents.get(a).map(|s| s.status.as_str()) {
            Some("done") => "done",
            Some("failed") => "failed",
            _ => match self.agent_status.get(&(state.thread.clone(), a.to_string())).copied() {
                Some("running" | "idle") => "running",
                Some("blocked") => "blocked",
                _ => "pending",
            },
        };
        let frame = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() / 80) as usize;
        let mut lines = plan_lines(flow, status, frame, width);
        // The log follows LOG_MARK; a FLOWPLAN from before the marker starts its log at the first "──" line.
        let all: Vec<&str> = text.lines().collect();
        let start = all.iter().position(|l| l.trim_end() == crate::LOG_MARK).map(|i| i + 1).or_else(|| all.iter().position(|l| l.starts_with("──")));
        let start = start.unwrap_or(all.len()).max(all.len().saturating_sub(LOG_TAIL));
        lines.extend(all[start..].iter().map(|l| Line::styled(l.to_string(), Style::new().fg(pal().overlay0))));
        Some(lines)
    }

    /// Re-flattens the tree (after a refresh or a fold), keeping the selected node.
    fn reflow(&mut self) {
        let keep = self.rows.get(self.sel).map(|r| (r.space.clone(), r.path.clone()));
        let mut rows = Vec::new();
        for s in &self.tree {
            flatten(s, 0, &s.path, "", &self.toggled, &mut rows);
        }
        self.rows = rows;
        if let Some(i) = keep.and_then(|k| self.rows.iter().position(|r| (&r.space, &r.path) == (&k.0, &k.1))) {
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
        while self.offset < self.sel && !layout(&self.rows, self.offset, h).iter().any(|&(_, i)| i == self.sel) {
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
                let (thread, agent, stage) = (r.thread.clone(), tree::agent_id(&r.path, &r.label), r.stage);
                if r.folder && !r.open {
                    self.toggle(i);
                }
                self.open_agent(thread.clone(), &thread, &agent, stage);
            }
            Kind::Terminal => {
                let (key, name) = (r.thread.clone(), r.label.clone());
                if let Some((a, _)) = self.live_shells().into_iter().find(|(a, t)| a.agent == name && *t == key) {
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
        let found = p.find(|t| matches!(t, Tab::Term { thread: t, agent: a, .. } if t == thread && a == agent));
        if let Some((j, _)) = found {
            p.focus = j;
        }
        if let Some((j, i)) = found.filter(|&(j, i)| matches!(&p.list[j].list[i], Tab::Term { live, .. } if live.alive())) {
            p.list[j].active = i;
            self.focus_main = true;
            return;
        }
        // Not live (brigd restarted, or it exited): reopen its session.
        match runner::resume_agent(thread, agent) {
            Ok(live) => {
                let b = self.pane_body(&key);
                let _ = live.resize(b.height.max(1), b.width.max(1));
                let tab = Tab::Term { thread: thread.into(), agent: agent.into(), stage, live };
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
        let cwd = if space.as_os_str().is_empty() { env::current_dir().unwrap_or_default() } else { space.clone() };
        // Registry key (space, name): names are unique per space, and a closed-but-running tab keeps its name.
        let thread = space.to_string_lossy().into_owned();
        let reg = runner::registry().lock().unwrap().clone();
        let taken = self.tabs.values().flat_map(|p| &p.list).flat_map(|t| &t.list).filter_map(|t| match t {
            Tab::Term { thread: t, agent, .. } if *t == thread => Some(agent.as_str()),
            _ => None,
        });
        // ponytail: names of every live shell are taken too, so the terminal folder can find a shell by thread + name
        let is_shell = |k: &(String, String)| k.0 == thread || self.shell_thread.contains_key(k);
        let name = fresh_name(taken.chain(reg.iter().filter(|(k, a)| is_shell(k) && a.alive()).map(|(k, _)| k.1.as_str())));
        // No dir: a shell has no hooks/status/session files (status() reads as "starting", never shown).
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = portable_pty::CommandBuilder::new(&shell);
        if let Some(t) = &brigd_thread {
            cmd.env("BRIGD_THREAD", t);
        }
        if let (Ok(bin), Ok(shim)) = (env::current_exe(), crate::ensure_shim()) {
            let path = env::var_os("PATH").unwrap_or_default();
            if let Ok(p) = env::join_paths([shim.clone()].into_iter().chain(env::split_paths(&path))) {
                cmd.env("PATH", p);
            }
            cmd.env("BRIGD_BIN", bin);
            if shell.ends_with("zsh") {
                // zsh rcs often prepend PATH; the wrapper rc sources the user's, then puts the shim first again.
                cmd.env("BRIGD_SHIM", &shim);
                cmd.env("BRIGD_ZDOTDIR", env::var_os("ZDOTDIR").unwrap_or_default());
                cmd.env("ZDOTDIR", crate::root().join("zsh"));
            }
        }
        match LiveAgent::spawn(cmd, &thread, &name, &cwd, Path::new("")).map(runner::register) {
            Ok(live) => {
                let b = self.pane_body(&self.thread);
                let _ = live.resize(b.height.max(1), b.width.max(1));
                if let Some(t) = brigd_thread {
                    self.shell_thread.insert((thread.clone(), name.clone()), t);
                }
                push_tab(self.pane(), Tab::Term { thread, agent: name, stage: None, live });
                self.focus_main = true;
            }
            Err(e) => self.flash(format!("{name}: {e}")),
        }
    }

    /// Selects and opens a thread's FLOWPLAN viewer.
    fn open_flowplan(&mut self, thread: &str) {
        let plan = crate::thread_dir(thread).join("FLOWPLAN");
        if let Some(i) = self.rows.iter().position(|r| r.kind == Kind::Output && r.path == plan) {
            self.sel = i;
            self.activate(i, false);
        }
    }

    fn open_view(&mut self, thread: String, path: PathBuf) {
        let p = self.tabs.entry(thread).or_default();
        if let Some((j, i)) = p.find(|t| matches!(t, Tab::View { path: p, .. } if *p == path)) {
            (p.focus, p.list[j].active) = (j, i);
        } else {
            let title = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let title = if path.file_name().is_some_and(|f| f == "FLOWPLAN") { format!("{title} FLOWPLAN") } else { title };
            let tab = match tree::diff_view(&path) {
                Some((rel, text)) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let agent = path.ancestors().find(|a| a.ends_with(".diff")).and_then(|a| a.parent()?.file_name()).unwrap_or_default().to_string_lossy();
                    Tab::View { title: format!("± {agent} {name}"), text, scroll: 0, rel: Some(rel), path }
                }
                None => {
                    let text = fs::read_to_string(&path).unwrap_or_else(|e| format!("({e})"));
                    Tab::View { path, title: format!("≡ {title}"), text, scroll: 0, rel: None }
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
        let (n, focus) = self.tabs.get(key).map_or((1, 0), |p| (p.list.len(), p.focus));
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
        let Some(p) = self.tabs.get_mut(&self.thread) else { return };
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
        let shown: Vec<&Arc<LiveAgent>> = self.tabs.get(&self.thread).into_iter().flat_map(|p| &p.list).filter_map(|t| match t.list.get(t.active) {
            Some(Tab::Term { live, .. }) => Some(live),
            _ => None,
        }).collect();
        for a in live_agents().into_iter().filter(|a| !shown.iter().any(|s| Arc::ptr_eq(s, a))) {
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
        let Some(Tab::Term { live, .. }) = self.cur() else { return };
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

    /// Kills the agent, reverts its changes, keeps its output (crate::terminate), and says how it went.
    /// PAUSE: ESC to every live agent of the thread (flow agents, and the shell tabs strays
    /// run in). RESUME: "continue" + Enter to the same set.
    fn toggle_pause(&mut self, thread: &str) {
        let resume = self.paused.contains(thread);
        let strays = self.tree.iter().any(|n| has_live_stray(n, thread));
        let mut targets = live_agents().into_iter().filter(|a| a.thread == thread).collect::<Vec<_>>();
        if strays {
            // ponytail: a shell tab is matched to the thread by $BRIGD_THREAD, not to the exact claude inside it
            targets.extend(self.live_shells().into_iter().filter(|(_, t)| t == thread).map(|(a, _)| a));
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
        reg.iter().filter(|(_, a)| a.alive()).filter_map(|(k, a)| Some((a.clone(), self.shell_thread.get(k)?.clone()))).collect()
    }

    fn terminate(&mut self, thread: &str, agent: &str) {
        match crate::terminate(thread, agent) {
            Ok(n) => self.flash(format!("terminated {agent}: reverted {n} file(s), output kept")),
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
        crate::stop_thread(thread);
        self.live_shells().into_iter().filter(|(_, t)| t == thread).for_each(|(a, _)| a.kill());
        self.tabs.remove(thread);
        if self.thread == thread {
            self.thread.clear(); // not a thread any more: reflow re-picks the first one
            self.reflow();
        }
        self.focus_main &= self.cur().is_some();
        self.paused.remove(thread);
    }

    fn archive(&mut self, thread: &str) {
        let to = crate::archive_dir(&crate::root(), thread);
        if to.exists() {
            return self.flash(format!("archive {thread}: {} already exists", to.display()));
        }
        self.stop_thread(thread);
        match crate::archive_thread(&crate::root(), thread) {
            Ok(to) => self.flash(format!("archived {thread} to {} (its worktrees stay put)", to.display())),
            Err(e) => self.flash(format!("archive {thread}: {e}")),
        }
        self.refresh();
    }

    fn delete(&mut self, thread: &str) {
        self.stop_thread(thread);
        match crate::delete_thread(&crate::root(), thread) {
            Ok(()) => self.flash(format!("deleted {thread}")),
            Err(e) => self.flash(format!("delete {thread}: {e}")),
        }
        self.refresh();
    }

    fn tx(&mut self) -> mpsc::Sender<Bg> {
        self.bg.get_or_insert_with(mpsc::channel).0.clone()
    }

    /// Enter in the new-thread window: reserves the name, then plans off the UI thread.
    fn submit_newt(&mut self, repo: PathBuf, name: String, task: String) {
        let dir = crate::thread_dir(&name);
        // create_dir reserves the name; tree::build hides the dir until flowmap.json exists.
        let reserve = || -> Res<()> {
            crate::check_new_thread(&name)?;
            if task.is_empty() {
                return Err("task is empty".into());
            }
            fs::create_dir_all(crate::root().join("threads"))?;
            Ok(fs::create_dir(&dir)?)
        };
        if let Err(e) = reserve() {
            let focus_task = !task.is_empty() || name.is_empty();
            self.newt = Some(Newt { repo, name, task, focus_task, err: Some(e.to_string()), ..Default::default() });
            return;
        }
        self.newt = Some(Newt { repo: repo.clone(), name: name.clone(), task: task.clone(), focus_task: true, planning: true, ..Default::default() });
        self.planning.insert(name.clone());
        self.flash(format!("planning {name}…"));
        let tx = self.tx();
        // ponytail: no cancel; quitting brigd mid-plan orphans the `claude -p` planner.
        std::thread::spawn(move || {
            let r = std::panic::catch_unwind(|| {
                crate::plan(&task, &repo).and_then(|flow| {
                    crate::create_thread(&crate::root(), &name, &repo.to_string_lossy(), &flow)?;
                    Ok(fs::write(dir.join("FLOWPLAN"), crate::flow_text(&flow))?)
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
                    Some(n) if n.planning && n.name == name => (n.planning, n.hidden, n.err) = (false, false, Some(e)),
                    _ => self.flash(format!("plan {name}: {e}")),
                }
            }
            Bg::Planned(name, repo) => {
                self.planning.remove(&name);
                if self.newt.as_ref().is_some_and(|n| n.planning && n.name == name) {
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

    /// y on a planned thread: the same execute as --flow / resume, off the UI thread.
    fn run_planned(&mut self, name: String) {
        match crate::load(&name) {
            Ok((flow, state)) => {
                let tx = self.tx();
                std::thread::spawn(move || {
                    let r = crate::execute(&name, &flow, state).map_err(|e| e.to_string());
                    let _ = tx.send(Bg::Finished(name, r));
                });
            }
            Err(e) => self.flash(e.to_string()),
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        // The new-thread window and its run confirm come first: no key may reach anything behind them.
        if let Some(n) = self.newt.as_mut().filter(|n| !n.hidden) {
            if n.planning {
                // Read-only while planning; Esc only hides the box.
                n.hidden = k.code == KeyCode::Esc;
                return;
            }
            n.err = None;
            match k.code {
                KeyCode::Esc => self.newt = None,
                KeyCode::Tab | KeyCode::Up | KeyCode::Down => n.focus_task = !n.focus_task,
                KeyCode::Enter if !n.focus_task => n.focus_task = true,
                KeyCode::Enter => {
                    let n = self.newt.take().unwrap();
                    self.submit_newt(n.repo, n.name.trim().into(), n.task.trim().into());
                }
                KeyCode::Backspace => _ = if n.focus_task { &mut n.task } else { &mut n.name }.pop(),
                KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if n.focus_task { &mut n.task } else { &mut n.name }.push(c)
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
                KeyCode::Char('c') => self.msg = Some((format!("theme: {}", next_theme()), Instant::now())),
                KeyCode::Char('q') => self.ask_quit(),
                _ => {}
            }
            return;
        }
        if ctrl_o {
            self.prefix = true;
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
            Some(Tab::View { text, scroll, .. }) => {
                let max = text.lines().count().saturating_sub(1) as i32;
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
                *scroll = (*scroll as i32 + d).clamp(0, max) as u16;
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
            KeyCode::Right | KeyCode::Char('l') if sel == Some((true, false)) => self.toggle(self.sel),
            KeyCode::Left | KeyCode::Char('h') if sel == Some((true, true)) => self.toggle(self.sel),
            KeyCode::Tab => self.focus_main = self.cur().is_some(),
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if self.newt.as_ref().is_some_and(|n| !n.hidden) || self.confirm_run.is_some() {
            return;
        }
        let at = Position { x: m.column, y: m.row };
        if matches!(m.kind, MouseEventKind::Down(_)) {
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
                let Some((i, _)) = hit(self.side_inner, self.offset, &self.rows, m.column, m.row) else { return };
                let r = &self.rows[i];
                let items = match r.kind {
                    Kind::Agent => vec![Act::Terminate { thread: r.thread.clone(), agent: tree::agent_id(&r.path, &r.label) }],
                    Kind::Thread => vec![Act::Delete(r.label.clone()), Act::Archive(r.label.clone())],
                    _ => return,
                };
                self.sel = i;
                let (w, h) = (items.iter().map(|a| a.label().len()).max().unwrap_or(0) as u16, items.len() as u16);
                let x = if self.screen.width == 0 { m.column } else { m.column.min(self.screen.right().saturating_sub(w)) };
                let y = if self.screen.height == 0 { m.row } else { m.row.min(self.screen.bottom().saturating_sub(h)) };
                self.menu = Some(Menu { rect: Rect::new(x, y, w, h), items, sel: 0 });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.resizing = self.divider.contains(at);
                // The sidebar is not among these, so a drag starting there selects nothing.
                let panes = self.tabs.get(&self.thread).into_iter().flat_map(|p| &p.list).flat_map(|t| [t.bar, t.body]);
                self.drag = panes.chain([self.hint]).find(|r| r.contains(at)).map(|r| (r, at, None));
                if let Some(repo) = self.plus_btns.iter().find(|(r, _)| r.contains(at)).map(|(_, p)| p.clone()) {
                    self.prefix = false;
                    // A hidden box still planning is tracked in `newt`: replacing it would lose its inputs on failure.
                    if let Some(n) = self.newt.as_mut().filter(|n| n.planning) {
                        n.hidden = false;
                    } else {
                        self.newt = Some(Newt { repo, ..Default::default() });
                    }
                } else if let Some(t) = self.pause_btns.iter().find(|(r, _)| r.contains(at)).map(|(_, t)| t.clone()) {
                    self.toggle_pause(&t);
                } else if let Some((i, arrow)) = hit(self.side_inner, self.offset, &self.rows, m.column, m.row) {
                    self.sel = i;
                    self.activate(i, arrow);
                    self.focus_main = false; // a click shows the item but keeps j/k on the sidebar
                } else if let Some(p) = self.tabs.get_mut(&self.thread) {
                    // A click on a pane focuses it; on one of its tabs, also shows that tab.
                    if let Some(j) = p.list.iter().position(|t| t.bar.contains(at) || t.body.contains(at)) {
                        let t = &mut p.list[j];
                        if let Some(&(_, _, i)) = t.spans.iter().find(|&&(a, b, _)| t.bar.contains(at) && (a..b).contains(&m.column)) {
                            t.active = i;
                        }
                        if !t.list.is_empty() {
                            (p.focus, self.focus_main) = (j, true);
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.resizing {
                    self.side_w = Some(m.column.saturating_sub(self.side.x).clamp(MIN_SIDE_W, SIDE_W));
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
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown | MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                let back = matches!(m.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft);
                let d: i32 = if back { -3 } else { 3 };
                if self.side.contains(at) {
                    let max = max_offset(&self.rows, self.side_inner.height as usize) as i32;
                    self.offset = (self.offset as i32 + d).clamp(0, max.max(0)) as usize;
                } else if let Some(t) = self.tabs.get_mut(&self.thread).and_then(|p| p.list.iter_mut().find(|t| t.body.contains(at))) {
                    match t.list.get_mut(t.active) {
                        Some(Tab::Term { live, .. }) => {
                            let mut p = live.parser.lock().unwrap();
                            let s = p.screen().scrollback() as i32;
                            p.screen_mut().set_scrollback((s - d).max(0) as usize); // wheel up = further back
                        }
                        Some(Tab::View { text, scroll, .. }) => {
                            *scroll = (*scroll as i32 + d).clamp(0, text.lines().count().saturating_sub(1) as i32) as u16
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
            if !n.planning {
                if n.focus_task { &mut n.task } else { &mut n.name }.push_str(&s.replace(['\r', '\n'], " "));
            }
            return;
        }
        if self.confirm_run.is_some() || !self.focus_main {
            return;
        }
        let Some(Tab::Term { live, .. }) = self.cur() else { return };
        let bracketed = live.parser.lock().unwrap().screen().bracketed_paste();
        let bytes = if bracketed { format!("\x1b[200~{s}\x1b[201~") } else { s.replace('\n', "\r") };
        self.send(bytes.as_bytes());
    }

    fn row_line(&self, r: &Row, selected: bool, width: usize) -> Line<'static> {
        let arrow = match (r.folder, r.open) {
            (false, _) => "  ",
            (true, true) => "▾ ",
            (true, false) => "▸ ",
        };
        let mut spans = vec![Span::raw(format!("{}{arrow}", " ".repeat(r.depth * INDENT)))];
        let mut label = Style::new();
        match r.kind {
            Kind::Agent => {
                let st = self.agent_status.get(&(r.thread.clone(), tree::agent_id(&r.path, &r.label))).copied().unwrap_or("off");
                let (g, style) = match st {
                    "running" => ("● ", Style::new().fg(pal().green)),
                    "idle" => ("◦ ", Style::new().fg(pal().green)),
                    "blocked" => ("! ", Style::new().fg(pal().red).add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)),
                    "done" => ("✓ ", Style::new().fg(pal().blue)),
                    "failed" => ("✗ ", Style::new().fg(pal().red)),
                    _ => ("○ ", Style::new().fg(pal().overlay0)),
                };
                label = if st == "blocked" { style } else { label.fg(stage_color(r.stage)) };
                spans.push(Span::styled(g, style));
                if let Some(s) = r.stage {
                    spans.push(Span::styled(format!("{}·", s + 1), Style::new().fg(stage_color(r.stage))));
                }
            }
            Kind::Terminal => {
                spans.push(Span::styled("$ ", Style::new().fg(pal().green)));
                label = label.fg(pal().text);
            }
            Kind::Output => label = label.fg(pal().sky),
            Kind::Diff => label = label.fg(pal().yellow),
            Kind::Space => label = label.fg(pal().mauve).add_modifier(Modifier::BOLD),
            Kind::Thread => label = label.fg(pal().text).add_modifier(Modifier::BOLD),
            _ => label = label.fg(pal().subtext0),
        }
        let slash = if matches!(r.kind, Kind::Output | Kind::Diff | Kind::Terminal) { "" } else { "/" };
        spans.push(Span::styled(format!("{}{slash}", r.label), label));
        // Worktree name (agents) and +add -del (diffs) sit flush right, one column of padding.
        let mut right = vec![];
        if let Some(wt) = &r.worktree {
            right.push(Span::styled(wt.clone(), Style::new().fg(pal().overlay0)));
        }
        if let Some((add, del)) = (r.kind == Kind::Diff).then(|| tree::diff_stat(&r.path)).flatten() {
            right.push(Span::styled(format!("+{add}"), Style::new().fg(pal().green)));
            right.push(Span::raw(" "));
            right.push(Span::styled(format!("-{del}"), Style::new().fg(pal().red)));
        }
        if !right.is_empty() {
            let used: usize = spans.iter().chain(&right).map(|s| s.content.chars().count()).sum();
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used + 1).max(1))));
            spans.extend(right);
        }
        if r.kind == Kind::Thread {
            let st = self.thread_status.get(&r.path).map(String::as_str).unwrap_or("");
            spans.push(Span::styled(format!(" {st}"), Style::new().fg(pal().overlay0)));
        }
        let line = Line::from(spans);
        match (selected, self.focus_main) {
            (true, false) => line.style(Style::new().add_modifier(Modifier::REVERSED)),
            (true, true) => line.style(Style::new().bg(pal().surface1)),
            // The space (repo) whose tabs show.
            _ if r.kind == Kind::Space && r.space == self.space => line.style(Style::new().bg(pal().surface0)),
            _ => line,
        }
    }

    /// Pane `j` of the shown thread: its tab bar (the thread name first on pane 0), then the active tab.
    fn draw_pane(&mut self, f: &mut Frame, j: usize, bar: Rect, body: Rect) {
        let p = &self.tabs[&self.thread];
        let (tabs, focused) = (&p.list[j], p.focus == j);
        let mut spans = vec![];
        let pill = if j == 0 {
            spans.push(Span::styled(format!(" {} ", self.thread), Style::new().fg(pal().crust).bg(pal().mauve)));
            self.thread.chars().count() as u16 + 2
        } else {
            0
        };
        let mut titles = vec![];
        for t in &tabs.list {
            titles.push(match t {
                Tab::Term { agent, live, stage, .. } if !live.alive() => (format!(" {} (exited) ", tail(agent)), Some(stage_color(*stage))),
                Tab::Term { agent, stage, .. } => (format!(" {} ", tail(agent)), Some(stage_color(*stage))),
                Tab::View { title, .. } => (format!(" {title} "), None),
            });
        }
        let widths: Vec<u16> = titles.iter().map(|(t, _)| 1 + t.chars().count() as u16).collect(); // "│" + title
        let (first, end) = tab_window(&widths, 0, Some(tabs.active), bar.width.saturating_sub(pill));
        let mut tab_spans = vec![];
        let marker = |c| Span::styled(c, Style::new().fg(pal().overlay0));
        if first > 0 {
            spans.push(marker("‹"));
        }
        let mut x = bar.x + pill + (first > 0) as u16;
        // The focused pane's active tab is reversed, the other pane's underlined.
        let on = if focused { Modifier::REVERSED | Modifier::BOLD } else { Modifier::UNDERLINED };
        for i in first..end {
            let (title, fg) = &titles[i];
            tab_spans.push((x + 1, x + widths[i], i));
            x += widths[i];
            spans.push(Span::raw("│"));
            let style = fg.map_or(Style::new(), |c| Style::new().fg(c));
            spans.push(Span::styled(title.clone(), if tabs.active == i { style.add_modifier(on) } else { style }));
        }
        if end < titles.len() {
            spans.push(marker("›"));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), bar);

        let cursor = self.focus_main && focused;
        let pad = Rect::new(body.x + 2, body.y + 1, body.width.saturating_sub(2), body.height.saturating_sub(1));
        let mut plan = tabs.list.get(tabs.active).and_then(|t| match t {
            Tab::View { path, text, rel: None, .. } => self.plan_view(path, text, pad.width as usize),
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
                f.render_widget(Paragraph::new(diff_lines(text)).wrap(Wrap { trim: false }).scroll((*scroll, 0)), d);
                f.render_widget(Paragraph::new(rel.as_str()).style(Style::new().fg(pal().overlay0)).right_aligned(), foot);
            }
            Some(Tab::View { scroll, .. }) if plan.is_some() => {
                let lines = plan.take().unwrap_or_default();
                *scroll = (*scroll).min(lines.len().saturating_sub(1) as u16);
                f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), pad);
            }
            Some(Tab::View { path, text, scroll, .. }) => {
                let p = if path.extension().is_some_and(|e| e == "md") { Paragraph::new(md_lines(text)) } else { Paragraph::new(text.as_str()) };
                f.render_widget(p.wrap(Wrap { trim: false }).scroll((*scroll, 0)), body);
            }
            None => f.render_widget(Paragraph::new("\n  Enter or click an agent to open its claude tab, or an output.md to read it."), body),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [top, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [side, divider, main] = Layout::horizontal([Constraint::Length(self.side_w.unwrap_or(SIDE_W)), Constraint::Length(1), Constraint::Min(0)]).areas(top);
        let border = if self.focus_main { pal().overlay0 } else { pal().lavender };
        // No box: a dim title row, then the tree inside a one-column margin.
        let inner = Rect::new(side.x + 1, side.y + 1, side.width.saturating_sub(2), side.height.saturating_sub(1));
        if main != self.main {
            self.main = main;
            self.resize_all();
        }
        (self.screen, self.side, self.side_inner, self.divider, self.hint) = (f.area(), side, inner, divider, hint);
        f.render_widget(Paragraph::new("spaces").style(Style::new().fg(pal().overlay0)), Rect::new(inner.x, side.y, inner.width, 1));
        f.render_widget(Paragraph::new(vec![Line::from("┃"); divider.height as usize]).style(Style::new().fg(border)), divider);

        let h = inner.height as usize;
        self.offset = self.offset.min(max_offset(&self.rows, h));
        let shown = layout(&self.rows, self.offset, h);
        let mut lines: Vec<Line> = vec![];
        for &(y, i) in &shown {
            lines.resize(y, Line::default());
            lines.push(self.row_line(&self.rows[i], i == self.sel, inner.width as usize));
        }
        if lines.is_empty() {
            f.render_widget(Paragraph::new("no threads yet\n\nbrigd <name> \"task\""), inner);
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
            let rect = Rect::new(inner.right().saturating_sub(w).max(inner.x), inner.y + y as u16, w.min(inner.width), 1);
            f.render_widget(Paragraph::new(label).style(Style::new().fg(color)), rect);
            if r.kind == Kind::Space {
                self.plus_btns.push((rect, r.path.clone()));
            } else {
                self.pause_btns.push((rect, r.label.clone()));
            }
        }

        // The thread's panes side by side, a dim "│" between them.
        let rects = pane_rects(main, self.tabs.entry(self.thread.clone()).or_default().list.len());
        if let [(l, _), _] = rects[..] {
            f.render_widget(Paragraph::new(vec![Line::from("│"); main.height as usize]).style(Style::new().fg(pal().surface1)), Rect::new(l.right(), main.y, 1, main.height));
        }
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
            _ if !self.focus_main => Span::raw("↑↓/jk move · Enter open/fold · ←→ fold · Tab back to tab · M-h/l tabs · M-t terminal · q quit · wheel scrolls"),
            _ if viewing => Span::raw("↑↓/jk PgUp/PgDn g/G scroll · Tab/Esc sidebar · M-h/l tabs · M-t terminal · C-o w close · C-o q quit"),
            _ => Span::raw("keys go to the tab · C-o s sidebar · M-h/l tabs · M-t terminal · C-o w close (keeps running) · C-o q quit · wheel scrollback"),
        };
        f.render_widget(Paragraph::new(Line::from(text).style(Style::new().fg(pal().subtext0).bg(pal().mantle))), hint);

        if let Some(m) = &self.menu {
            let r = m.rect.intersection(f.area());
            f.render_widget(Clear, r);
            let lines = m.items.iter().enumerate().map(|(i, a)| {
                let bg = if i == m.sel { pal().red } else { pal().surface1 };
                Line::styled(a.label(), Style::new().fg(if i == m.sel { pal().crust } else { pal().text }).bg(bg).add_modifier(Modifier::BOLD))
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
            for (label, text, on) in [("name: ", &n.name, !n.focus_task), ("task: ", &n.task, n.focus_task)] {
                let cursor = if on && !n.planning { "█" } else { "" };
                let style = if n.planning { dim } else { Style::new() };
                lines.extend(chunk(&format!("{label}{text}{cursor}"), inner).into_iter().map(|l| Line::styled(l, style)));
            }
            let (status, style) = if n.planning {
                (format!("{} planning {}… · Esc hides", SPIN[self.ticks as usize % SPIN.len()], n.name), Style::new().fg(pal().yellow))
            } else if let Some(e) = &n.err {
                (e.clone(), Style::new().fg(pal().red))
            } else {
                ("Tab/↑↓ switch · Enter next/plans · Esc cancels".into(), dim)
            };
            lines.extend(chunk(&status, inner).into_iter().map(|l| Line::styled(l, style)));
            Some((format!(" new thread in {} ", n.repo.display()), lines, w))
        } else {
            self.confirm_run.as_ref().map(|(name, repo)| {
                let l = Line::styled(format!("Run thread {name} in {repo}? y / n"), Style::new().add_modifier(Modifier::BOLD));
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
            f.render_widget(Paragraph::new(lines).scroll((skip, 0)).wrap(Wrap { trim: false }).block(Block::bordered().title(title).border_style(Style::new().fg(pal().lavender))), r);
        }

        if let Some((r, a, Some(b))) = self.drag {
            self.buf = f.buffer_mut().clone();
            for (x, y) in sel_rows(r, a, b).into_iter().flat_map(|(y, xs)| xs.map(move |x| (x, y))) {
                if let Some(c) = f.buffer_mut().cell_mut((x, y)) {
                    c.modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }
}

/// Does `thread` have a stray whose claude is still running?
fn has_live_stray(n: &Node, thread: &str) -> bool {
    let strays = |t: &Node| t.children.iter().filter(|c| c.kind == Kind::Folder).flat_map(|c| &c.children).any(|a| a.kind == Kind::Agent && !matches!(stray_status(&a.path), "off" | "done" | "failed"));
    if n.kind == Kind::Thread { n.label == thread && strays(n) } else { n.children.iter().any(|c| has_live_stray(c, thread)) }
}

/// Lists each thread's live terminals (`terms`: (thread, shell name)) in a TERMS folder, after its
/// FLOWPLAN if it has one, else first.
fn add_terminals(tree: &mut [Node], terms: &[(String, String)]) {
    for t in tree.iter_mut().flat_map(|s| &mut s.children) {
        let mut names: Vec<&str> = terms.iter().filter(|(th, _)| *th == t.label).map(|(_, n)| n.as_str()).collect();
        if names.is_empty() {
            continue;
        }
        names.sort();
        let dir = t.path.join(".terminal");
        let node = |kind, label: &str, path, children| Node { kind, label: label.into(), path, children, stage: None, worktree: None };
        let kids = names.into_iter().map(|n| node(Kind::Terminal, n, dir.join(n), vec![])).collect();
        let at = t.children.iter().position(|c| c.kind == Kind::Output && c.path.ends_with("FLOWPLAN")).map_or(0, |i| i + 1);
        t.children.insert(at, node(Kind::Folder, tree::TERMS, dir, kids));
    }
}

/// A stray tab shows "stray-1", not "stray agents/stray-1".
fn tail(agent: &str) -> &str {
    agent.rsplit('/').next().unwrap_or(agent)
}

/// A stray running in a terminal tab is no LiveAgent: it is alive while the pid it saved
/// (exec kept brigd's) runs; the pid file goes once it is gone. Status from its hooks.
fn stray_status(dir: &Path) -> &'static str {
    // Gone: terminated by Mark (failed) or just exited (off).
    let gone = || if dir.join(runner::TERMINATED).exists() { "failed" } else { "off" };
    let Ok(pid) = fs::read_to_string(dir.join("pid")) else { return gone() };
    if !runner::pid_alive(&pid) {
        let _ = fs::remove_file(dir.join("pid"));
        return gone();
    }
    let cwd = fs::read_to_string(dir.join("cwd")).unwrap_or_default();
    agent_glyph_status(Some(&runner::read_status(dir, Path::new(cwd.trim()))), "")
}

/// The first `term-<n>` not in `taken`.
fn fresh_name<'a>(taken: impl Iterator<Item = &'a str>) -> String {
    let taken: HashSet<&str> = taken.collect();
    (1..).map(|n| format!("term-{n}")).find(|n| !taken.contains(n.as_str())).unwrap()
}

/// Unified diff → colored lines, one per source line: + green, - red, @@ cyan.
fn diff_lines(text: &str) -> Vec<Line<'_>> {
    text.lines()
        .map(|l| match l.as_bytes().first() {
            Some(b'+') => Line::styled(l, Style::new().fg(pal().green)),
            Some(b'-') => Line::styled(l, Style::new().fg(pal().red)),
            Some(b'@') if l.starts_with("@@") => Line::styled(l, Style::new().fg(pal().sky).add_modifier(Modifier::DIM)),
            _ => Line::raw(l),
        })
        .collect()
}

/// Markdown → styled lines, one per source line so scroll bounds match `text.lines()`.
fn md_lines(text: &str) -> Vec<Line<'static>> {
    let (mut code, dim) = (false, Style::new().fg(pal().overlay0));
    text.lines()
        .map(|l| {
            let t = l.trim_start();
            let ind = &l[..l.len() - t.len()];
            if t.starts_with("```") {
                code = !code;
                return Line::styled("─".repeat(40), dim);
            }
            if code {
                return Line::styled(l.to_string(), Style::new().fg(pal().peach).bg(pal().surface0));
            }
            if let Some(n) = (1..=6).find(|&n| t.starts_with(&format!("{} ", "#".repeat(n)))) {
                let c = [pal().mauve, pal().sky, pal().blue][(n - 1).min(2)];
                return Line::from(inline(&t[n + 1..], Style::new().fg(c).add_modifier(Modifier::BOLD)));
            }
            if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_')) {
                return Line::styled("─".repeat(40), dim);
            }
            if let Some(q) = t.strip_prefix('>') {
                let mut v = vec![Span::raw(ind.to_string()), Span::styled("│ ", dim)];
                v.extend(inline(q.trim_start(), Style::new().fg(pal().subtext0).add_modifier(Modifier::ITALIC)));
                return Line::from(v);
            }
            let num = t.find(". ").filter(|&i| i > 0 && t[..i].bytes().all(|b| b.is_ascii_digit()));
            let (mark, rest) = if let Some(r) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
                ("•".to_string(), r)
            } else if let Some(i) = num {
                (t[..=i].to_string(), &t[i + 2..])
            } else {
                return Line::from([vec![Span::raw(ind.to_string())], inline(t, Style::new())].concat());
            };
            Line::from([vec![Span::raw(ind.to_string()), Span::styled(mark + " ", Style::new().fg(pal().yellow))], inline(rest, Style::new())].concat())
        })
        .collect()
}

/// Inline `code`, **bold**, *italic*/_italic_, [text](url) spans.
fn inline(s: &str, base: Style) -> Vec<Span<'static>> {
    let (mut out, mut buf, mut rest) = (Vec::new(), String::new(), s);
    let flush = |buf: &mut String, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            out.push(Span::styled(std::mem::take(buf), base));
        }
    };
    while let Some(c) = rest.chars().next() {
        let r = &rest[c.len_utf8()..];
        let close = |pat: &str, from: &str| from.find(pat).filter(|&e| e > 0 && !from.starts_with(' '));
        if c == '`' {
            if let Some(e) = r.find('`') {
                flush(&mut buf, &mut out);
                out.push(Span::styled(r[..e].to_string(), Style::new().fg(pal().peach).bg(pal().surface0)));
                rest = &r[e + 1..];
                continue;
            }
        } else if rest.starts_with("**") {
            if let Some(e) = close("**", &rest[2..]) {
                flush(&mut buf, &mut out);
                out.extend(inline(&rest[2..2 + e], base.add_modifier(Modifier::BOLD)));
                rest = &rest[4 + e..];
                continue;
            }
        } else if c == '*' || (c == '_' && !buf.chars().last().is_some_and(char::is_alphanumeric)) {
            if let Some(e) = close(&c.to_string(), r) {
                flush(&mut buf, &mut out);
                out.extend(inline(&r[..e], base.add_modifier(Modifier::ITALIC)));
                rest = &r[e + 1..];
                continue;
            }
        } else if c == '[' {
            if let Some((e, u)) = r.find("](").and_then(|e| r[e + 2..].find(')').map(|u| (e, e + 2 + u))) {
                flush(&mut buf, &mut out);
                out.extend(inline(&r[..e], base.fg(pal().sky).add_modifier(Modifier::UNDERLINED)));
                rest = &r[u + 1..];
                continue;
            }
        }
        buf.push(c);
        rest = r;
    }
    flush(&mut buf, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    fn one_pane(list: Vec<Tab>) -> Panes {
        Panes { list: vec![Tabs { list, ..Default::default() }], focus: 0 }
    }

    #[test]
    fn live_status_beats_saved() {
        assert_eq!(agent_glyph_status(Some("working"), "done"), "running");
        assert_eq!(agent_glyph_status(Some("idle"), "done"), "idle");
        assert_eq!(agent_glyph_status(Some("blocked"), "failed"), "blocked");
        assert_eq!(agent_glyph_status(None, "done"), "done");
        assert_eq!(agent_glyph_status(None, ""), "off");
    }

    #[test]
    fn keys_to_bytes() {
        let none = KeyModifiers::NONE;
        assert_eq!(key_bytes(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false), [3]);
        assert_eq!(key_bytes(key(KeyCode::Char('A'), KeyModifiers::SHIFT), false), b"A");
        assert_eq!(key_bytes(key(KeyCode::Char('é'), none), false), "é".as_bytes());
        assert_eq!(key_bytes(key(KeyCode::Enter, none), false), b"\r");
        assert_eq!(key_bytes(key(KeyCode::Enter, KeyModifiers::ALT), false), b"\x1b\r");
        assert_eq!(key_bytes(key(KeyCode::Backspace, none), false), [0x7f]);
        assert_eq!(key_bytes(key(KeyCode::Up, none), false), b"\x1b[A");
        assert_eq!(key_bytes(key(KeyCode::Up, none), true), b"\x1bOA");
        assert_eq!(key_bytes(key(KeyCode::BackTab, KeyModifiers::SHIFT), false), b"\x1b[Z");
        assert_eq!(key_bytes(key(KeyCode::Esc, KeyModifiers::ALT), false), [0x1b]);
        assert_eq!(key_bytes(key(KeyCode::F(1), none), false), b"\x1bOP");
        assert_eq!(key_bytes(key(KeyCode::F(12), none), false), b"\x1b[24~");
        assert_eq!(key_bytes(key(KeyCode::CapsLock, none), false), b"");
    }

    #[test]
    fn markdown_lines() {
        let l = md_lines("# Title\nsome **bold** x\n```\n**raw**\n```");
        assert_eq!(l.len(), 5); // one rendered line per source line
        assert_eq!(l[0].spans[0].content, "Title");
        assert!(l[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        let b = l[1].spans.iter().find(|s| s.content == "bold").unwrap();
        assert!(b.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(l[3].spans[0].content, "**raw**"); // no inline parsing in code
    }

    #[test]
    fn theme_names_resolve() {
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(theme_index(t.name), i);
        }
        assert_eq!(THEMES[theme_index("nope")].name, "catppuccin-mocha");
    }

    #[test]
    fn diff_colors() {
        let l = diff_lines("@@ -1,2 +1,2 @@\n a\n-b\n+c");
        let fg: Vec<_> = l.iter().map(|l| l.style.fg).collect();
        assert_eq!(fg, [Some(pal().sky), None, Some(pal().red), Some(pal().green)]);
    }

    #[test]
    fn click_hits_tree_rows() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None, worktree: None };
        let agent = node(Kind::Agent, "a", vec![node(Kind::Output, "output.md", vec![])]);
        let tree = node(Kind::Space, "repo", vec![node(Kind::Thread, "t", vec![agent, node(Kind::Agent, "b", vec![])])]);
        let mut rows = Vec::new();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut rows);
        // Spaces and threads start open, agents folded.
        let labels: Vec<_> = rows.iter().map(|r| (r.depth, r.label.as_str(), r.thread.as_str())).collect();
        assert_eq!(labels, [(0, "repo", ""), (1, "t", "t"), (2, "a", "t"), (2, "b", "t")]);
        let inner = Rect::new(1, 1, 34, 3); // lines 1..=3: repo, t, a
        assert_eq!(hit(inner, 0, &rows, 5, 1), Some((0, false)));
        assert_eq!(hit(inner, 1, &rows, 10, 2), Some((2, false))); // scrolled by one
        assert_eq!(hit(inner, 0, &rows, 1 + 6, 3), Some((2, true))); // depth 2 arrow at x 6..8
        assert_eq!(hit(inner, 0, &rows, 1 + 8, 3), Some((2, false)));
        assert_eq!(hit(inner, 1, &rows, 1 + 6, 3), Some((3, false))); // "b" has no children: no arrow
        assert_eq!(hit(inner, 2, &rows, 5, 3), None); // past the last row
        assert_eq!(hit(inner, 0, &rows, 5, 4), None); // below the list
        assert_eq!(hit(inner, 0, &rows, 0, 1), None); // on the border
        assert_eq!(max_offset(&rows, 3), 1);
        // A blank line above a later thread and a later space, none under a space.
        let two = node(Kind::Space, "r", vec![node(Kind::Thread, "t", vec![]), node(Kind::Thread, "u", vec![])]);
        let mut rows = Vec::new();
        flatten(&two, 0, &two.path, "", &HashSet::new(), &mut rows);
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut rows);
        let ys: Vec<_> = layout(&rows, 0, 20).into_iter().map(|(y, _)| y).collect();
        assert_eq!(ys, [0, 1, 3, 5, 6, 7, 8]); // r t _ u _ repo t a b
        // Unfolding agent "a" shows its output.md.
        let toggled = HashSet::from([(tree.path.clone(), PathBuf::from("/a"))]);
        let mut rows = Vec::new();
        flatten(&tree, 0, &tree.path, "", &toggled, &mut rows);
        assert_eq!((rows[3].kind.clone(), rows[3].depth), (Kind::Output, 3));
    }

    #[test]
    fn alt_hl_cycles_tabs() {
        let view = || Tab::View { path: PathBuf::new(), title: String::new(), text: String::new(), scroll: 0, rel: None };
        let mut app = App::default();
        app.tabs.insert(String::new(), one_pane(vec![view(), view()]));
        app.on_key(key(KeyCode::Char('l'), KeyModifiers::ALT)); // from the sidebar
        assert_eq!((app.tabs[""].list[0].active, app.focus_main), (1, true));
        app.on_key(key(KeyCode::Char('˙'), KeyModifiers::NONE));
        assert_eq!(app.tabs[""].list[0].active, 0);
    }

    #[test]
    fn tab_bar_follows_active_tab() {
        let view = |n: usize| Tab::View { path: PathBuf::new(), title: format!("tab-number-{n}"), text: String::new(), scroll: 0, rel: None };
        let mut app = App::default();
        app.tabs.insert(String::new(), one_pane((0..10).map(view).collect()));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 10)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.tabs[""].list[0].spans[0].2, 0);
        for _ in 0..9 {
            app.on_key(key(KeyCode::Char('l'), KeyModifiers::ALT));
        }
        term.draw(|f| app.draw(f)).unwrap();
        let t = &app.tabs[""].list[0];
        let (first, (a, b, last)) = (t.spans[0].2, *t.spans.last().unwrap());
        assert!(first > 0 && last == 9); // the last tab is shown, fully
        assert!(b <= t.bar.right() && b - a == "tab-number-9".len() as u16 + 2);
        // A click on a tab maps through the scrolled window.
        let (a, _, i) = t.spans[0];
        app.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: a, row: 0, modifiers: KeyModifiers::NONE });
        assert_eq!(app.tabs[""].list[0].active, i);
    }

    #[test]
    fn flowplan_view_draws_step_tree() {
        let agent = |name: &str, wt: Option<&str>| crate::Agent {
            name: name.into(),
            model: "haiku".into(),
            effort: "low".into(),
            worktree: wt.map(String::from),
            task: format!("task of {name}\nmore"),
            files: vec![],
            mode: "default".into(),
        };
        let flow = crate::Flow { goal: "ship it".into(), stages: vec![vec![agent("check-db", None), agent("trace-auth", Some("login"))], vec![agent("review", None)]] };
        let text = crate::flow_text(&flow);
        assert!(text.contains("│  ├─ ◇ check-db  ") && text.ends_with("│\n└  log\n"), "{text}");

        let path = PathBuf::from("/th/FLOWPLAN");
        let mut state = crate::State { thread: "th".into(), ..Default::default() };
        state.agents.insert("check-db".into(), crate::AgentState { status: "done".into() });
        let mut app = App::default();
        app.agent_status.insert(("th".into(), "trace-auth".into()), "running");
        app.plans.insert(path.clone(), (flow, state));
        let tab = Tab::View { path, title: String::new(), text: text + "   start check-db in /repo\n", scroll: 0, rel: None };
        app.tabs.insert(String::new(), one_pane(vec![tab]));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let b = term.backend().buffer();
        let rows: Vec<String> = (0..b.area.height).map(|y| (0..b.area.width).map(|x| b.cell((x, y)).map_or(" ", |c| c.symbol())).collect()).collect();
        let has = |s: &str| rows.iter().any(|r| r.contains(s));
        assert!(has("◆  ship it") && has("│  ├─ ✔ check-db") && has("│  │    task of check-db"), "{}", rows.join("\n"));
        assert!(SPIN.iter().any(|f| has(&format!("│  └─ {f} trace-auth"))) && has("wt login"));
        assert!(SPIN.iter().any(|f| has(&format!("{f}  stage 1 · 2 parallel"))) && has("◇  stage 2 · 1 agent"));
        assert!(has("└  log") && has("start check-db in /repo"));
    }

    #[test]
    fn tab_cap_evicts_oldest() {
        let mut app = App::default();
        let paths = |t: &App, th: &str| t.tabs[th].list[0].list.iter().map(|t| if let Tab::View { path, .. } = t { path.clone() } else { PathBuf::new() }).collect::<Vec<_>>();
        for i in 1..=6 {
            app.open_view("t".into(), PathBuf::from(format!("/nope/{i}")));
        }
        let want: Vec<_> = (2..=6).map(|i| PathBuf::from(format!("/nope/{i}"))).collect();
        assert_eq!((paths(&app, "t"), app.tabs["t"].list[0].active), (want.clone(), 4));
        app.open_view("t".into(), PathBuf::from("/nope/2")); // already open: only activated
        assert_eq!((paths(&app, "t"), app.tabs["t"].list[0].active), (want.clone(), 0));
        app.open_view("u".into(), PathBuf::from("/nope/7"));
        assert_eq!((paths(&app, "t"), app.tabs["u"].list[0].list.len()), (want, 1));
    }

    #[test]
    fn split_gives_new_pane_own_tab_bar() {
        let main = Rect::new(41, 0, 79, 19);
        let widths: Vec<_> = pane_rects(main, 2).iter().map(|(bar, body)| (bar.x, bar.width, body.y, body.height)).collect();
        assert_eq!(widths, [(41, 39, 1, 18), (81, 39, 1, 18)]);
        let mut app = App { thread: "t".into(), ..Default::default() };
        app.open_view("t".into(), PathBuf::from("/nope/1"));
        app.open_view("t".into(), PathBuf::from("/nope/2"));
        let p = app.tabs.get_mut("t").unwrap();
        p.list.push(Tabs::default()); // what split does, then new_term; a viewer stands in for the shell
        p.focus = 1;
        app.open_view("t".into(), PathBuf::from("/nope/3"));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let p = &app.tabs["t"];
        assert_eq!(p.list.iter().map(|t| (t.list.len(), t.bar, t.spans.len())).collect::<Vec<_>>(), [(2, pane_rects(main, 2)[0].0, 2), (1, pane_rects(main, 2)[1].0, 1)]);
        assert_eq!(term.backend().buffer().cell((80, 5)).unwrap().symbol(), "│");
        app.split();
        assert_eq!((app.tabs["t"].list.len(), app.msg.as_ref().unwrap().0.as_str()), (2, "max 2 panes"));
        app.open_view("t".into(), PathBuf::from("/nope/1")); // open in pane 0: focused there, not opened twice
        assert_eq!((app.tabs["t"].focus, app.tabs["t"].list[1].list.len()), (0, 1));
        app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        app.on_key(key(KeyCode::Char('o'), KeyModifiers::NONE));
        app.thread = "u".into(); // another thread and back: both panes and the focus stay
        term.draw(|f| app.draw(f)).unwrap();
        app.thread = "t".into();
        assert_eq!((app.tabs["t"].list.len(), app.tabs["t"].focus), (2, 1));
        app.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        app.on_key(key(KeyCode::Char('w'), KeyModifiers::NONE)); // pane 1's last tab: the pane goes
        let p = &app.tabs["t"];
        assert_eq!((p.list.len(), p.focus, p.list[0].list.len()), (1, 0, 2));
    }

    #[test]
    fn terminal_folder_under_thread() {
        let node = |kind, label: &str, path: &str, children| Node { kind, label: label.into(), path: PathBuf::from(path), children, stage: None, worktree: None };
        let t = node(Kind::Thread, "t", "/t", vec![node(Kind::Output, "FLOWPLAN", "/t/FLOWPLAN", vec![]), node(Kind::Folder, tree::STRAY, "/t/stray agents", vec![]), node(Kind::Agent, "a", "/t/a", vec![])]);
        let mut tree = vec![node(Kind::Space, "repo", "/repo", vec![t, node(Kind::Thread, "u", "/u", vec![node(Kind::Agent, "b", "/u/b", vec![])])])];
        let terms = [("t", "term-2"), ("t", "term-1"), ("zz", "term-3"), ("u", "term-4")].map(|(a, b)| (a.to_string(), b.to_string()));
        add_terminals(&mut tree, &terms);
        let mut rows = Vec::new();
        flatten(&tree[0], 0, &tree[0].path, "", &HashSet::new(), &mut rows);
        let labels: Vec<_> = rows.iter().map(|r| (r.label.as_str(), r.thread.as_str())).collect();
        assert_eq!(labels, [("repo", ""), ("t", "t"), ("FLOWPLAN", "t"), ("terminal", "t"), ("term-1", "t"), ("term-2", "t"), ("stray agents", "t"), ("a", "t"), ("u", "u"), ("terminal", "u"), ("term-4", "u"), ("b", "u")]);
        assert_eq!((rows[4].kind.clone(), rows[4].path.clone()), (Kind::Terminal, PathBuf::from("/t/.terminal/term-1")));
    }

    #[test]
    fn fresh_names() {
        assert_eq!(fresh_name([].into_iter()), "term-1");
        assert_eq!(fresh_name(["term-1", "a", "term-3"].into_iter()), "term-2");
    }

    #[test]
    fn right_click_opens_terminate_menu() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None, worktree: None };
        let tree = node(Kind::Space, "repo", vec![node(Kind::Thread, "t", vec![node(Kind::Agent, "a", vec![])])]);
        let mut app = App::default();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
        (app.side_inner, app.screen) = (Rect::new(1, 1, 98, 10), Rect::new(0, 0, 100, 20));
        let click = |b, x, y| MouseEvent { kind: MouseEventKind::Down(b), column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(click(MouseButton::Right, 5, 1)); // the space row: no menu
        assert!(app.menu.is_none());
        app.on_mouse(click(MouseButton::Right, 5, 3)); // the agent row
        let m = app.menu.as_ref().unwrap();
        assert_eq!((m.items.clone(), m.rect.y), (vec![Act::Terminate { thread: "t".into(), agent: "a".into() }], 3));
        app.on_mouse(click(MouseButton::Left, 50, 8)); // elsewhere: closes, opens nothing
        assert!(app.menu.is_none() && app.tabs.is_empty());
        app.on_mouse(click(MouseButton::Right, 5, 3));
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.menu.is_none());
        app.on_mouse(click(MouseButton::Right, 97, 3)); // at the screen edge: the menu is shifted left
        assert_eq!(app.menu.as_ref().unwrap().rect.right(), 100);
        app.on_mouse(click(MouseButton::Right, 5, 2)); // the thread row: Delete / Archive
        let m = app.menu.as_ref().unwrap();
        assert_eq!((m.items.clone(), m.rect.height), (vec![Act::Delete("t".into()), Act::Archive("t".into())], 2));
        app.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // Delete asks first
        assert_eq!((app.menu.is_none(), app.confirm_delete.as_deref()), (true, Some("t")));
        app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.confirm_delete.is_none());
    }

    #[test]
    fn sidebar_click_keeps_focus() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None, worktree: None };
        let tree = node(Kind::Space, "repo", vec![node(Kind::Output, "nope.md", vec![])]);
        let mut app = App::default();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
        app.side_inner = Rect::new(1, 1, 34, 10);
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(click(5, 2)); // the output row: opens a viewer tab, focus stays on the sidebar
        assert_eq!((app.tabs[""].list[0].list.len(), app.focus_main), (1, false));
        app.tabs.get_mut("").unwrap().list[0].body = Rect::new(40, 2, 40, 10);
        app.on_key(key(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.sel, 0); // j/k moved the sidebar selection
        app.on_mouse(click(50, 5)); // the body
        assert!(app.focus_main);
    }

    #[test]
    fn pause_button_toggles() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None, worktree: None };
        let mut app = App::default();
        app.pause_btns = vec![(Rect::new(28, 2, 7, 1), "t".into())];
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(click(5, 2)); // beside the button: nothing
        assert!(app.paused.is_empty());
        app.on_mouse(click(30, 2));
        assert!(app.paused.contains("t"));
        app.on_mouse(click(30, 2));
        assert!(app.paused.is_empty());
        assert!(!has_live_stray(&node(Kind::Thread, "t", vec![]), "t"));
    }

    #[test]
    fn plus_opens_new_thread_window() {
        let mut app = App { plus_btns: vec![(Rect::new(30, 1, 3, 1), PathBuf::from("/repo"))], ..Default::default() };
        app.on_mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 31, row: 1, modifiers: KeyModifiers::NONE });
        assert_eq!(app.newt, Some(Newt { repo: "/repo".into(), ..Default::default() }));
        let typ = |app: &mut App, s: &str| s.chars().for_each(|c| app.on_key(key(KeyCode::Char(c), KeyModifiers::NONE)));
        typ(&mut app, "ab");
        app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        app.on_key(key(KeyCode::Char('t'), KeyModifiers::ALT)); // no terminal opens behind the window
        assert_eq!((app.newt.as_ref().unwrap().name.as_str(), app.tabs.is_empty()), ("a", true));
        app.on_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        typ(&mut app, "Bad!");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // name -> task
        assert!(app.newt.as_ref().unwrap().focus_task);
        app.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!app.newt.as_ref().unwrap().focus_task);
        app.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        typ(&mut app, "x y");
        app.on_paste("p\nq");
        assert_eq!(app.newt.as_ref().unwrap().task, "x yp q");
        app.on_key(key(KeyCode::Enter, KeyModifiers::NONE)); // a bad name keeps the window open with an error
        let n = app.newt.as_ref().unwrap();
        assert!(n.err.as_ref().unwrap().contains("bad thread name") && !n.planning && n.task == "x yp q");
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.newt.is_none());
        // "Bad!" can never be a thread, so n's delete touches nothing on disk.
        app.confirm_run = Some(("Bad!".into(), "/repo".into()));
        app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(app.confirm_run.is_some());
        app.on_key(key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.confirm_run.is_none());
    }

    #[test]
    fn chunk_by_width() {
        assert_eq!(chunk("abcde", 2), ["ab", "cd", "e"]);
        assert_eq!(chunk("", 2), [""]);
        assert_eq!(chunk("日本語", 4), ["日本", "語"]); // wide chars are 2 columns
        assert_eq!(chunk("a\nb", 4), ["a", "b"]);
    }

    #[test]
    fn newt_planning_state() {
        let mut app = App { newt: Some(Newt { repo: "/r".into(), name: "n".into(), task: "t".into(), planning: true, ..Default::default() }), ..Default::default() };
        app.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE)); // read-only
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE)); // hides, keeps planning
        assert!(app.newt.as_ref().unwrap().hidden);
        app.on_bg(Bg::Failed("n".into(), "boom".into())); // reopens, editable, fields kept
        let n = app.newt.as_ref().unwrap();
        assert!(!n.hidden && !n.planning && n.err.as_deref() == Some("boom") && (n.name.as_str(), n.task.as_str()) == ("n", "t"));
    }

    #[test]
    fn newt_draws_on_tiny_terminal() {
        let mut app = App { newt: Some(Newt { repo: "/r".into(), name: "n".into(), task: "日本語".repeat(40), err: Some("a\nb".into()), ..Default::default() }), ..Default::default() };
        for (w, h) in [(1, 1), (10, 3), (80, 4), (80, 24)] {
            let mut t = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
        }
    }

    #[test]
    fn drag_copies_selection() {
        let mut app = App::default();
        app.buf = Buffer::with_lines(["side|hello world  ", "side|second line  ", "side|third        "]);
        app.side = Rect::new(0, 0, 5, 3);
        app.pane().body = Rect::new(5, 0, 13, 3);
        let ev = |kind, x, y| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        let (down, drag, up) = (MouseEventKind::Down(MouseButton::Left), MouseEventKind::Drag(MouseButton::Left), MouseEventKind::Up(MouseButton::Left));
        app.on_mouse(ev(down, 11, 0));
        app.on_mouse(ev(drag, 10, 1));
        app.on_mouse(ev(up, 10, 1));
        assert_eq!(app.clip.take().as_deref(), Some("world\nsecond"));
        app.on_mouse(ev(down, 6, 2)); // backwards past the body's left edge: clamped
        app.on_mouse(ev(drag, 0, 2));
        app.on_mouse(ev(up, 0, 2));
        assert_eq!(app.clip.take().as_deref(), Some("th"));
        app.on_mouse(ev(down, 7, 1)); // a plain click copies nothing
        app.on_mouse(ev(up, 7, 1));
        assert!(app.clip.is_none());
        assert_eq!((base64(b"Man"), base64(b"Ma"), base64(b"M")), ("TWFu".into(), "TWE=".into(), "TQ==".into()));
    }

    #[test]
    fn divider_drag_resizes() {
        let mut app = App::default();
        (app.side, app.divider) = (Rect::new(0, 0, 40, 5), Rect::new(40, 0, 1, 5));
        app.pane().body = Rect::new(41, 0, 40, 5);
        let ev = |kind, x| MouseEvent { kind, column: x, row: 1, modifiers: KeyModifiers::NONE };
        let (down, drag, up) = (MouseEventKind::Down(MouseButton::Left), MouseEventKind::Drag(MouseButton::Left), MouseEventKind::Up(MouseButton::Left));
        app.on_mouse(ev(down, 40));
        app.on_mouse(ev(drag, 25));
        assert_eq!(app.side_w, Some(25));
        assert!(app.drag.is_none()); // no text selection
        app.on_mouse(ev(drag, 70)); // past SIDE_W: stops there
        assert_eq!(app.side_w, Some(SIDE_W));
        app.on_mouse(ev(drag, 2)); // and never below the minimum
        assert_eq!(app.side_w, Some(MIN_SIDE_W));
        app.on_mouse(ev(up, 2));
        app.on_mouse(ev(drag, 30)); // released: a stray drag moves nothing
        assert_eq!(app.side_w, Some(MIN_SIDE_W));
    }

    #[test]
    fn sidebar_drag_selects_nothing() {
        let mut app = App::default();
        app.buf = Buffer::with_lines(["side|hello world  ", "side|second line  "]);
        app.side = Rect::new(0, 0, 5, 2);
        app.pane().body = Rect::new(5, 0, 13, 2);
        let ev = |kind, x, y| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 2, 0));
        app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 10, 1));
        app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), 10, 1));
        assert!(app.drag.is_none() && app.clip.is_none());
    }
}
