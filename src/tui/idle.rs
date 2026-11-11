//! The idle screen: after `IDLE_AFTER` with no input an ASCII animation covers brigd until a key,
//! click, scroll or paste wakes it. Animations are text files: two are built in (anims/), others
//! are picked by name (see `load_anim`).
//!
//! File format: line 1 is `fps=<n> walk` or `fps=<n> grow=<secs to fill>`, optionally followed by `dim`; then optional `#`
//! comment lines; then frames separated by a line that is exactly `---`. Tabs become 4 spaces,
//! trailing spaces are ignored, and every frame is padded to the widest and tallest frame so
//! nothing jitters. `walk` slides the sprite along the bottom; `grow` tiles the frame over the
//! screen (so it must tile seamlessly) and reveals it from the bottom up in `secs`.

use super::theme::pal;
use super::App;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::Frame;
use std::path::Path;
use std::time::Duration;
use std::{env, fs};

/// No input for this long starts the animation ($BRIGD_IDLE_AFTER overrides, in seconds).
pub(super) const IDLE_AFTER: Duration = Duration::from_secs(300);
/// The run loop draws every 40ms, so a faster animation would only skip frames.
const MAX_FPS: u32 = 25;

#[derive(Debug, PartialEq)]
pub(super) enum Mode {
    /// The sprite moves one column per frame tick, left to right, then starts over at the left.
    Walk,
    /// Seconds until the tiled frame covers the whole screen.
    Grow(u64),
    /// A damaged panel, drawn by `glitch.rs`; the file needs no frames.
    Glitch,
}

#[derive(Debug)]
pub(super) struct Anim {
    /// Padded to `w` x `h`, rows joined by '\n'.
    pub(super) frames: Vec<String>,
    pub(super) fps: u16,
    pub(super) mode: Mode,
    /// Header word `dim`: the screen under the animation is dimmed.
    pub(super) dim: bool,
    pub(super) w: u16,
    pub(super) h: u16,
}

