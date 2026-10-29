//! Host terminal plumbing: raw mode/alt screen setup and teardown (plus panic hook), the clipboard, and mouse-selection text.

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{self as ct, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use std::io::{self, Write};
use std::process::{Command, Stdio};

pub(super) fn setup() -> io::Result<()> {
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

pub(super) fn teardown() {
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
pub(super) fn copy_to_clipboard(s: &str) {
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

pub(super) fn base64(b: &[u8]) -> String {
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
pub(super) fn sel_rows(
    r: Rect,
    a: Position,
    b: Position,
) -> Vec<(u16, std::ops::RangeInclusive<u16>)> {
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
pub(super) fn sel_text(buf: &Buffer, r: Rect, a: Position, b: Position) -> String {
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
