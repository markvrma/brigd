//! brigd's TUI: a sidebar of spaces (the tree.rs folder tree) and, for the
//! selected space, a tab bar of agent terminals (runner.rs PTYs drawn with
//! tui-term) and read-only output.md viewers. No panes.

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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};
use tui_term::widget::PseudoTerminal;

const SIDE_W: u16 = 36;
const REFRESH: Duration = Duration::from_millis(700);

/// (space, node path): a thread spanning spaces shows under each, folded separately.
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
}

enum Tab {
    Term { thread: String, agent: String, stage: Option<usize>, live: Arc<LiveAgent> },
    /// `rel`: a Diff node's file path (relative to the agent's cwd); `text` is then its diff.
    View { path: PathBuf, title: String, text: String, scroll: u16, rel: Option<String> },
}

/// The right-click popup on an agent row: one "Terminate" item filling `rect`.
struct Menu {
    rect: Rect,
    thread: String,
    agent: String,
}

const MENU_ITEM: &str = " Terminate ";

#[derive(Default)]
struct Tabs {
    list: Vec<Tab>,
    active: usize,
}

#[derive(Default)]
struct App {
    tree: Vec<Node>,
    rows: Vec<Row>,
    toggled: HashSet<NodeKey>,
    sel: usize,
    offset: usize,
    focus_main: bool,
    /// The space whose tabs show.
    space: PathBuf,
    tabs: BTreeMap<PathBuf, Tabs>,
    /// (thread, agent) -> running / blocked / done / failed / off
    agent_status: HashMap<(String, String), &'static str>,
    /// thread dir -> state.json status
    thread_status: HashMap<PathBuf, String>,
    blocked: HashSet<(String, String)>,
    prefix: bool,
    confirm_quit: bool,
    quit: bool,
    msg: Option<(String, Instant)>,
    ticks: u32,
    menu: Option<Menu>,
    /// Threads whose agents are paused (the button reads RESUME). In memory only.
    paused: HashSet<String>,
    /// Shell tab registry key -> the thread its $BRIGD_THREAD names (strays run inside these).
    shell_thread: HashMap<(String, String), String>,
    /// PAUSE/RESUME button rects from the last draw.
    pause_btns: Vec<(Rect, String)>,
    // Layout from the last draw, for mouse hit tests.
    screen: Rect,
    side: Rect,
    side_inner: Rect,
    tab_bar: Rect,
    tab_spans: Vec<(u16, u16)>,
    body: Rect,
    hint: Rect,
    /// Left-drag text selection: (rect it started in, anchor, cursor once dragged).
    drag: Option<(Rect, Position, Option<Position>)>,
    /// The last frame while dragging, to read the selected text from.
    buf: Buffer,
    /// Selected text waiting for the main loop to put on the clipboard.
    clip: Option<String>,
}

/// Opens the TUI over every thread in ~/.brigd/threads until Mark quits.
/// Quitting kills every live agent.
pub fn run(open: Option<&str>) -> Res<()> {
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
            if last.elapsed() >= REFRESH {
                app.refresh();
                last = Instant::now();
            }
        }
        live_agents().iter().for_each(|a| a.kill());
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

fn resize_all(body: Rect) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    for a in live_agents() {
        let size = a.parser.lock().unwrap().screen().size();
        if size != (body.height, body.width) {
            let _ = a.resize(body.height, body.width);
        }
    }
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

