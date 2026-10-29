//! Panes and tabs: what a tab shows, each thread's 1-2 panes, the tab window, and opening/closing/cycling/splitting tabs and sending keys to them.

use super::threads::{fresh_name, live_agents, registry_snapshot};
use super::App;
use crate::runner::{self, LiveAgent};
use crate::tree::Kind;
use ratatui::layout::{Constraint, Layout, Rect};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, fs};

/// What a tab shows: an agent's (or shell's) live terminal, or a read-only file/diff viewer.
pub(super) enum Tab {
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

/// One pane: a tab bar and its tabs, `active` the one shown.
#[derive(Default)]
pub(super) struct Pane {
    pub(super) list: Vec<Tab>,
    pub(super) active: usize,
    // From the last draw: the pane's tab bar, its body, and each shown tab's (x0, x1, index).
    pub(super) bar: Rect,
    pub(super) body: Rect,
    pub(super) spans: Vec<(u16, u16, usize)>,
}

/// A thread's panes, left to right (1 or 2, never 0), and the focused one.
pub(super) struct ThreadPanes {
    pub(super) list: Vec<Pane>,
    pub(super) focus: usize,
}

impl Default for ThreadPanes {
    fn default() -> Self {
        ThreadPanes {
            list: vec![Pane::default()],
            focus: 0,
        }
    }
}

impl ThreadPanes {
    /// (pane, tab) of the first tab matching `f`: an open tab is focused, never shown twice.
    fn find(&self, f: impl Fn(&Tab) -> bool) -> Option<(usize, usize)> {
        self.list
            .iter()
            .enumerate()
            .find_map(|(j, t)| Some((j, t.list.iter().position(&f)?)))
    }
}

/// `n` equal columns of `main` with a 1-column gap: each pane's (tab bar, body).
pub(super) fn pane_rects(main: Rect, n: usize) -> Vec<(Rect, Rect)> {
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

fn push_tab(tabs: &mut Pane, tab: Tab) {
    tabs.list.push(tab);
    while tabs.list.len() > MAX_TABS {
        tabs.list.remove(0);
    }
    tabs.active = tabs.list.len() - 1;
}

/// The tabs `first..end` shown in `avail` columns, a ‹ / › marker costing one column each when tabs
/// are hidden on that side. With `follow`, `first` moves just far enough that that tab is fully shown.
pub(super) fn tab_window(
    widths: &[u16],
    first: usize,
    follow: Option<usize>,
    avail: u16,
) -> (usize, usize) {
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

/// A stray tab shows "stray-1", not "stray agents/stray-1".
pub(super) fn tail(agent: &str) -> &str {
    agent.rsplit('/').next().unwrap_or(agent)
}

impl App {
    /// Opens registry agent (thread, agent) as a tab in brigd thread `key`'s focused pane
    /// (or focuses the pane that already has it).
    pub(super) fn open_agent(
        &mut self,
        key: String,
        thread: &str,
        agent: &str,
        stage: Option<usize>,
    ) {
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
    pub(super) fn new_term(&mut self) {
        let space = self.space.clone();
        let brigd_thread = Some(self.thread.clone()).filter(|t| !t.is_empty());
        let cwd = if space.as_os_str().is_empty() {
            env::current_dir().unwrap_or_default()
        } else {
            space.clone()
        };
        // Registry key (space, name): names are unique per space, and a closed-but-running tab keeps its name.
        let thread = space.to_string_lossy().into_owned();
        let reg = registry_snapshot();
        let taken = self.all_tabs().filter_map(|t| match t {
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
                    self.focused_pane(),
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
    pub(super) fn open_flowplan(&mut self, thread: &str) {
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

    pub(super) fn open_view(&mut self, thread: String, path: PathBuf) {
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
    pub(super) fn focused_pane(&mut self) -> &mut Pane {
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

    /// The focused pane's shown tab.
    pub(super) fn active_tab(&mut self) -> Option<&mut Tab> {
        let p = self.tabs.get_mut(&self.thread)?;
        let t = &mut p.list[p.focus];
        t.list.get_mut(t.active)
    }

    pub(super) fn cycle(&mut self, d: isize) {
        let t = self.focused_pane();
        if !t.list.is_empty() {
            t.active = (t.active as isize + d).rem_euclid(t.list.len() as isize) as usize;
            self.focus_main = true;
        }
    }

    /// Closes the current tab (its agent keeps running), and its pane once empty unless it is the only one.
    pub(super) fn close_tab(&mut self) {
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
    pub(super) fn split(&mut self) {
        let p = self.tabs.entry(self.thread.clone()).or_default();
        if p.list.len() >= 2 {
            return self.flash("max 2 panes");
        }
        p.list.push(Pane::default());
        p.focus = 1;
        self.new_term();
        if self.active_tab().is_none() {
            self.close_tab(); // the shell failed to start: drop the empty pane
        }
    }

    pub(super) fn other_pane(&mut self) {
        if let Some(p) = self.tabs.get_mut(&self.thread) {
            p.focus = (p.focus + 1) % p.list.len();
            self.focus_main = !p.list[p.focus].list.is_empty();
        }
    }

    /// Sizes every live agent not on screen to a one-pane body (draw sizes the shown ones), so panes don't fight over SIGWINCH.
    pub(super) fn resize_all(&self) {
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

    /// Every open tab of every thread.
    pub(super) fn all_tabs(&self) -> impl Iterator<Item = &Tab> {
        self.tabs
            .values()
            .flat_map(|p| &p.list)
            .flat_map(|t| &t.list)
    }

    pub(super) fn all_tabs_mut(&mut self) -> impl Iterator<Item = &mut Tab> {
        self.tabs
            .values_mut()
            .flat_map(|p| &mut p.list)
            .flat_map(|t| &mut t.list)
    }

    pub(super) fn send(&mut self, bytes: &[u8]) {
        let Some(Tab::Term { live, .. }) = self.active_tab() else {
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
}
