//! brigd's TUI: a sidebar of spaces (the tree.rs folder tree) and, for the
//! selected thread, one or two side-by-side panes, each a tab bar of agent terminals
//! (runner.rs PTYs drawn with tui-term) and read-only output.md viewers.

use crate::tree::{self, Kind, Node};
use crate::{runner, Res};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::Terminal;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod draw;
mod flowplan;
mod input;
mod markdown;
mod newt;
mod sidebar;
mod tabs;
mod term;
mod theme;
mod threads;
pub use flowplan::plan_lines;
use newt::{Bg, Newt};
use sidebar::{NodeKey, Row};
use tabs::{Tab, ThreadPanes};
use term::{copy_to_clipboard, setup, teardown};
use theme::{init_theme, pal};
use threads::{add_terminals, agent_glyph_status, live_agents, stray_status, Menu};

const SIDE_W: u16 = 40;
const MIN_SIDE_W: u16 = 12;
const REFRESH: Duration = Duration::from_millis(700);
/// Run-log lines the FLOWPLAN view shows under the plan.
const LOG_TAIL: usize = 15;

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
    tabs: BTreeMap<String, ThreadPanes>,
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

    fn ask_quit(&mut self) {
        if live_agents().is_empty() {
            self.quit = true;
        } else {
            self.confirm_quit = true;
        }
    }
}

#[cfg(test)]
mod tests;