/// Catppuccin Mocha (catppuccin.com/palette), the TUI's colors.
mod pal {
    use ratatui::style::Color::{self, Rgb};
    pub const MAUVE: Color = Rgb(0xcb, 0xa6, 0xf7);
    pub const PINK: Color = Rgb(0xf5, 0xc2, 0xe7);
    pub const RED: Color = Rgb(0xf3, 0x8b, 0xa8);
    pub const PEACH: Color = Rgb(0xfa, 0xb3, 0x87);
    pub const YELLOW: Color = Rgb(0xf9, 0xe2, 0xaf);
    pub const GREEN: Color = Rgb(0xa6, 0xe3, 0xa1);
    pub const TEAL: Color = Rgb(0x94, 0xe2, 0xd5);
    pub const SKY: Color = Rgb(0x89, 0xdc, 0xeb);
    pub const BLUE: Color = Rgb(0x89, 0xb4, 0xfa);
    pub const LAVENDER: Color = Rgb(0xb4, 0xbe, 0xfe);
    pub const TEXT: Color = Rgb(0xcd, 0xd6, 0xf4);
    pub const SUBTEXT0: Color = Rgb(0xa6, 0xad, 0xc8);
    pub const OVERLAY0: Color = Rgb(0x6c, 0x70, 0x86);
    pub const SURFACE1: Color = Rgb(0x45, 0x47, 0x5a);
    pub const SURFACE0: Color = Rgb(0x31, 0x32, 0x44);
    pub const MANTLE: Color = Rgb(0x18, 0x18, 0x25);
    pub const CRUST: Color = Rgb(0x11, 0x11, 0x1b);
}

/// Agent rows/tabs are tinted by stage; avoids the status-glyph and selection colors.
const STAGE_COLORS: [Color; 6] = [pal::MAUVE, pal::YELLOW, pal::BLUE, pal::PEACH, pal::PINK, pal::TEAL];

fn stage_color(stage: Option<usize>) -> Color {
    stage.map_or(Color::Reset, |s| STAGE_COLORS[s % STAGE_COLORS.len()])
}

fn flatten(n: &Node, depth: usize, space: &Path, thread: &str, toggled: &HashSet<NodeKey>, out: &mut Vec<Row>) {
    let folder = !n.children.is_empty();
    let default_open = matches!(n.kind, Kind::Space | Kind::Thread | Kind::Folder);
    let open = folder && default_open != toggled.contains(&(space.into(), n.path.clone()));
    let thread = if n.kind == Kind::Thread { n.label.as_str() } else { thread };
    out.push(Row { depth, kind: n.kind.clone(), label: n.label.clone(), path: n.path.clone(), space: space.into(), thread: thread.into(), folder, open, stage: n.stage });
    if open {
        n.children.iter().for_each(|c| flatten(c, depth + 1, space, thread, toggled, out));
    }
}

