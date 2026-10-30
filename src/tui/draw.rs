//! Drawing a frame: sidebar, buttons, panes and their tab bars, hint line, menu, modal and the drag selection.

use super::flowplan::{flow_lines, SPIN};
use super::markdown::{diff_lines, md_text};
use super::newt::{chunk, NEWT_W};
use super::sidebar::{layout, max_offset};
use super::tabs::{pane_rects, tab_window, tail, Pane, Tab, CLOSE_W, FADE};
use super::term::sel_rows;
use super::theme::{pal, stage_color};
use super::threads::live_agents;
use super::{App, SIDE_W};
use crate::runner::LiveAgent;
use crate::tree::Kind;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

/// How long a flash message stays on the hint line.
const FLASH_FOR: Duration = Duration::from_secs(5);

impl App {
    /// Draws one frame. Later layers paint over earlier ones: sidebar, its buttons, panes, hint,
    /// menu, modal, and the drag selection last, so the text it copies includes the menu and modal.
    pub(super) fn draw(&mut self, f: &mut Frame) {
        let [top, hint] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [side, divider, main] = Layout::horizontal([
            Constraint::Length(self.side_w.unwrap_or(SIDE_W)),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(top);
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

        let shown = self.draw_sidebar(f, side, divider, inner);
        self.draw_side_buttons(f, side, inner, &shown);
        self.draw_panes(f, main);
        let text = self.hint_text();
        f.render_widget(
            Paragraph::new(
                Line::from(text).style(Style::new().fg(pal().subtext0).bg(pal().mantle)),
            ),
            hint,
        );
        self.draw_menu(f);
        self.draw_modal(f);
        self.draw_selection(f);
    }

    /// The "spaces" title, the divider and the tree rows; returns the shown rows as (line, row index).
    fn draw_sidebar(
        &mut self,
        f: &mut Frame,
        side: Rect,
        divider: Rect,
        inner: Rect,
    ) -> Vec<(usize, usize)> {
        let border = if self.focus_main {
            pal().overlay0
        } else {
            pal().lavender
        };
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
            let (sel, w) = (i == self.sel, inner.width as usize);
            if self.rows[i].kind == Kind::Master {
                lines.extend(self.master_banner(sel, w));
            } else {
                lines.push(self.row_line(&self.rows[i], sel, w));
            }
        }
        if lines.is_empty() {
            f.render_widget(
                Paragraph::new("no threads yet\n\nbrigd <name> \"task\""),
                inner,
            );
        } else {
            f.render_widget(Paragraph::new(lines), inner);
        }
        shown
    }

    /// At the right end of each visible space row a '+' (new thread), of each thread row
    /// its pause dot: ○ running, ● paused (click resumes). Then the header '＋'.
    fn draw_side_buttons(
        &mut self,
        f: &mut Frame,
        side: Rect,
        inner: Rect,
        shown: &[(usize, usize)],
    ) {
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
    }

    /// The thread's panes side by side, a dim "│" between them.
    fn draw_panes(&mut self, f: &mut Frame, main: Rect) {
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
        // draw_pane refills task_btns.
        self.task_btns.clear();
        for (j, (bar, body)) in rects.into_iter().enumerate() {
            self.draw_pane(f, j, bar, body);
        }
    }

    /// Pane `j` of the shown thread: its tab bar (the thread name first on pane 0), then the active tab.
    fn draw_pane(&mut self, f: &mut Frame, j: usize, bar: Rect, body: Rect) {
        let p = &self.tabs[&self.thread];
        let (tabs, focused) = (&p.list[j], p.focus == j);
        let pill = (j == 0).then_some(self.thread.as_str());
        let (spans, tab_spans) = tab_bar(tabs, focused, pill, bar);
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
        // From here `t` borrows self.tabs mutably; the arms get every other field passed in.
        let t = &mut self.tabs.get_mut(&self.thread).unwrap().list[j];
        (t.bar, t.body, t.spans) = (bar, body, tab_spans);
        match t.list.get_mut(t.active) {
            Some(Tab::Term { live, .. }) => render_term(f, live, body, cursor),
            Some(Tab::View {
                text,
                scroll,
                rel: Some(rel),
                ..
            }) => render_diff(f, text, scroll, rel, body),
            Some(Tab::View { scroll, .. }) if plan.is_some() => {
                let lines = plan.take().unwrap_or_default();
                let path = plan_path.unwrap_or_default();
                render_plan(f, lines, scroll, pad, hits, &path, &mut self.task_btns);
            }
            Some(Tab::View {
                path, text, scroll, ..
            }) => render_text(f, path, text, scroll, pad, body),
            None => f.render_widget(
                Paragraph::new(
                    "\n  Enter or click an agent to open its claude tab, or an output.md to read it.",
                ),
                body,
            ),
        }
    }

    /// The hint line: a fresh flash message, a pending confirm, the armed prefix's keys, else
    /// the keys for where focus is.
    fn hint_text(&mut self) -> Span<'static> {
        let viewing = matches!(self.active_tab(), Some(Tab::View { .. }));
        match &self.msg {
            Some((m, t)) if t.elapsed() < FLASH_FOR => Span::styled(m.clone(), Style::new().fg(pal().yellow)),
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
        }
    }

