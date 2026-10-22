//! Box drawing and block elements (U+2500 to U+259F), drawn to fill their
//! cell: the glyphs of fonts rarely span the whole height of a cell, and
//! frames showed gaps between rows (see Spec-Native-grid).

use gtk::cairo;

/// The arms of each box drawing char, 2 bits each from the lowest: up,
/// right, down, left; 0 none, 1 light, 2 heavy, 3 double. Generated from
/// the Unicode names of the chars; 0 for those not drawn as arms: dashes,
/// which keep the font's glyphs, arcs and diagonals.
const ARMS: [u8; 128] = [
    0x44, 0x88, 0x11, 0x22, 0x00, 0x00, 0x00, 0x00, // ─━│┃┄┅┆┇
    0x00, 0x00, 0x00, 0x00, 0x14, 0x18, 0x24, 0x28, // ┈┉┊┋┌┍┎┏
    0x50, 0x90, 0x60, 0xa0, 0x05, 0x09, 0x06, 0x0a, // ┐┑┒┓└┕┖┗
    0x41, 0x81, 0x42, 0x82, 0x15, 0x19, 0x16, 0x25, // ┘┙┚┛├┝┞┟
    0x26, 0x1a, 0x29, 0x2a, 0x51, 0x91, 0x52, 0x61, // ┠┡┢┣┤┥┦┧
    0x62, 0x92, 0xa1, 0xa2, 0x54, 0x94, 0x58, 0x98, // ┨┩┪┫┬┭┮┯
    0x64, 0xa4, 0x68, 0xa8, 0x45, 0x85, 0x49, 0x89, // ┰┱┲┳┴┵┶┷
    0x46, 0x86, 0x4a, 0x8a, 0x55, 0x95, 0x59, 0x99, // ┸┹┺┻┼┽┾┿
    0x56, 0x65, 0x66, 0x96, 0x5a, 0xa5, 0x69, 0x9a, // ╀╁╂╃╄╅╆╇
    0xa9, 0xa6, 0x6a, 0xaa, 0x00, 0x00, 0x00, 0x00, // ╈╉╊╋╌╍╎╏
    0xcc, 0x33, 0x1c, 0x34, 0x3c, 0xd0, 0x70, 0xf0, // ═║╒╓╔╕╖╗
    0x0d, 0x07, 0x0f, 0xc1, 0x43, 0xc3, 0x1d, 0x37, // ╘╙╚╛╜╝╞╟
    0x3f, 0xd1, 0x73, 0xf3, 0xdc, 0x74, 0xfc, 0xcd, // ╠╡╢╣╤╥╦╧
    0x47, 0xcf, 0xdd, 0x77, 0xff, 0x00, 0x00, 0x00, // ╨╩╪╫╬╭╮╯
    0x00, 0x00, 0x00, 0x00, 0x40, 0x01, 0x04, 0x10, // ╰╱╲╳╴╵╶╷
    0x80, 0x02, 0x08, 0x20, 0x48, 0x21, 0x84, 0x12, // ╸╹╺╻╼╽╾╿
];

/// Whether raterm draws `c` itself, instead of the font.
pub(super) fn drawn(c: &str) -> bool {
    let mut chars = c.chars();
    match (chars.next(), chars.next()) {
        (Some(c @ '\u{2500}'..='\u{257f}'), None) => {
            ARMS[c as usize - 0x2500] != 0 || ('\u{256d}'..='\u{2573}').contains(&c)
        }
        (Some('\u{2580}'..='\u{259f}'), None) => true,
        _ => false,
    }
}

/// Draws `c` (one of `drawn`) in the cell at `x`, `y`, in the current
/// source color of `cr`.
pub(super) fn draw(cr: &cairo::Context, c: &str, x: f64, y: f64, cw: f64, ch: f64) {
    let Some(c) = c.chars().next() else { return };
    // On whole pixels, so that cells next to each other join.
    let (x0, x1, y0, y1) = (x.round(), (x + cw).round(), y.round(), (y + ch).round());
    let (w, h) = (x1 - x0, y1 - y0);
    let light = (w / 8.0).round().max(1.0);
    let fill = |l: f64, t: f64, r: f64, b: f64| {
        cr.rectangle(l.round(), t.round(), (r - l).round(), (b - t).round());
    };
    match c as u32 {
        0x2580..=0x259f => blocks(c as u32, x0, y0, w, h, fill, cr),
        0x256d..=0x2573 => {
            let (cx, cy) = (
                x0 + (w / 2.0).floor() + light / 2.0,
                y0 + (h / 2.0).floor() + light / 2.0,
            );
            cr.set_line_width(light);
            match c {
                '╭' => curve(cr, (x1, cy), (cx, cy), (cx, y1)),
                '╮' => curve(cr, (x0, cy), (cx, cy), (cx, y1)),
                '╯' => curve(cr, (x0, cy), (cx, cy), (cx, y0)),
                '╰' => curve(cr, (x1, cy), (cx, cy), (cx, y0)),
                _ => {
                    if c != '╲' {
                        cr.move_to(x1, y0);
                        cr.line_to(x0, y1);
                    }
                    if c != '╱' {
                        cr.move_to(x0, y0);
                        cr.line_to(x1, y1);
                    }
                }
            }
            let _ = cr.stroke();
            return;
        }
        _ => lines(ARMS[c as usize - 0x2500], x0, y0, x1, y1, light, fill),
    }
    let _ = cr.fill();
}

