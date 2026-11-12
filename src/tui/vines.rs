//! Braille vines for the idle screen: stems sprout along the bottom, climb, fork and curl into
//! tendrils until the screen is a thicket, then keep swaying. Like the site's braille spirals, the
//! picture is a grid of dots (2 wide, 4 tall per cell) that discs are stamped onto, then written
//! out as U+28xx characters. Nothing is stored between frames: `paint` redraws the whole plant
//! from the time `t`, so branch positions are a pure function of (screen size, t).

use super::theme::pal;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Duration;

/// Dot values: 0 empty, 1 stem, 2 growing tip (drawn in a brighter color).
const STEM: u8 = 1;
const TIP: u8 = 2;
/// Branch generations below the trunk: trunk, branches, twigs, sprigs.
const MAX_DEPTH: u32 = 3;
/// Dot bit values of a braille cell, indexed [dx][dy] (Unicode order: 1,2,3 then 7 down the left column).
const BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// A deterministic hash to 0..1: the same seed always grows the same plant.
fn rnd(seed: u64, k: u64) -> f32 {
    let mut z = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ k.wrapping_add(0x632B_E59B_D9B4_E019);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

struct Plant {
    dots: Vec<u8>,
    w: i32,
    h: i32,
    /// Seconds: drives the sway.
    t: f32,
    /// How far (in dots) every branch may have grown by now.
    g: f32,
}

impl Plant {
    /// A filled disc, so a stroke has weight (a one-dot line is faint in braille).
    fn stamp(&mut self, cx: f32, cy: f32, r: f32, v: u8) {
        let (x0, x1) = (
            (cx - r).floor().max(0.0) as i32,
            (cx + r).ceil().min(self.w as f32 - 1.0) as i32,
        );
        let (y0, y1) = (
            (cy - r).floor().max(0.0) as i32,
            (cy + r).ceil().min(self.h as f32 - 1.0) as i32,
        );
        for y in y0..=y1 {
            for x in x0..=x1 {
                let (dx, dy) = (x as f32 - cx, y as f32 - cy);
                if dx * dx + dy * dy <= r * r {
                    let d = &mut (self.dots[(y * self.w + x) as usize]);
                    *d = (*d).max(v);
                }
            }
        }
    }

    /// Grows one branch from (x, y) heading `th0` (radians, y down), `len` dots long, starting to
    /// grow when the plant has grown `delay`. Forks spawn child branches as it passes them.
    #[allow(clippy::too_many_arguments)]
    fn branch(
        &mut self,
        seed: u64,
        depth: u32,
        (mut x, mut y): (f32, f32),
        th0: f32,
        len: f32,
        delay: f32,
        r0: f32,
    ) {
        let grown = (self.g - delay).min(len);
        // The last stretch winds into a tightening spiral (a tendril).
        let curl = (len * 0.45).clamp(14.0, 40.0).min(len);
        let side = if rnd(seed, 1) < 0.5 { -1.0 } else { 1.0 };
        let (amp, freq, phase) = (
            0.35 + 0.45 * rnd(seed, 2),
            0.04 + 0.04 * rnd(seed, 3),
            rnd(seed, 4) * TAU,
        );
        let gap = 16.0 + 10.0 * rnd(seed, 5);
        let (mut next_fork, mut k, mut s) = (gap * 0.8, 0u64, 0.0f32);
        while s < grown {
            let u = ((s - (len - curl)) / curl).max(0.0);
            // Sway grows toward the tip, like a real stem in a draught.
            let sway = 0.22 * (s / len) * (s * 0.035 - self.t * 0.5 + phase).sin();
            let th = th0 + amp * (s * freq + phase).sin() + sway + side * 5.0 * u * u;
            (x, y) = (x + th.cos(), y + th.sin());
            let growing = grown < len && s > grown - 8.0;
            self.stamp(
                x,
                y,
                (r0 * (1.0 - 0.55 * s / len)).max(0.5),
                if growing { TIP } else { STEM },
            );
            if depth > 0 && s % 11.0 < 1.0 && s < len * 0.9 {
                // A leaf: a small disc off the stem, alternating sides.
                let off = if ((s / 11.0) as u32).is_multiple_of(2) {
                    2.6
                } else {
                    -2.6
                };
                self.stamp(x - th.sin() * off, y + th.cos() * off, 1.3, STEM);
            }
            if depth < MAX_DEPTH && s >= next_fork && s < len * 0.8 {
                let cs = seed.wrapping_mul(31).wrapping_add(k + 7);
                let dir = if k % 2 == 0 { 1.0 } else { -1.0 };
                let child_len = ((len - s) * (0.55 + 0.25 * rnd(cs, 1))).max(14.0);
                self.branch(
                    cs,
                    depth + 1,
                    (x, y),
                    th + dir * (0.9 + 0.5 * rnd(cs, 2)),
                    child_len,
                    delay + s,
                    r0 * 0.6,
                );
                (k, next_fork) = (k + 1, next_fork + gap);
            }
            s += 1.0;
        }
    }
}

/// Draws the plant at time `t` into `area`; `secs` is how long until every branch is fully grown.
pub(super) fn paint(buf: &mut Buffer, area: Rect, t: Duration, secs: u64, style: Style) {
    let (w, h) = (area.width as i32 * 2, area.height as i32 * 4);
    if w == 0 || h == 0 {
        return;
    }
    // Every branch ends no later than its parent, so all are done once the longest trunk is.
    let longest = h as f32 * 1.1;
    let mut plant = Plant {
        dots: vec![0; (w * h) as usize],
        w,
        h,
        t: t.as_secs_f32(),
        g: (t.as_secs_f32() / secs as f32).min(1.0) * longest,
    };
    // Trunks every ~26 dots along the bottom, each a little different.
    let n = (w / 26).clamp(3, 24) as u64;
    for i in 0..n {
        let x = w as f32 * (i as f32 + 0.5 + 0.6 * (rnd(i, 100) - 0.5)) / n as f32;
        let th0 = -FRAC_PI_2 + 0.25 * (rnd(i, 101) - 0.5);
        let len = h as f32 * (0.65 + 0.45 * rnd(i, 102));
        plant.branch(i * 1000 + 1, 0, (x, h as f32 + 2.0), th0, len, 0.0, 1.9);
    }
    let tip = Style::new().fg(pal().yellow);
    for cy in 0..area.height {
        for cx in 0..area.width {
            let (mut bits, mut v) = (0, 0);
            for (dx, col) in BITS.iter().enumerate() {
                for (dy, bit) in col.iter().enumerate() {
                    let d = plant.dots
                        [((cy as i32 * 4 + dy as i32) * w + cx as i32 * 2 + dx as i32) as usize];
                    if d > 0 {
                        bits |= bit;
                        v = v.max(d);
                    }
                }
            }
            if let Some(c) = (bits > 0).then(|| char::from_u32(0x2800 + bits)).flatten() {
                buf[(area.x + cx, area.y + cy)]
                    .set_char(c)
                    .set_style(if v == TIP { tip } else { style });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn cells(w: u16, h: u16, t: Duration) -> Vec<(u16, u16)> {
        let area = Rect::new(0, 0, w, h);
        let mut b = Buffer::empty(area);
        paint(&mut b, area, t, 300, Style::new());
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| b[(x, y)].symbol() != " ")
            .collect()
    }

    #[test]
    fn starts_empty_and_climbs_from_the_bottom() {
        assert!(cells(80, 24, Duration::ZERO).is_empty());
        let early = cells(80, 24, 30 * S);
        assert!(!early.is_empty());
        assert!(early.iter().all(|&(_, y)| y >= 16), "{early:?}"); // still low on the screen
        let full = cells(80, 24, 300 * S);
        assert!(full.iter().any(|&(_, y)| y < 8), "reaches the top third");
        assert!(full.len() > early.len() * 4);
    }

    #[test]
    fn keeps_swaying_once_grown() {
        assert_ne!(cells(80, 24, 400 * S), cells(80, 24, 420 * S));
    }

    #[test]
    fn same_time_same_picture() {
        assert_eq!(cells(60, 20, 123 * S), cells(60, 20, 123 * S));
    }

    #[test]
    fn survives_tiny_and_huge_screens() {
        for (w, h) in [(0, 0), (1, 1), (2, 1), (5, 3), (300, 80)] {
            for t in [Duration::ZERO, 100 * S, 300 * S, 5000 * S] {
                cells(w, h, t);
            }
        }
    }
}