/// Parses an animation file; a bad or missing header, or no frames, gives None.
pub(super) fn parse(src: &str) -> Option<Anim> {
    let src = src.replace('\t', "    ");
    let mut lines = src.lines(); // also strips the \r of CRLF
    let mut head = lines.next()?.split_whitespace();
    let fps = head.next()?.strip_prefix("fps=")?.parse::<u32>().ok()?;
    let mode = match head.next()? {
        "walk" => Mode::Walk,
        "glitch" => Mode::Glitch,
        g => Mode::Grow(g.strip_prefix("grow=")?.parse::<u64>().ok()?.max(1)),
    };
    let dim = match head.next() {
        None => false,
        Some("dim") => true,
        Some(_) => return None,
    };
    if head.next().is_some() {
        return None;
    }
    if mode == Mode::Glitch {
        return Some(Anim {
            fps: fps.clamp(1, MAX_FPS) as u16,
            mode,
            dim,
            w: 1,
            h: 1,
            frames: vec![" ".into()],
        });
    }
    let mut frames: Vec<Vec<&str>> = vec![vec![]];
    for l in lines.skip_while(|l| l.starts_with('#')) {
        match l {
            "---" => frames.push(vec![]),
            _ => frames.last_mut()?.push(l.trim_end()),
        }
    }
    frames.retain(|f| !f.is_empty());
    let w = frames
        .iter()
        .flatten()
        .map(|l| Span::raw(*l).width())
        .max()?;
    let h = frames.iter().map(Vec::len).max()?;
    if w == 0 {
        return None; // nothing to draw (and grow tiles by the width)
    }
    let pad = |f: &Vec<&str>| {
        (0..h)
            .map(|i| {
                let l = f.get(i).copied().unwrap_or("");
                format!("{l}{}", " ".repeat(w - Span::raw(l).width()))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Some(Anim {
        fps: fps.clamp(1, MAX_FPS) as u16,
        mode,
        dim,
        w: u16::try_from(w).ok()?,
        h: u16::try_from(h).ok()?,
        frames: frames.iter().map(pad).collect(),
    })
}

/// How far a column of `screen_h` rows has grown after `t`: 0 at t=0, all of it at `secs`. Each
/// column grows 1.0 to 1.2 times as fast as the slowest (fixed per column), so the top edge is ragged.
pub(super) fn grow_h(col: u16, screen_h: u16, t: Duration, secs: u64) -> u16 {
    let k = 1.0 + (col as u32 * 7 % 5) as f32 * 0.05;
    let p = (t.as_secs_f32() / secs as f32 * k).min(1.0);
    (screen_h as f32 * p).ceil() as u16
}

impl Anim {
    fn tick(&self, t: Duration) -> usize {
        (t.as_millis() * self.fps as u128 / 1000) as usize
    }

    pub(super) fn frame_at(&self, t: Duration) -> usize {
        self.tick(t) % self.frames.len()
    }

    /// The sprite's left column: it pops in whole at the left, walks to the right edge and starts over.
    pub(super) fn walk_x(&self, screen_w: u16, t: Duration) -> u16 {
        (self.tick(t) % (screen_w.saturating_sub(self.w) as usize + 1)) as u16
    }

    /// Draws the frame for time `t` inside `area`; every write is clipped to it.
    pub(super) fn paint(&self, buf: &mut Buffer, area: Rect, t: Duration, style: Style) {
        let n = self.frame_at(t);
        match self.mode {
            Mode::Glitch => super::glitch::paint(buf, area, self.tick(t)),
            Mode::Walk => {
                let x = area.x + self.walk_x(area.width, t);
                // Stands on the row above the bottom one (the hint line).
                let y = area.y + area.height.saturating_sub(self.h + 1);
                for (dy, line) in self.frames[n].lines().enumerate() {
                    let y = y.saturating_add(dy as u16);
                    // Spaces are transparent: the screen shows through the sprite.
                    for (i, c) in line.chars().enumerate() {
                        let cx = x.saturating_add(i as u16);
                        if c != ' ' && y < area.bottom() && cx < area.right() {
                            buf[(cx, y)].set_char(c).set_style(style);
                        }
                    }
                }
            }
            Mode::Grow(secs) => {
                // ponytail: one char per column, so wide glyphs draw misaligned (a short row just reads as blank)
                let rows: Vec<Vec<Vec<char>>> = (0..self.frames.len())
                    .map(|i| {
                        let f = &self.frames[(n + i) % self.frames.len()];
                        f.lines().map(|l| l.chars().collect()).collect()
                    })
                    .collect();
                for x in 0..area.width {
                    // Neighbouring tiles are out of step: tile i shows frame n + i.
                    let tile = &rows[(x / self.w) as usize % rows.len()];
                    let shown = grow_h(x, area.height, t, secs);
                    for y in area.height - shown..area.height {
                        let c = tile[y as usize % tile.len()]
                            .get((x % self.w) as usize)
                            .copied()
                            .unwrap_or(' ');
                        if c != ' ' {
                            buf[(area.x + x, area.y + y)].set_char(c).set_style(style);
                        }
                    }
                }
            }
        }
    }
}

fn builtin(name: &str) -> Option<&'static str> {
    match name {
        "monkey" => Some(include_str!("anims/monkey.txt")),
        "vines" => Some(include_str!("anims/vines.txt")),
        "glitch" => Some(include_str!("anims/glitch.txt")),
        _ => None,
    }
}

/// The text behind a name: a path if it has a '/', else ~/.brigd/anims/<name>.txt, else a built-in.
/// None = no such animation, which turns the feature off (so `off` needs no special case).
fn source(name: &str, root: &Path) -> Option<String> {
    if name.contains('/') {
        return Some(fs::read_to_string(name).unwrap_or_default()); // unreadable: fails to parse
    }
    fs::read_to_string(root.join("anims").join(format!("{name}.txt")))
        .ok()
        .or_else(|| builtin(name).map(String::from))
}

/// The animation and, when its file is unusable, a message for the startup flash.
/// $BRIGD_IDLE, else ~/.brigd/idle, else `monkey`. A bad file disables it (no silent fallback).
pub(super) fn load_anim() -> (Option<Anim>, Option<String>) {
    let root = crate::store::root();
    let name = env::var("BRIGD_IDLE")
        .or_else(|_| fs::read_to_string(root.join("idle")))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "monkey".into());
    match source(&name, &root).map(|t| parse(&t)) {
        None => (None, None),
        Some(Some(a)) => (Some(a), None),
        Some(None) => (None, Some(format!("idle anim {name}: bad file"))),
    }
}

/// $BRIGD_IDLE_AFTER seconds (above 0), else `IDLE_AFTER`.
pub(super) fn idle_after() -> Duration {
    env::var("BRIGD_IDLE_AFTER")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&n| n > 0)
        .map_or(IDLE_AFTER, Duration::from_secs)
}

