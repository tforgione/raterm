//! Sixel images: the data of `DCS P1 ; P2 ; P3 q … ST`, after `q`.

use super::MAX_IMAGE;
use crate::rich::numbers;

pub const REGISTERS: usize = 1024;

/// The VT340's 16 first registers, in percents of RGB; the others are black.
const VT340: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (20, 20, 80),
    (80, 13, 13),
    (20, 80, 20),
    (80, 20, 80),
    (20, 80, 80),
    (80, 80, 20),
    (53, 53, 53),
    (26, 26, 26),
    (33, 33, 60),
    (60, 26, 26),
    (33, 60, 33),
    (60, 33, 60),
    (33, 60, 60),
    (60, 60, 33),
    (80, 80, 80),
];

/// Decodes `data` into premultiplied ARGB pixels, clipped to `max` (width,
/// height) and to `MAX_IMAGE`: its width, height and pixels, or `None` when
/// it draws nothing. Unset pixels are transparent.
pub fn decode(data: &[u8], max: (usize, usize)) -> Option<(usize, usize, Vec<u32>)> {
    let max_w = max.0.max(1);
    let max_h = max.1.min(MAX_IMAGE / 4 / max_w);
    let percent = |v: usize| (v.min(100) * 255 + 50) / 100;
    let mut palette = vec![0xff00_0000u32; REGISTERS];
    for (reg, &(r, g, b)) in palette.iter_mut().zip(&VT340) {
        *reg = rgb(
            percent(r as usize),
            percent(g as usize),
            percent(b as usize),
        );
    }
    // Rows of `max_w` pixels, cropped at the end.
    let mut pixels: Vec<u32> = Vec::new();
    let (mut width, mut height) = (0, 0);
    let (mut x, mut y, mut color) = (0, 0, 0);
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        i += 1;
        let mut params = || {
            let start = i;
            while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                i += 1;
            }
            numbers(&data[start..i])
        };
        let mut repeat = 1;
        let bits = match b {
            b'"' => {
                if let [_, _, w, h] = params()[..] {
                    width = width.max(w.min(max_w));
                    height = height.max(h.min(max_h));
                }
                continue;
            }
            b'#' => {
                let p = params();
                color = p[0] % REGISTERS;
                match p[..] {
                    [_, 1, h, l, s] => palette[color] = hls(h, l, s),
                    [_, 2, r, g, b] => palette[color] = rgb(percent(r), percent(g), percent(b)),
                    _ => {}
                }
                continue;
            }
            b'!' => {
                repeat = params()[0].max(1);
                match data.get(i) {
                    Some(&b @ 0x3f..=0x7e) => {
                        i += 1;
                        b - 0x3f
                    }
                    _ => continue,
                }
            }
            b'$' => {
                x = 0;
                continue;
            }
            b'-' => {
                x = 0;
                y += 6;
                continue;
            }
            0x3f..=0x7e => b - 0x3f,
            _ => continue,
        };
        // A huge repeat only moves on: past `max_w`, nothing is drawn.
        let drawn = repeat.min(max_w.saturating_sub(x));
        for bit in (0..6).filter(|bit| drawn > 0 && bits & (1 << bit) != 0) {
            let row = y + bit;
            if row >= max_h {
                break;
            }
            if pixels.len() < (row + 1) * max_w {
                pixels.resize((row + 1) * max_w, 0);
            }
            pixels[row * max_w + x..][..drawn].fill(palette[color]);
            height = height.max(row + 1);
        }
        x = x.saturating_add(repeat);
        width = width.max(x.min(max_w));
    }
    if width == 0 || height == 0 {
        return None;
    }
    pixels.resize(height * max_w, 0);
    let cropped = pixels
        .chunks(max_w)
        .flat_map(|row| &row[..width])
        .copied()
        .collect();
    Some((width, height, cropped))
}

fn rgb(r: usize, g: usize, b: usize) -> u32 {
    0xff00_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// DEC's HLS: hue in degrees from blue (blue 0, red 120, green 240),
/// lightness and saturation in percents.
fn hls(h: usize, l: usize, s: usize) -> u32 {
    let h = ((h + 240) % 360) as f64 / 60.0;
    let (l, s) = (l.min(100) as f64 / 100.0, s.min(100) as f64 / 100.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let v = |v: f64| ((v + m) * 255.0).round() as usize;
    rgb(v(r), v(g), v(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: u32 = 0xffff_0000;

    #[test]
    fn decodes_bands_colors_and_repeats() {
        // A red register, 3 columns of a full band, then a band where only
        // the top pixel of the second column is set.
        let (w, h, px) = decode(b"#1;2;100;0;0#1!3~-?@", (100, 100)).unwrap();
        assert_eq!((w, h), (3, 7));
        assert!(px[..18].iter().all(|&p| p == RED));
        assert_eq!(&px[18..21], &[0, RED, 0]);
    }

    #[test]
    fn raster_attributes_set_the_size_and_unset_pixels_stay_clear() {
        let (w, h, px) = decode(b"\"1;1;4;8#0@", (100, 100)).unwrap();
        assert_eq!((w, h), (4, 8));
        assert_eq!(px[0], 0xff00_0000);
        assert!(px[1..].iter().all(|&p| p == 0));
    }

    #[test]
    fn carriage_return_overlays_the_band() {
        let (w, h, px) = decode(b"#1;2;100;0;0@$#2;2;0;100;0A", (100, 100)).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(px, [RED, 0xff00_ff00]);
    }

    #[test]
    fn huge_sizes_are_clipped() {
        let (w, h, _) = decode(b"\"1;1;99999;99999!99999999~", (64, 32)).unwrap();
        assert_eq!((w, h), (64, 32));
        assert_eq!(decode(b"#1;2;1;2;3", (10, 10)), None);
    }

    #[test]
    fn hls_hues_start_from_blue() {
        assert_eq!(hls(0, 50, 100), 0xff00_00ff);
        assert_eq!(hls(120, 50, 100), RED);
        assert_eq!(hls(240, 50, 100), 0xff00_ff00);
    }
}