    /// The right-click menu, the selected item filled.
    fn draw_menu(&self, f: &mut Frame) {
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
    }

    /// The modal's (title, lines, width) on screen `a`: the new-thread window, or the run
    /// confirm once it is planned.
    fn modal_content(&self, a: Rect) -> Option<(String, Vec<Line<'static>>, u16)> {
        if let Some(n) = self.newt.as_ref().filter(|n| !n.hidden) {
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
        }
    }

    /// The modal, if any, centered on top.
    fn draw_modal(&self, f: &mut Frame) {
        let a = f.area();
        if let Some((title, lines, w)) = self.modal_content(a) {
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
    }

    /// While dragging: keeps the frame (to copy the selection from), then shows the selection reversed.
    fn draw_selection(&mut self, f: &mut Frame) {
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

/// A pane's tab bar in `bar`: the thread-name `pill` (pane 0 only), then the tabs that fit, each
/// with its fade cells and "× ". Returns the spans and each shown tab's (x0, x1, index).
fn tab_bar(
    tabs: &Pane,
    focused: bool,
    pill: Option<&str>,
    bar: Rect,
) -> (Vec<Span<'static>>, Vec<(u16, u16, usize)>) {
    let mut spans = vec![];
    let pill = if let Some(thread) = pill {
        spans.push(Span::styled(
            format!(" {thread} "),
            Style::new()
                .fg(pal().crust)
                .bg(pal().subtext0)
                .add_modifier(Modifier::BOLD),
        ));
        thread.chars().count() as u16 + 2
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
        .map(|(t, _)| FADE + t.chars().count() as u16 + CLOSE_W)
        .collect(); // fade cells + title + "× "
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
        // Smooth left fade: FADE blank cells whose bg steps from the page bg to the tab bg.
        for k in 1..=FADE {
            let c = match (pal().mantle, bg) {
                (Color::Rgb(r0, g0, b0), Color::Rgb(r1, g1, b1)) => {
                    let m = |a: u8, b: u8| {
                        ((a as u16 * (FADE + 1 - k) + b as u16 * k) / (FADE + 1)) as u8
                    };
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
    (spans, tab_spans)
}

/// A live terminal, resized to `body` first; the cursor shows only when keys go to it and it is not scrolled back.
fn render_term(f: &mut Frame, live: &LiveAgent, body: Rect, cursor: bool) {
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

/// A diff tab: the diff above, the file's path right-aligned on the body's last row.
fn render_diff(f: &mut Frame, text: &str, scroll: &mut u16, rel: &str, body: Rect) {
    let [d, foot] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(body);
    let p = Paragraph::new(diff_lines(text)).wrap(Wrap { trim: false });
    *scroll = (*scroll).min(
        p.line_count(d.width)
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16,
    );
    f.render_widget(p.scroll((*scroll, 0)), d);
    f.render_widget(
        Paragraph::new(rel)
            .style(Style::new().fg(pal().overlay0))
            .right_aligned(),
        foot,
    );
}

/// A FLOWPLAN (or flow.md) step tree in `pad`; each visible task hit becomes a `task_btns` rect.
fn render_plan(
    f: &mut Frame,
    lines: Vec<Line<'static>>,
    scroll: &mut u16,
    pad: Rect,
    hits: Vec<(usize, Range<u16>, String)>,
    plan_path: &Path,
    task_btns: &mut Vec<(Rect, (PathBuf, String))>,
) {
    *scroll = (*scroll).min(lines.len().saturating_sub(1) as u16);
    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), pad);
    // One screen row per line (no wrap): line i sits at pad.y + i - scroll.
    let top = *scroll as usize;
    for (i, xs, a) in hits {
        if i < top || i - top >= pad.height as usize {
            continue;
        }
        let x = pad.x.saturating_add(xs.start).min(pad.right());
        let w = xs
            .end
            .min(pad.right().saturating_sub(pad.x))
            .saturating_sub(xs.start);
        let rect = Rect::new(x, pad.y + (i - top) as u16, w, 1);
        task_btns.push((rect, (plan_path.to_path_buf(), a)));
    }
}

/// Any other file: markdown rendered in `pad`, plain text wrapped in the whole `body`.
fn render_text(f: &mut Frame, path: &Path, text: &str, scroll: &mut u16, pad: Rect, body: Rect) {
    let (p, area) = if path.extension().is_some_and(|e| e == "md") {
        (Paragraph::new(md_text(text, pad.width)), pad)
    } else {
        (Paragraph::new(text), body)
    };
    let p = p.wrap(Wrap { trim: false });
    *scroll = (*scroll).min(
        p.line_count(area.width)
            .saturating_sub(1)
            .min(u16::MAX as usize) as u16,
    );
    f.render_widget(p.scroll((*scroll, 0)), area);
}
