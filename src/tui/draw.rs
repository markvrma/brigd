//! Drawing a frame: sidebar, buttons, panes and their tab bars, hint line, menu, modal and the drag selection.

use super::flowplan::{flow_lines, SPIN};
use super::markdown::{diff_lines, md_text};
use super::newt::{chunk, NEWT_W};
use super::sidebar::{layout, max_offset};
use super::tabs::{pane_rects, tab_window, tail, Tab};
use super::term::sel_rows;
use super::theme::{pal, stage_color};
use super::threads::live_agents;
use super::{App, SIDE_W};
use crate::tree::Kind;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

impl App {
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

    pub(super) fn draw(&mut self, f: &mut Frame) {
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

        let viewing = matches!(self.active_tab(), Some(Tab::View { .. }));
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