impl App {
    /// True while the animation covers the screen. `last_input` None (tests, before `run`) is never idle.
    pub(super) fn idle(&self) -> bool {
        self.idle_anim.is_some()
            && self
                .last_input
                .is_some_and(|t| t.elapsed() >= self.idle_after)
    }

    /// The cover: the animation (its clock starts when idleness does) and a dim hint on the bottom row.
    pub(super) fn draw_idle(&self, f: &mut Frame) {
        let (Some(a), Some(last)) = (&self.idle_anim, self.last_input) else {
            return;
        };
        let area = f.area();
        if area.is_empty() {
            return;
        }
        let fg = match a.mode {
            Mode::Walk | Mode::Glitch => pal().peach,
            Mode::Grow(_) => pal().green,
        };
        let t = last.elapsed().saturating_sub(self.idle_after);
        if a.dim {
            f.buffer_mut().set_style(area, Modifier::DIM);
        }
        a.paint(f.buffer_mut(), area, t, Style::new().fg(fg));
        let hint = match self.blocked.len() {
            0 => " any key ".to_string(),
            n => format!(" {n} blocked · any key "),
        };
        let w = (Span::raw(&hint).width() as u16).min(area.width);
        f.buffer_mut().set_stringn(
            area.x + (area.width - w) / 2,
            area.bottom() - 1,
            &hint,
            w as usize,
            Style::new().fg(pal().overlay0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn anim(src: &str) -> Anim {
        parse(src).unwrap()
    }

    #[test]
    fn parses_header_comments_and_frames() {
        let a = anim("fps=8 walk\n# a comment\n# another\nab\n---\ncd\n---\nef\n");
        assert_eq!((a.fps, a.mode, a.frames.len()), (8, Mode::Walk, 3));
        assert_eq!(a.frames[0], "ab");
        let g = anim("fps=3 grow=60\n#x\nab\n");
        assert_eq!((g.fps, g.mode), (3, Mode::Grow(60)));
    }

    #[test]
    fn bad_input_is_none() {
        for bad in [
            "",
            "walk\nab",
            "fps=x walk\nab",
            "fps=5\nab",
            "fps=5 fly\nab",
            "fps=5 grow=x\nab",
            "fps=5 walk extra\nab",
            "fps=5 walk dim extra\nab",
            "fps=5 walk\n",
            "fps=5 walk\n# only comments\n",
            "fps=5 walk\n \n---\n  \n", // nothing visible
        ] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn crlf_tabs_trailing_spaces_and_padding() {
        let a = anim("fps=5 walk\r\n\tx  \r\n---\r\nabc\r\ndef\r\n");
        // widest is 5 columns (tab + x); tallest is 2 rows; both are padded to it
        assert_eq!((a.w, a.h), (5, 2));
        assert_eq!(a.frames, ["    x\n     ", "abc  \ndef  "]);
        // leading spaces stay, and width is in columns, not bytes
        let a = anim("fps=5 walk\n  ┃\n---\n┗┓\n");
        assert_eq!((a.w, a.h), (3, 1));
        assert_eq!(a.frames, ["  ┃", "┗┓ "]);
    }

    #[test]
    fn dim_is_an_optional_header_word() {
        assert!(!anim("fps=5 walk\nx").dim);
        assert!(anim("fps=5 walk dim\nx").dim);
        assert!(anim("fps=5 grow=9 dim\nx").dim);
    }

    #[test]
    fn fps_is_clamped() {
        assert_eq!(anim("fps=0 walk\nx").fps, 1);
        assert_eq!(anim("fps=9999 walk\nx").fps, 25);
        assert_eq!(anim("fps=12 walk\nx").fps, 12);
    }

    #[test]
    fn frames_cycle_and_the_sprite_walks_and_wraps() {
        let a = anim("fps=10 walk\nab\n---\ncd\n---\nef\n"); // w=2: 9 positions on 10 columns
        assert_eq!(a.frame_at(Duration::ZERO), 0);
        assert_eq!(a.frame_at(Duration::from_millis(100)), 1);
        assert_eq!(a.frame_at(Duration::from_millis(300)), 0);
        let xs: Vec<_> = (0..11)
            .map(|i| a.walk_x(10, Duration::from_millis(i * 100)))
            .collect();
        assert_eq!(xs, [0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 1]);
        // wider than the screen: stays put, no underflow
        assert_eq!(a.walk_x(1, 5 * S), 0);
        assert_eq!(a.walk_x(0, 5 * S), 0);
    }

    #[test]
    fn grow_starts_empty_only_rises_and_is_full_on_time() {
        for col in 0..40 {
            assert_eq!(grow_h(col, 30, Duration::ZERO, 300), 0);
            assert_eq!(grow_h(col, 30, 300 * S, 300), 30, "col {col}");
            assert_eq!(grow_h(col, 30, 900 * S, 300), 30);
            let hs: Vec<_> = (0..=300).map(|s| grow_h(col, 30, S * s, 300)).collect();
            assert!(hs.windows(2).all(|w| w[0] <= w[1]), "col {col}");
            assert!(hs.iter().all(|&h| h <= 30));
        }
        // columns differ in speed, so the edge is ragged
        let mid: Vec<_> = (0..5).map(|c| grow_h(c, 30, 100 * S, 300)).collect();
        assert!(mid.iter().any(|&h| h != mid[0]));
        assert_eq!(grow_h(0, 0, 10 * S, 10), 0);
    }

    #[test]
    fn paint_clips_to_a_tiny_screen() {
        let walk = anim("fps=5 walk\nabcd\nefgh\n");
        let grow = anim("fps=5 grow=10\nab\ncd\n");
        let glitch = anim(builtin("glitch").unwrap());
        for (w, h) in [(0, 0), (1, 1), (3, 1), (2, 5), (9, 9), (120, 40)] {
            let area = Rect::new(0, 0, w, h);
            for a in [&walk, &grow, &glitch] {
                for t in [Duration::ZERO, 5 * S, 99 * S] {
                    a.paint(&mut Buffer::empty(area), area, t, Style::new());
                }
            }
        }
        // the walker stands above the bottom row
        let area = Rect::new(0, 0, 6, 4);
        let mut b = Buffer::empty(area);
        walk.paint(&mut b, area, Duration::ZERO, Style::new());
        assert_eq!(b[(0, 1)].symbol(), "a");
        assert_eq!(b[(3, 2)].symbol(), "h");
        // a wide glyph in a grow frame (fewer chars than columns) must not panic
        let wide = anim("fps=5 grow=10\n界\n");
        for (w, h) in [(1, 1), (7, 3)] {
            let area = Rect::new(0, 0, w, h);
            wide.paint(&mut Buffer::empty(area), area, 10 * S, Style::new());
        }
        // a full grow screen is covered edge to edge, and empty at the start
        let (mut full, mut empty) = (Buffer::empty(area), Buffer::empty(area));
        grow.paint(&mut full, area, 10 * S, Style::new());
        grow.paint(&mut empty, area, Duration::ZERO, Style::new());
        assert!(full.content().iter().all(|c| c.symbol() != " "));
        assert!(empty.content().iter().all(|c| c.symbol() == " "));
    }

    #[test]
    fn built_ins_parse_and_stay_in_shape() {
        let m = anim(builtin("monkey").unwrap());
        assert_eq!(m.mode, Mode::Walk);
        assert!((6..=8).contains(&m.frames.len()), "{}", m.frames.len());
        assert!((10..=16).contains(&m.h), "{}", m.h);
        let v = anim(builtin("vines").unwrap());
        assert_eq!(v.mode, Mode::Grow(300));
        assert!(v.frames.len() >= 4);
        assert_eq!(anim(builtin("glitch").unwrap()).mode, Mode::Glitch);
        assert!(builtin("off").is_none());
    }

    #[test]
    fn names_resolve_without_touching_the_real_home() {
        let nowhere = Path::new("/nonexistent/brigd-test-root");
        assert!(source("monkey", nowhere).is_some());
        assert!(source("off", nowhere).is_none());
        assert!(source("no-such-anim", nowhere).is_none());
        // a path that can't be read is "bad", not "off"
        assert_eq!(
            source("/nonexistent/anim.txt", nowhere).as_deref(),
            Some("")
        );
        assert!(parse("").is_none());
    }
}