/// A rounded corner from `from` to `to` around `corner`.
fn curve(cr: &cairo::Context, from: (f64, f64), corner: (f64, f64), to: (f64, f64)) {
    cr.move_to(from.0, from.1);
    cr.curve_to(corner.0, corner.1, corner.0, corner.1, to.0, to.1);
}

/// The arms of a box drawing char, from the center to the edges of the
/// cell. Double arms are two light lines, which stop where the lines of a
/// double arm across them run, as in `╔` or `╬`.
fn lines(
    arms: u8,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    light: f64,
    fill: impl Fn(f64, f64, f64, f64),
) {
    let [up, right, down, left] = [0, 2, 4, 6].map(|s| (arms >> s) & 3);
    let thick = |a: u8| match a {
        0 => 0.0,
        2 => 2.0 * light,
        _ => light,
    };
    // The center, and the distance of the lines of a double arm to it.
    let cx = x0 + ((x1 - x0 - light) / 2.0).round() + light / 2.0;
    let cy = y0 + ((y1 - y0 - light) / 2.0).round() + light / 2.0;
    let d = light;
    // How far past the center a line beside `near` (and across from `far`)
    // starts, for the arms across them.
    let start = |near: u8, far: u8| match (near, far) {
        (3, _) => d - light / 2.0,
        (1 | 2, _) => -thick(near) / 2.0,
        (0, 3) => -d - light / 2.0,
        (0, 1 | 2) => -thick(far) / 2.0,
        _ => 0.0,
    };
    // Single and heavy arms reach across what they meet.
    let reach = |a: u8, b: u8| {
        if a == 3 || b == 3 {
            d + light / 2.0
        } else {
            thick(a).max(thick(b)) / 2.0
        }
    };
    let band = |c: f64, t: f64| (c - t / 2.0, c + t / 2.0);
    for (arm, horizontal, outward) in [
        (right, true, true),
        (left, true, false),
        (down, false, true),
        (up, false, false),
    ] {
        let (near, far, center, across) = if horizontal {
            (up, down, cx, cy)
        } else {
            (left, right, cy, cx)
        };
        let (edge0, edge1) = if horizontal { (x0, x1) } else { (y0, y1) };
        let segment = |at: f64, t: f64, from: f64| {
            let (a, b) = if outward {
                (center + from, edge1)
            } else {
                (edge0, center - from)
            };
            let (p, q) = band(at, t);
            if horizontal {
                fill(a, p, b, q)
            } else {
                fill(p, a, q, b)
            }
        };
        match arm {
            0 => {}
            3 => {
                segment(across - d, light, start(near, far));
                segment(across + d, light, start(far, near));
            }
            _ => segment(across, thick(arm), -reach(near, far)),
        }
    }
}

/// Block elements: halves, eighths, quadrants and shades.
fn blocks(
    c: u32,
    x0: f64,
    y0: f64,
    w: f64,
    h: f64,
    fill: impl Fn(f64, f64, f64, f64),
    cr: &cairo::Context,
) {
    let (x1, y1) = (x0 + w, y0 + h);
    let (mx, my) = (x0 + (w / 2.0).round(), y0 + (h / 2.0).round());
    let eighth = |n: u32, of: f64| (of * n as f64 / 8.0).round();
    match c {
        0x2580 => fill(x0, y0, x1, my),
        0x2581..=0x2588 => fill(x0, y1 - eighth(c - 0x2580, h), x1, y1),
        0x2589..=0x258f => fill(x0, y0, x0 + eighth(0x2590 - c, w), y1),
        0x2590 => fill(mx, y0, x1, y1),
        0x2591..=0x2593 => {
            // Shades: the color at a quarter, half or three quarters.
            let alpha = (c - 0x2590) as f64 / 4.0;
            let _ = cr.save();
            cr.rectangle(x0, y0, w, h);
            cr.clip();
            let _ = cr.paint_with_alpha(alpha);
            let _ = cr.restore();
        }
        0x2594 => fill(x0, y0, x1, y0 + eighth(1, h)),
        0x2595 => fill(x1 - eighth(1, w), y0, x1, y1),
        _ => {
            // Quadrants: upper left, upper right, lower left, lower right.
            let q = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14][(c - 0x2596) as usize];
            for (bit, (l, t, r, b)) in [
                (1, (x0, y0, mx, my)),
                (2, (mx, y0, x1, my)),
                (4, (x0, my, mx, y1)),
                (8, (mx, my, x1, y1)),
            ] {
                if q & bit != 0 {
                    fill(l, t, r, b);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_follow_the_unicode_names() {
        let arms = |c: char| ARMS[c as usize - 0x2500];
        // Up, right, down, left: 1 light, 2 heavy, 3 double.
        assert_eq!(arms('┼'), 0b01_01_01_01);
        assert_eq!(arms('╋'), 0b10_10_10_10);
        assert_eq!(arms('╬'), 0b11_11_11_11);
        assert_eq!(arms('┟'), 0b00_10_01_01);
        assert_eq!(arms('╒'), 0b00_01_11_00);
        assert!(drawn("│") && drawn("╭") && drawn("█") && drawn("▒"));
        // Dashes keep the font's glyphs.
        assert!(!drawn("┄") && !drawn("╎") && !drawn("a") && !drawn("│\u{301}"));
    }
}
