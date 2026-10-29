//! Keyboard, mouse and paste input, routed by modal precedence to the open modal, menu, prefix command, tab or sidebar.

use super::sidebar::{hit, max_offset};
use super::tabs::{Tab, CLOSE_W};
use super::term::sel_text;
use super::theme::next_theme;
use super::threads::{Act, Menu};
use super::{App, MIN_SIDE_W, SIDE_W};
use crate::tree::{self, Kind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

/// Ctrl-o / Ctrl-t as bytes: pressed twice, the prefix key goes through to the tab.
const CTRL_O: u8 = 0x0f;
const CTRL_T: u8 = 0x14;
/// Lines a viewer scrolls per PgUp/PgDn (and space).
const PAGE: i32 = 20;
/// Lines (or sidebar rows) per wheel notch.
const WHEEL_STEP: i32 = 3;
/// A scroll far enough to reach either end once clamped (g / G).
const SCROLL_END: i32 = i32::MAX / 2;

/// A key as the bytes a terminal would send. `app_cursor` = DECCKM (arrows as ESC O x).
pub(super) fn key_bytes(k: KeyEvent, app_cursor: bool) -> Vec<u8> {
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
    pub(super) fn on_key(&mut self, k: KeyEvent) {
        // A modal that opened while a prefix was armed must not leave it armed behind it.
        if self.newt.as_ref().is_some_and(|n| !n.hidden)
            || self.confirm_run.is_some()
            || self.menu.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_quit
        {
            self.disarm_prefixes();
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
                _ if ctrl_o => self.send(&[CTRL_O]), // Ctrl-o twice sends one to claude
                KeyCode::Char('s') => self.focus_main = false,
                KeyCode::Char('n') => self.cycle(1),
                KeyCode::Char('p') => self.cycle(-1),
                KeyCode::Char('w') => self.close_tab(),
                KeyCode::Char('t') => self.new_term(),
                KeyCode::Char('v') => self.split(),
                KeyCode::Char('o') => self.other_pane(),
                KeyCode::Char('c') => self.flash(format!("theme: {}", next_theme())),
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
                _ if ctrl_t => self.send(&[CTRL_T]), // Ctrl-t twice sends one to claude
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
        match self.active_tab() {
            Some(Tab::Term { live, .. }) => {
                let app_cursor = live.parser.lock().unwrap().screen().application_cursor();
                self.send(&key_bytes(k, app_cursor));
            }
            Some(Tab::View { scroll, .. }) => {
                let d = match k.code {
                    KeyCode::Up | KeyCode::Char('k') => -1,
                    KeyCode::Down | KeyCode::Char('j') => 1,
                    KeyCode::PageUp => -PAGE,
                    KeyCode::PageDown | KeyCode::Char(' ') => PAGE,
                    KeyCode::Home | KeyCode::Char('g') => -SCROLL_END,
                    KeyCode::End | KeyCode::Char('G') => SCROLL_END,
                    KeyCode::Tab | KeyCode::Esc => return self.focus_main = false,
                    _ => 0,
                };
                *scroll = (*scroll as i32 + d).clamp(0, u16::MAX as i32) as u16;
                // draw clamps to the rendered line count
            }
            None => self.focus_main = false,
        }
    }

    pub(super) fn on_mouse(&mut self, m: MouseEvent) {
        if self.newt.as_ref().is_some_and(|n| !n.hidden) || self.confirm_run.is_some() {
            return;
        }
        let at = Position {
            x: m.column,
            y: m.row,
        };
        if matches!(m.kind, MouseEventKind::Down(_)) {
            self.disarm_prefixes();
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
                    self.disarm_prefixes();
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
                            if m.column + CLOSE_W >= x1 {
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
                            let t = self.focused_pane();
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
                let d = if back { -WHEEL_STEP } else { WHEEL_STEP };
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

    /// Drops a pending Ctrl-o / Ctrl-t prefix.
    fn disarm_prefixes(&mut self) {
        (self.prefix, self.thread_prefix) = (false, false);
    }

    pub(super) fn on_paste(&mut self, s: &str) {
        if let Some(n) = self.newt.as_mut().filter(|n| !n.hidden) {
            if !n.planning && !n.ask {
                n.fields[n.focus].push_str(&s.replace(['\r', '\n'], " "));
            }
            return;
        }
        if self.confirm_run.is_some() || !self.focus_main {
            return;
        }
        let Some(Tab::Term { live, .. }) = self.active_tab() else {
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
}
