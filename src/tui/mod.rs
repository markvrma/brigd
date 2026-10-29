//! brigd's TUI: a sidebar of spaces (the tree.rs folder tree) and, for the
//! selected thread, one or two side-by-side panes, each a tab bar of agent terminals
//! (runner.rs PTYs drawn with tui-term) and read-only output.md viewers.

use crate::tree::{self, Kind, Node};
use crate::{runner, Res};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tui_term::widget::PseudoTerminal;

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
use flowplan::{flow_lines, SPIN};
use markdown::{diff_lines, md_text};
use newt::{chunk, Bg, Newt, NEWT_W};
use sidebar::{layout, max_offset, NodeKey, Row};
use tabs::{pane_rects, tab_window, tail, Panes, Tab};
use term::{copy_to_clipboard, sel_rows, setup, teardown};
use theme::{init_theme, pal, stage_color};
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

#[cfg(test)]
mod tests;