/// The sidebar row under a click, and whether the click hit its fold arrow.
fn hit(inner: Rect, offset: usize, rows: &[Row], col: u16, row: u16) -> Option<(usize, bool)> {
    if !inner.contains(Position { x: col, y: row }) {
        return None;
    }
    let i = offset + (row - inner.y) as usize;
    let r = rows.get(i)?;
    let arrow_x = (r.depth * 2) as u16;
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
        for tab in self.tabs.values_mut().flat_map(|t| &mut t.list) {
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
        resize_all(self.body);
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
    }

    fn move_sel(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.sel = (self.sel as isize + d).clamp(0, self.rows.len() as isize - 1) as usize;
        self.space = self.rows[self.sel].space.clone();
        let h = (self.side_inner.height as usize).max(1);
        self.offset = self.offset.clamp(self.sel.saturating_sub(h - 1), self.sel);
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
        let Some(r) = self.rows.get(i) else { return };
        self.space = r.space.clone();
        match r.kind {
            Kind::Output | Kind::Diff if !arrow => {
                let (path, space) = (r.path.clone(), r.space.clone());
                self.open_view(space, path);
            }
            Kind::Agent if !arrow => {
                let (space, thread, agent, stage) = (r.space.clone(), r.thread.clone(), tree::agent_id(&r.path, &r.label), r.stage);
                if r.folder && !r.open {
                    self.toggle(i);
                }
                self.open_agent(space, &thread, &agent, stage);
            }
            _ if r.folder => self.toggle(i),
            _ => {}
        }
    }

    fn open_agent(&mut self, space: PathBuf, thread: &str, agent: &str, stage: Option<usize>) {
        let tabs = self.tabs.entry(space.clone()).or_default();
        let found = tabs.list.iter().position(|t| matches!(t, Tab::Term { thread: t, agent: a, .. } if t == thread && a == agent));
        if let Some(i) = found.filter(|&i| matches!(&tabs.list[i], Tab::Term { live, .. } if live.alive())) {
            tabs.active = i;
            self.focus_main = true;
            return;
        }
        // Not live (brigd restarted, or it exited): reopen its session.
        match runner::resume_agent(thread, agent) {
            Ok(live) => {
                let _ = live.resize(self.body.height.max(1), self.body.width.max(1));
                let tab = Tab::Term { thread: thread.into(), agent: agent.into(), stage, live };
                let tabs = self.tabs.entry(space).or_default();
                match found {
                    Some(i) => tabs.list[i] = tab,
                    None => tabs.list.push(tab),
                }
                tabs.active = found.unwrap_or(tabs.list.len() - 1);
                self.focus_main = true;
            }
            Err(e) => self.flash(format!("{thread}/{agent}: {e}")),
        }
    }

    /// Opens the login shell as a new tab in the current space. `claude` typed in it
    /// becomes a stray agent of the selected (else the space's first) thread: the
    /// shell gets $BRIGD_THREAD and the ~/.brigd/bin shim first on PATH.
    fn new_term(&mut self) {
        let space = self.space.clone();
        let sel = self.rows.get(self.sel).filter(|r| r.space == space && !r.thread.is_empty()).map(|r| r.thread.clone());
        let first = || self.tree.iter().find(|s| s.path == space)?.children.first().map(|t| t.label.clone());
        let brigd_thread = sel.or_else(first);
        let cwd = if space.as_os_str().is_empty() { env::current_dir().unwrap_or_default() } else { space.clone() };
        // Registry key (space, name): names are unique per space, and a closed-but-running tab keeps its name.
        let thread = space.to_string_lossy().into_owned();
        let reg = runner::registry().lock().unwrap().clone();
        let tabs = self.tabs.entry(space).or_default();
        let taken = tabs.list.iter().filter_map(|t| match t {
            Tab::Term { agent, .. } => Some(agent.as_str()),
            _ => None,
        });
        let name = fresh_name(taken.chain(reg.iter().filter(|(k, a)| k.0 == thread && a.alive()).map(|(k, _)| k.1.as_str())));
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
                let _ = live.resize(self.body.height.max(1), self.body.width.max(1));
                if let Some(t) = brigd_thread {
                    self.shell_thread.insert((thread.clone(), name.clone()), t);
                }
                tabs.list.push(Tab::Term { thread, agent: name, stage: None, live });
                tabs.active = tabs.list.len() - 1;
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

    fn open_view(&mut self, space: PathBuf, path: PathBuf) {
        let tabs = self.tabs.entry(space).or_default();
        if let Some(i) = tabs.list.iter().position(|t| matches!(t, Tab::View { path: p, .. } if *p == path)) {
            tabs.active = i;
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
            tabs.list.push(tab);
            tabs.active = tabs.list.len() - 1;
        }
        self.focus_main = true;
    }

    fn cur(&mut self) -> Option<&mut Tab> {
        let t = self.tabs.get_mut(&self.space)?;
        t.list.get_mut(t.active)
    }

    fn cycle(&mut self, d: isize) {
        if let Some(t) = self.tabs.get_mut(&self.space).filter(|t| !t.list.is_empty()) {
            t.active = (t.active as isize + d).rem_euclid(t.list.len() as isize) as usize;
            self.focus_main = true;
        }
    }

    /// Closes the current tab; its agent keeps running.
    fn close_tab(&mut self) {
        if let Some(t) = self.tabs.get_mut(&self.space).filter(|t| !t.list.is_empty()) {
            t.list.remove(t.active);
            t.active = t.active.min(t.list.len().saturating_sub(1));
            self.focus_main = !t.list.is_empty();
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
            let reg = runner::registry().lock().unwrap().clone();
            let shells = reg.iter().filter(|(k, a)| a.alive() && self.shell_thread.get(*k).is_some_and(|t| t == thread));
            targets.extend(shells.map(|(_, a)| a.clone()));
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

    fn terminate(&mut self, thread: &str, agent: &str) {
        match crate::terminate(thread, agent) {
            Ok(n) => self.flash(format!("terminated {agent}: reverted {n} file(s), output kept")),
            Err(e) => self.flash(format!("terminate {agent}: {e}")),
        }
        self.refresh();
    }

    fn on_key(&mut self, k: KeyEvent) {
        if let Some(m) = self.menu.take() {
            // Enter picks the item; Esc or any other key just closes the menu.
            if k.code == KeyCode::Enter {
                self.terminate(&m.thread, &m.agent);
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
                KeyCode::Char('s') => self.focus_main = false,
                KeyCode::Char('n') => self.cycle(1),
                KeyCode::Char('p') => self.cycle(-1),
                KeyCode::Char('w') => self.close_tab(),
                KeyCode::Char('t') => self.new_term(),
                KeyCode::Char('q') => self.ask_quit(),
                _ if ctrl_o => self.send(&[0x0f]), // Ctrl-o twice sends one to claude
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
            KeyCode::Tab => self.focus_main = self.tabs.get(&self.space).is_some_and(|t| !t.list.is_empty()),
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position { x: m.column, y: m.row };
        if matches!(m.kind, MouseEventKind::Down(_)) {
            if let Some(menu) = self.menu.take() {
                // A left click on the item picks it; any other click closes the menu (a right
                // click on another agent row then opens that row's menu below).
                if m.kind == MouseEventKind::Down(MouseButton::Left) {
                    if menu.rect.contains(at) {
                        self.terminate(&menu.thread, &menu.agent);
                    }
                    return;
                }
            }
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                let agent = hit(self.side_inner, self.offset, &self.rows, m.column, m.row).map(|(i, _)| i).filter(|&i| self.rows[i].kind == Kind::Agent);
                if let Some(i) = agent {
                    self.sel = i;
                    let r = &self.rows[i];
                    let (thread, agent) = (r.thread.clone(), tree::agent_id(&r.path, &r.label));
                    let w = MENU_ITEM.len() as u16;
                    let x = if self.screen.width == 0 { m.column } else { m.column.min(self.screen.right().saturating_sub(w)) };
                    self.menu = Some(Menu { rect: Rect::new(x, m.row, w, 1), thread, agent });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // The sidebar is not among these, so a drag starting there selects nothing.
                self.drag = [self.tab_bar, self.body, self.hint].into_iter().find(|r| r.contains(at)).map(|r| (r, at, None));
                if let Some(t) = self.pause_btns.iter().find(|(r, _)| r.contains(at)).map(|(_, t)| t.clone()) {
                    self.toggle_pause(&t);
                } else if let Some((i, arrow)) = hit(self.side_inner, self.offset, &self.rows, m.column, m.row) {
                    self.sel = i;
                    self.activate(i, arrow);
                    self.focus_main = false; // a click shows the item but keeps j/k on the sidebar
                } else if self.tab_bar.contains(at) {
                    if let Some(i) = self.tab_spans.iter().position(|&(a, b)| (a..b).contains(&m.column)) {
                        self.tabs.entry(self.space.clone()).or_default().active = i;
                        self.focus_main = true;
                    }
                } else if self.body.contains(at) && self.cur().is_some() {
                    self.focus_main = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(d) = &mut self.drag {
                    d.2 = Some(at);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((r, a, Some(b))) = self.drag.take() {
                    let t = sel_text(&self.buf, r, a, b);
                    if !t.trim().is_empty() {
                        self.flash(format!("copied {} chars", t.chars().count()));
                        self.clip = Some(t);
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d: i32 = if m.kind == MouseEventKind::ScrollUp { -3 } else { 3 };
                if self.side.contains(at) {
                    let max = self.rows.len().saturating_sub(self.side_inner.height as usize) as i32;
                    self.offset = (self.offset as i32 + d).clamp(0, max.max(0)) as usize;
                } else if self.body.contains(at) {
                    match self.cur() {
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
        if !self.focus_main {
            return;
        }
        let Some(Tab::Term { live, .. }) = self.cur() else { return };
        let bracketed = live.parser.lock().unwrap().screen().bracketed_paste();
        let bytes = if bracketed { format!("\x1b[200~{s}\x1b[201~") } else { s.replace('\n', "\r") };
        self.send(bytes.as_bytes());
    }

    fn row_line(&self, r: &Row, selected: bool) -> Line<'static> {
        let arrow = match (r.folder, r.open) {
            (false, _) => "  ",
            (true, true) => "▾ ",
            (true, false) => "▸ ",
        };
        let mut spans = vec![Span::raw(format!("{}{arrow}", "  ".repeat(r.depth)))];
        let mut label = Style::new();
        match r.kind {
            Kind::Agent => {
                let st = self.agent_status.get(&(r.thread.clone(), tree::agent_id(&r.path, &r.label))).copied().unwrap_or("off");
                let (g, style) = match st {
                    "running" => ("● ", Style::new().fg(pal::GREEN)),
                    "idle" => ("◦ ", Style::new().fg(pal::GREEN)),
                    "blocked" => ("! ", Style::new().fg(pal::RED).add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)),
                    "done" => ("✓ ", Style::new().fg(pal::BLUE)),
                    "failed" => ("✗ ", Style::new().fg(pal::RED)),
                    _ => ("○ ", Style::new().fg(pal::OVERLAY0)),
                };
                label = if st == "blocked" { style } else { label.fg(stage_color(r.stage)) };
                spans.push(Span::styled(g, style));
                if let Some(s) = r.stage {
                    spans.push(Span::styled(format!("{}·", s + 1), Style::new().fg(stage_color(r.stage))));
                }
            }
            Kind::Output => label = label.fg(pal::SKY),
            Kind::Diff => label = label.fg(pal::YELLOW),
            Kind::Space => label = label.fg(pal::MAUVE).add_modifier(Modifier::BOLD),
            Kind::Thread => label = label.fg(pal::TEXT).add_modifier(Modifier::BOLD),
            _ => label = label.fg(pal::SUBTEXT0),
        }
        let slash = if matches!(r.kind, Kind::Output | Kind::Diff) { "" } else { "/" };
        spans.push(Span::styled(format!("{}{slash}", r.label), label));
        if r.kind == Kind::Thread {
            let st = self.thread_status.get(&r.path).map(String::as_str).unwrap_or("");
            spans.push(Span::styled(format!(" {st}"), Style::new().fg(pal::OVERLAY0)));
        }
        let line = Line::from(spans);
        match (selected, self.focus_main) {
            (true, false) => line.style(Style::new().add_modifier(Modifier::REVERSED)),
            (true, true) => line.style(Style::new().bg(pal::SURFACE1)),
            // The space (repo or worktree) whose tabs show.
            _ if r.kind == Kind::Space && r.space == self.space => line.style(Style::new().bg(pal::SURFACE0)),
            _ => line,
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [top, hint] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [side, main] = Layout::horizontal([Constraint::Length(SIDE_W), Constraint::Min(0)]).areas(top);
        let [bar, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(main);
        let border = if self.focus_main { pal::SURFACE1 } else { pal::LAVENDER };
        let block = Block::bordered().title(" spaces ").border_style(Style::new().fg(border));
        let inner = block.inner(side);
        if body != self.body {
            resize_all(body);
        }
        (self.screen, self.side, self.side_inner, self.tab_bar, self.body, self.hint) = (f.area(), side, inner, bar, body, hint);
        f.render_widget(block, side);

        let h = inner.height as usize;
        self.offset = self.offset.min(self.rows.len().saturating_sub(h));
        let lines: Vec<Line> = self.rows.iter().enumerate().skip(self.offset).take(h).map(|(i, r)| self.row_line(r, i == self.sel)).collect();
        if lines.is_empty() {
            f.render_widget(Paragraph::new("no threads yet\n\nbrigd <name> \"task\""), inner);
        } else {
            f.render_widget(Paragraph::new(lines), inner);
        }

        // PAUSE/RESUME at the right end of each visible thread row.
        self.pause_btns.clear();
        for (n, r) in self.rows.iter().enumerate().skip(self.offset).take(h).filter(|(_, r)| r.kind == Kind::Thread) {
            let label = if self.paused.contains(&r.label) { " RESUME " } else { " PAUSE " };
            let w = label.len() as u16;
            let rect = Rect::new(inner.right().saturating_sub(w).max(inner.x), inner.y + (n - self.offset) as u16, w.min(inner.width), 1);
            f.render_widget(Paragraph::new(label).style(Style::new().fg(pal::CRUST).bg(pal::RED).add_modifier(Modifier::BOLD)), rect);
            self.pause_btns.push((rect, r.label.clone()));
        }

        // Tab bar: space name, then one tab per opened agent/viewer in this space.
        let space = self.space.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut spans = vec![Span::styled(format!(" {space} "), Style::new().fg(pal::CRUST).bg(pal::MAUVE))];
        let mut x = bar.x + space.chars().count() as u16 + 2;
        self.tab_spans.clear();
        let tabs = self.tabs.get(&self.space);
        for (i, t) in tabs.map(|t| t.list.as_slice()).unwrap_or_default().iter().enumerate() {
            let (title, fg) = match t {
                Tab::Term { agent, live, stage, .. } if !live.alive() => (format!(" {} (exited) ", tail(agent)), Some(stage_color(*stage))),
                Tab::Term { agent, stage, .. } => (format!(" {} ", tail(agent)), Some(stage_color(*stage))),
                Tab::View { title, .. } => (format!(" {title} "), None),
            };
            let w = title.chars().count() as u16;
            self.tab_spans.push((x + 1, x + 1 + w));
            x += 1 + w;
            let active = tabs.is_some_and(|t| t.active == i);
            spans.push(Span::raw("│"));
            let style = fg.map_or(Style::new(), |c| Style::new().fg(c));
            spans.push(Span::styled(title, if active { style.add_modifier(Modifier::REVERSED | Modifier::BOLD) } else { style }));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), bar);

        let focus_main = self.focus_main;
        match self.cur() {
            Some(Tab::Term { live, .. }) => {
                let live = live.clone();
                if live.parser.lock().unwrap().screen().size() != (body.height, body.width) {
                    let _ = live.resize(body.height, body.width);
                }
                let p = live.parser.lock().unwrap();
                let mut pt = PseudoTerminal::new(p.screen());
                if !focus_main || p.screen().scrollback() > 0 {
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
                f.render_widget(Paragraph::new(rel.as_str()).style(Style::new().fg(pal::OVERLAY0)).right_aligned(), foot);
            }
            Some(Tab::View { path, text, scroll, .. }) => {
                let p = if path.extension().is_some_and(|e| e == "md") { Paragraph::new(md_lines(text)) } else { Paragraph::new(text.as_str()) };
                f.render_widget(p.wrap(Wrap { trim: false }).scroll((*scroll, 0)), body);
            }
            None => f.render_widget(Paragraph::new("\n  Enter or click an agent to open its claude tab, or an output.md to read it."), body),
        }

        let viewing = matches!(self.cur(), Some(Tab::View { .. }));
        let text = match &self.msg {
            Some((m, t)) if t.elapsed() < Duration::from_secs(5) => Span::styled(m.clone(), Style::new().fg(pal::YELLOW)),
            _ if self.confirm_quit => Span::styled(
                format!("{} agents running; quitting kills them. Quit? y/N", live_agents().len()),
                Style::new().fg(pal::RED).add_modifier(Modifier::BOLD),
            ),
            _ if self.prefix => Span::raw("C-o …  s sidebar · n/p next/prev tab · t terminal · w close tab · q quit · C-o send C-o"),
            _ if !self.focus_main => Span::raw("↑↓/jk move · Enter open/fold · ←→ fold · Tab back to tab · M-h/l tabs · M-t terminal · q quit · wheel scrolls"),
            _ if viewing => Span::raw("↑↓/jk PgUp/PgDn g/G scroll · Tab/Esc sidebar · M-h/l tabs · M-t terminal · C-o w close · C-o q quit"),
            _ => Span::raw("keys go to the tab · C-o s sidebar · M-h/l tabs · M-t terminal · C-o w close (keeps running) · C-o q quit · wheel scrollback"),
        };
        f.render_widget(Paragraph::new(Line::from(text).style(Style::new().fg(pal::SUBTEXT0).bg(pal::MANTLE))), hint);

        if let Some(m) = &self.menu {
            let r = m.rect.intersection(f.area());
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(MENU_ITEM).style(Style::new().fg(pal::CRUST).bg(pal::RED).add_modifier(Modifier::BOLD)), r);
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
            Some(b'+') => Line::styled(l, Style::new().fg(pal::GREEN)),
            Some(b'-') => Line::styled(l, Style::new().fg(pal::RED)),
            Some(b'@') if l.starts_with("@@") => Line::styled(l, Style::new().fg(pal::SKY).add_modifier(Modifier::DIM)),
            _ => Line::raw(l),
        })
        .collect()
}

/// Markdown → styled lines, one per source line so scroll bounds match `text.lines()`.
fn md_lines(text: &str) -> Vec<Line<'static>> {
    let (mut code, dim) = (false, Style::new().fg(pal::OVERLAY0));
    text.lines()
        .map(|l| {
            let t = l.trim_start();
            let ind = &l[..l.len() - t.len()];
            if t.starts_with("```") {
                code = !code;
                return Line::styled("─".repeat(40), dim);
            }
            if code {
                return Line::styled(l.to_string(), Style::new().fg(pal::PEACH).bg(pal::SURFACE0));
            }
            if let Some(n) = (1..=6).find(|&n| t.starts_with(&format!("{} ", "#".repeat(n)))) {
                let c = [pal::MAUVE, pal::SKY, pal::BLUE][(n - 1).min(2)];
                return Line::from(inline(&t[n + 1..], Style::new().fg(c).add_modifier(Modifier::BOLD)));
            }
            if t.len() >= 3 && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_')) {
                return Line::styled("─".repeat(40), dim);
            }
            if let Some(q) = t.strip_prefix('>') {
                let mut v = vec![Span::raw(ind.to_string()), Span::styled("│ ", dim)];
                v.extend(inline(q.trim_start(), Style::new().fg(pal::SUBTEXT0).add_modifier(Modifier::ITALIC)));
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
            Line::from([vec![Span::raw(ind.to_string()), Span::styled(mark + " ", Style::new().fg(pal::YELLOW))], inline(rest, Style::new())].concat())
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
                out.push(Span::styled(r[..e].to_string(), Style::new().fg(pal::PEACH).bg(pal::SURFACE0)));
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
                out.extend(inline(&r[..e], base.fg(pal::SKY).add_modifier(Modifier::UNDERLINED)));
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
    fn diff_colors() {
        let l = diff_lines("@@ -1,2 +1,2 @@\n a\n-b\n+c");
        let fg: Vec<_> = l.iter().map(|l| l.style.fg).collect();
        assert_eq!(fg, [Some(pal::SKY), None, Some(pal::RED), Some(pal::GREEN)]);
    }

    #[test]
    fn click_hits_tree_rows() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None };
        let agent = node(Kind::Agent, "a", vec![node(Kind::Output, "output.md", vec![])]);
        let tree = node(Kind::Space, "repo", vec![node(Kind::Thread, "t", vec![agent, node(Kind::Agent, "b", vec![])])]);
        let mut rows = Vec::new();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut rows);
        // Spaces and threads start open, agents folded.
        let labels: Vec<_> = rows.iter().map(|r| (r.depth, r.label.as_str(), r.thread.as_str())).collect();
        assert_eq!(labels, [(0, "repo", ""), (1, "t", "t"), (2, "a", "t"), (2, "b", "t")]);
        let inner = Rect::new(1, 1, 34, 3); // rows 1..=3 visible
        assert_eq!(hit(inner, 0, &rows, 5, 1), Some((0, false)));
        assert_eq!(hit(inner, 1, &rows, 10, 2), Some((2, false))); // scrolled by one
        assert_eq!(hit(inner, 0, &rows, 1 + 4, 3), Some((2, true))); // depth 2 arrow at x 4..6
        assert_eq!(hit(inner, 0, &rows, 1 + 6, 3), Some((2, false)));
        assert_eq!(hit(inner, 1, &rows, 1 + 4, 3), Some((3, false))); // "b" has no children: no arrow
        assert_eq!(hit(inner, 2, &rows, 5, 3), None); // past the last row
        assert_eq!(hit(inner, 0, &rows, 5, 4), None); // below the list
        assert_eq!(hit(inner, 0, &rows, 0, 1), None); // on the border
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
        app.tabs.insert(PathBuf::new(), Tabs { list: vec![view(), view()], active: 0 });
        app.on_key(key(KeyCode::Char('l'), KeyModifiers::ALT)); // from the sidebar
        assert_eq!((app.tabs[&PathBuf::new()].active, app.focus_main), (1, true));
        app.on_key(key(KeyCode::Char('˙'), KeyModifiers::NONE));
        assert_eq!(app.tabs[&PathBuf::new()].active, 0);
    }

    #[test]
    fn fresh_names() {
        assert_eq!(fresh_name([].into_iter()), "term-1");
        assert_eq!(fresh_name(["term-1", "a", "term-3"].into_iter()), "term-2");
    }

    #[test]
    fn right_click_opens_terminate_menu() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None };
        let tree = node(Kind::Space, "repo", vec![node(Kind::Thread, "t", vec![node(Kind::Agent, "a", vec![])])]);
        let mut app = App::default();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
        (app.side_inner, app.screen) = (Rect::new(1, 1, 98, 10), Rect::new(0, 0, 100, 20));
        let click = |b, x, y| MouseEvent { kind: MouseEventKind::Down(b), column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(click(MouseButton::Right, 5, 1)); // the space row: no menu
        assert!(app.menu.is_none());
        app.on_mouse(click(MouseButton::Right, 5, 3)); // the agent row
        let m = app.menu.as_ref().unwrap();
        assert_eq!((m.thread.as_str(), m.agent.as_str(), m.rect.y), ("t", "a", 3));
        app.on_mouse(click(MouseButton::Left, 50, 8)); // elsewhere: closes, opens nothing
        assert!(app.menu.is_none() && app.tabs.is_empty());
        app.on_mouse(click(MouseButton::Right, 5, 3));
        app.on_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.menu.is_none());
        app.on_mouse(click(MouseButton::Right, 97, 3)); // at the screen edge: the menu is shifted left
        assert_eq!(app.menu.as_ref().unwrap().rect.right(), 100);
    }

    #[test]
    fn sidebar_click_keeps_focus() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None };
        let tree = node(Kind::Space, "repo", vec![node(Kind::Output, "nope.md", vec![])]);
        let mut app = App::default();
        flatten(&tree, 0, &tree.path, "", &HashSet::new(), &mut app.rows);
        (app.side_inner, app.body) = (Rect::new(1, 1, 34, 10), Rect::new(40, 2, 40, 10));
        let click = |x, y| MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(click(5, 2)); // the output row: opens a viewer tab, focus stays on the sidebar
        assert_eq!((app.tabs[&tree.path].list.len(), app.focus_main), (1, false));
        app.on_key(key(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.sel, 0); // j/k moved the sidebar selection
        app.on_mouse(click(50, 5)); // the body
        assert!(app.focus_main);
    }

    #[test]
    fn pause_button_toggles() {
        let node = |kind, label: &str, children| Node { kind, label: label.into(), path: PathBuf::from(format!("/{label}")), children, stage: None };
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
    fn drag_copies_selection() {
        let mut app = App::default();
        app.buf = Buffer::with_lines(["side|hello world  ", "side|second line  ", "side|third        "]);
        (app.side, app.body) = (Rect::new(0, 0, 5, 3), Rect::new(5, 0, 13, 3));
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
    fn sidebar_drag_selects_nothing() {
        let mut app = App::default();
        app.buf = Buffer::with_lines(["side|hello world  ", "side|second line  "]);
        (app.side, app.body) = (Rect::new(0, 0, 5, 2), Rect::new(5, 0, 13, 2));
        let ev = |kind, x, y| MouseEvent { kind, column: x, row: y, modifiers: KeyModifiers::NONE };
        app.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), 2, 0));
        app.on_mouse(ev(MouseEventKind::Drag(MouseButton::Left), 10, 1));
        app.on_mouse(ev(MouseEventKind::Up(MouseButton::Left), 10, 1));
        assert!(app.drag.is_none() && app.clip.is_none());
    }
}
