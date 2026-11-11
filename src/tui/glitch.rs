//! The `glitch` idle animation: a damaged panel. Coloured vertical stripes (some bands dead and
//! see-through), horizontal tears and flicker. All of it
//! is a pure function of (column, row, tick), so there is no state to keep.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Muted stripe colours. A band takes one of these 1 time in 5; every other band is dead and
/// shows the screen underneath, so the panel looks mostly fine with a few bad columns.
const PALETTE: [(u8, u8, u8); 6] = [
    (0, 95, 120),
    (120, 0, 95),
    (20, 120, 35),
    (15, 40, 120),
    (120, 20, 20),
    (120, 105, 0),
];

fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B).rotate_left(13);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^ (h >> 12)
}

fn put(buf: &mut Buffer, area: Rect, x: i32, y: i32, c: char, fg: Color) {
    if x >= 0 && y >= 0 && (x as u32) < area.width as u32 && (y as u32) < area.height as u32 {
        buf[(area.x + x as u16, area.y + y as u16)]
            .set_char(c)
            .set_fg(fg)
            .set_bg(Color::Reset);
    }
}

/// The stripe colour of column `x` at `tick`; a few bands re-roll every tick (flicker).
fn stripe(x: i32, tick: u32) -> Option<Color> {
    let x = x.max(0) as u32;
    let band = x / (hash(x / 3, 11) % 3 + 1);
    let flick = hash(band, tick).is_multiple_of(150);
    let i = hash(band, 5 + flick as u32 * (tick / 2 + 1)) % 30;
    let (r, g, b) = *PALETTE.get(i as usize)?;
    Some(Color::Rgb(r, g, b))
}

pub(super) fn paint(buf: &mut Buffer, area: Rect, tick: usize) {
    let tick = tick as u32;
    let (w, h) = (area.width as i32, area.height as i32);
    for y in 0..h {
        // A tear shifts a few rows sideways; now and then a row is pure noise.
        let tr = hash(y as u32 / 2, tick / 3);
        let shift = if tr.is_multiple_of(25) {
            (tr >> 8) as i32 % 5 - 2
        } else {
            0
        };
        let noise = tr % 211 == 1;
        for x in 0..w {
            let n = hash(x as u32 * 977 + y as u32, tick / 2);
            if noise {
                let (r, g, b) = PALETTE[(n % 6) as usize];
                put(buf, area, x, y, '░', Color::Rgb(r, g, b));
            } else if let Some(c) = stripe(x + shift, tick) {
                let ch = match n % 10 {
                    0 | 1 => continue, // gaps, so the stripes look worn, not solid
                    2 => '▒',
                    _ => '░',
                };
                put(buf, area, x, y, ch, c);
            }
        }
    }
}
