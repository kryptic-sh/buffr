use std::collections::HashMap;
use std::sync::OnceLock;

use fontdue::{Font as FdFont, FontSettings, Metrics};

const TARGET_PX: f32 = 15.0;

struct TtfFace {
    font: FdFont,
    advance: usize,
    /// Pixel height of an upper-case glyph (rasterized 'M'). Used to
    /// position the baseline so caps are visually centred inside the
    /// `TARGET_PX`-tall cell instead of bottom-aligned.
    cap_height: usize,
    cache: std::sync::Mutex<HashMap<char, (Metrics, Vec<u8>)>>,
}

enum FontFace {
    Ttf(Box<TtfFace>),
    Bitmap,
}

static FACE: OnceLock<FontFace> = OnceLock::new();

fn load_face() -> FontFace {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let names = [
        "Hack",
        "JetBrains Mono",
        "DejaVu Sans Mono",
        "Liberation Mono",
        "monospace",
    ];

    for name in names {
        let family = if name == "monospace" {
            fontdb::Family::Monospace
        } else {
            fontdb::Family::Name(name)
        };
        let query = fontdb::Query {
            families: &[family],
            ..fontdb::Query::default()
        };
        let Some(id) = db.query(&query) else {
            continue;
        };
        let mut result = None;
        db.with_face_data(id, |data, idx| {
            let settings = FontSettings {
                collection_index: idx,
                scale: TARGET_PX,
                ..FontSettings::default()
            };
            if let Ok(font) = FdFont::from_bytes(data, settings) {
                let (metrics, _) = font.rasterize('M', TARGET_PX);
                let advance = metrics.advance_width.round() as usize;
                result = Some(TtfFace {
                    font,
                    advance: advance.max(1),
                    cap_height: metrics.height.max(1),
                    cache: std::sync::Mutex::new(HashMap::new()),
                });
            }
        });
        if let Some(face) = result {
            return FontFace::Ttf(Box::new(face));
        }
    }

    FontFace::Bitmap
}

fn face() -> &'static FontFace {
    FACE.get_or_init(load_face)
}

pub fn glyph_w() -> usize {
    match face() {
        FontFace::Ttf(f) => f.advance,
        FontFace::Bitmap => BITMAP_GLYPH_W,
    }
}

pub fn glyph_h() -> usize {
    match face() {
        FontFace::Ttf(_) => TARGET_PX as usize,
        FontFace::Bitmap => BITMAP_GLYPH_H,
    }
}

pub fn text_width(s: &str) -> usize {
    let n = s.chars().count();
    if n == 0 {
        return 0;
    }
    n * (glyph_w() + 1) - 1
}

pub fn draw_text(buf: &mut [u32], width: usize, height: usize, x: i32, y: i32, s: &str, fg: u32) {
    let advance = (glyph_w() as i32) + 1;
    let mut pen_x = x;
    for c in s.chars() {
        draw_char(buf, width, height, pen_x, y, c, fg);
        pen_x += advance;
    }
}

fn draw_char(buf: &mut [u32], width: usize, height: usize, x: i32, y: i32, c: char, fg: u32) {
    match face() {
        FontFace::Ttf(f) => draw_ttf_char(f, buf, width, height, x, y, c, fg),
        FontFace::Bitmap => draw_bitmap_char(buf, width, height, x, y, bitmap_glyph(c), fg),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ttf_char(
    f: &TtfFace,
    buf: &mut [u32],
    width: usize,
    height: usize,
    x: i32,
    y: i32,
    c: char,
    fg: u32,
) {
    let (metrics, bitmap) = {
        let mut cache = f.cache.lock().unwrap();
        cache
            .entry(c)
            .or_insert_with(|| f.font.rasterize(c, TARGET_PX))
            .clone()
    };

    let fg_r = (fg >> 16) & 0xFF;
    let fg_g = (fg >> 8) & 0xFF;
    let fg_b = fg & 0xFF;

    let baseline_y = y + (TARGET_PX as i32 + f.cap_height as i32) / 2;
    let glyph_x = x + metrics.xmin;
    let glyph_y = baseline_y - metrics.height as i32 - metrics.ymin;

    for row in 0..metrics.height {
        let py = glyph_y + row as i32;
        if py < 0 || py as usize >= height {
            continue;
        }
        for col in 0..metrics.width {
            let coverage = bitmap[row * metrics.width + col];
            if coverage == 0 {
                continue;
            }
            let px = glyph_x + col as i32;
            if px < 0 || px as usize >= width {
                continue;
            }
            let idx = (py as usize) * width + (px as usize);
            let Some(slot) = buf.get_mut(idx) else {
                continue;
            };
            if coverage == 255 {
                *slot = fg;
                continue;
            }
            let bg = *slot;
            let bg_r = (bg >> 16) & 0xFF;
            let bg_g = (bg >> 8) & 0xFF;
            let bg_b = bg & 0xFF;
            let c = coverage as u32;
            let inv = 255 - c;
            let out_r = (fg_r * c + bg_r * inv) / 255;
            let out_g = (fg_g * c + bg_g * inv) / 255;
            let out_b = (fg_b * c + bg_b * inv) / 255;
            *slot = 0xFF_00_00_00 | (out_r << 16) | (out_g << 8) | out_b;
        }
    }
}

fn draw_bitmap_char(
    buf: &mut [u32],
    width: usize,
    height: usize,
    x: i32,
    y: i32,
    g: BitmapGlyph,
    fg: u32,
) {
    for (row_idx, row) in g.iter().enumerate() {
        let py = y + row_idx as i32;
        if py < 0 || py as usize >= height {
            continue;
        }
        for col in 0..BITMAP_GLYPH_W {
            let bit = 1u8 << (BITMAP_GLYPH_W - 1 - col);
            if row & bit == 0 {
                continue;
            }
            let px = x + col as i32;
            if px < 0 || px as usize >= width {
                continue;
            }
            let idx = (py as usize) * width + (px as usize);
            if let Some(slot) = buf.get_mut(idx) {
                *slot = fg;
            }
        }
    }
}

const BITMAP_GLYPH_W: usize = 6;
const BITMAP_GLYPH_H: usize = 10;
type BitmapGlyph = [u8; BITMAP_GLYPH_H];

const BITMAP_MISSING: BitmapGlyph = [
    0b00_0000, 0b01_1110, 0b01_0010, 0b01_0010, 0b01_0010, 0b01_0010, 0b01_0010, 0b01_1110,
    0b00_0000, 0b00_0000,
];

fn bitmap_glyph(c: char) -> BitmapGlyph {
    if (c as u32) > 0x7e {
        return BITMAP_MISSING;
    }
    for &(ch, g) in BITMAP_GLYPHS {
        if ch == c {
            return g;
        }
    }
    BITMAP_MISSING
}

const __: u8 = 0b00_0000;

#[rustfmt::skip]
const BITMAP_GLYPHS: &[(char, BitmapGlyph)] = &[
    (' ',  [__, __, __, __, __, __, __, __, __, __]),
    ('!',  [__, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, __, 0b00_1000, __, __]),
    ('"',  [__, 0b01_0100, 0b01_0100, __, __, __, __, __, __, __]),
    ('#',  [__, 0b01_0100, 0b01_0100, 0b11_1110, 0b01_0100, 0b11_1110, 0b01_0100, 0b01_0100, __, __]),
    ('$',  [__, 0b00_1000, 0b01_1110, 0b10_1000, 0b01_1100, 0b00_1010, 0b11_1100, 0b00_1000, __, __]),
    ('%',  [__, 0b11_0010, 0b10_0100, 0b00_1000, 0b01_0000, 0b10_0110, 0b00_0110, __, __, __]),
    ('&',  [__, 0b01_1000, 0b10_0100, 0b01_1000, 0b10_1010, 0b10_0100, 0b01_1010, __, __, __]),
    ('\'', [__, 0b00_1000, 0b00_1000, __, __, __, __, __, __, __]),
    ('(',  [__, 0b00_0100, 0b00_1000, 0b01_0000, 0b01_0000, 0b01_0000, 0b00_1000, 0b00_0100, __, __]),
    (')',  [__, 0b01_0000, 0b00_1000, 0b00_0100, 0b00_0100, 0b00_0100, 0b00_1000, 0b01_0000, __, __]),
    ('*',  [__, __, 0b10_1010, 0b01_1100, 0b11_1110, 0b01_1100, 0b10_1010, __, __, __]),
    ('+',  [__, __, __, 0b00_1000, 0b00_1000, 0b11_1110, 0b00_1000, 0b00_1000, __, __]),
    (',',  [__, __, __, __, __, __, 0b00_1000, 0b00_1000, 0b01_0000, __]),
    ('-',  [__, __, __, __, __, 0b11_1110, __, __, __, __]),
    ('.',  [__, __, __, __, __, __, __, 0b00_1000, __, __]),
    ('/',  [__, 0b00_0010, 0b00_0100, 0b00_0100, 0b00_1000, 0b01_0000, 0b01_0000, 0b10_0000, __, __]),
    ('0',  [__, 0b01_1100, 0b10_0010, 0b10_0110, 0b10_1010, 0b11_0010, 0b10_0010, 0b01_1100, __, __]),
    ('1',  [__, 0b00_1000, 0b01_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b01_1100, __, __]),
    ('2',  [__, 0b01_1100, 0b10_0010, 0b00_0010, 0b00_0100, 0b00_1000, 0b01_0000, 0b11_1110, __, __]),
    ('3',  [__, 0b01_1100, 0b10_0010, 0b00_0010, 0b00_1100, 0b00_0010, 0b10_0010, 0b01_1100, __, __]),
    ('4',  [__, 0b00_0100, 0b00_1100, 0b01_0100, 0b10_0100, 0b11_1110, 0b00_0100, 0b00_0100, __, __]),
    ('5',  [__, 0b11_1110, 0b10_0000, 0b11_1100, 0b00_0010, 0b00_0010, 0b10_0010, 0b01_1100, __, __]),
    ('6',  [__, 0b01_1100, 0b10_0000, 0b10_0000, 0b11_1100, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('7',  [__, 0b11_1110, 0b00_0010, 0b00_0100, 0b00_1000, 0b01_0000, 0b01_0000, 0b01_0000, __, __]),
    ('8',  [__, 0b01_1100, 0b10_0010, 0b10_0010, 0b01_1100, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('9',  [__, 0b01_1100, 0b10_0010, 0b10_0010, 0b01_1110, 0b00_0010, 0b00_0010, 0b01_1100, __, __]),
    (':',  [__, __, __, 0b00_1000, __, __, 0b00_1000, __, __, __]),
    (';',  [__, __, __, 0b00_1000, __, __, 0b00_1000, 0b00_1000, 0b01_0000, __]),
    ('<',  [__, __, 0b00_0100, 0b00_1000, 0b01_0000, 0b00_1000, 0b00_0100, __, __, __]),
    ('=',  [__, __, __, 0b11_1110, __, 0b11_1110, __, __, __, __]),
    ('>',  [__, __, 0b01_0000, 0b00_1000, 0b00_0100, 0b00_1000, 0b01_0000, __, __, __]),
    ('?',  [__, 0b01_1100, 0b10_0010, 0b00_0010, 0b00_0100, 0b00_1000, __, 0b00_1000, __, __]),
    ('@',  [__, 0b01_1100, 0b10_0010, 0b10_1110, 0b10_1010, 0b10_1110, 0b10_0000, 0b01_1100, __, __]),
    ('A',  [__, 0b01_1100, 0b10_0010, 0b10_0010, 0b11_1110, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('B',  [__, 0b11_1100, 0b10_0010, 0b10_0010, 0b11_1100, 0b10_0010, 0b10_0010, 0b11_1100, __, __]),
    ('C',  [__, 0b01_1100, 0b10_0010, 0b10_0000, 0b10_0000, 0b10_0000, 0b10_0010, 0b01_1100, __, __]),
    ('D',  [__, 0b11_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b11_1100, __, __]),
    ('E',  [__, 0b11_1110, 0b10_0000, 0b10_0000, 0b11_1100, 0b10_0000, 0b10_0000, 0b11_1110, __, __]),
    ('F',  [__, 0b11_1110, 0b10_0000, 0b10_0000, 0b11_1100, 0b10_0000, 0b10_0000, 0b10_0000, __, __]),
    ('G',  [__, 0b01_1100, 0b10_0010, 0b10_0000, 0b10_1110, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('H',  [__, 0b10_0010, 0b10_0010, 0b10_0010, 0b11_1110, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('I',  [__, 0b01_1100, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b01_1100, __, __]),
    ('J',  [__, 0b00_1110, 0b00_0100, 0b00_0100, 0b00_0100, 0b00_0100, 0b10_0100, 0b01_1000, __, __]),
    ('K',  [__, 0b10_0010, 0b10_0100, 0b10_1000, 0b11_0000, 0b10_1000, 0b10_0100, 0b10_0010, __, __]),
    ('L',  [__, 0b10_0000, 0b10_0000, 0b10_0000, 0b10_0000, 0b10_0000, 0b10_0000, 0b11_1110, __, __]),
    ('M',  [__, 0b10_0010, 0b11_0110, 0b10_1010, 0b10_1010, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('N',  [__, 0b10_0010, 0b11_0010, 0b10_1010, 0b10_0110, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('O',  [__, 0b01_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('P',  [__, 0b11_1100, 0b10_0010, 0b10_0010, 0b11_1100, 0b10_0000, 0b10_0000, 0b10_0000, __, __]),
    ('Q',  [__, 0b01_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_1010, 0b10_0100, 0b01_1010, __, __]),
    ('R',  [__, 0b11_1100, 0b10_0010, 0b10_0010, 0b11_1100, 0b10_1000, 0b10_0100, 0b10_0010, __, __]),
    ('S',  [__, 0b01_1110, 0b10_0000, 0b10_0000, 0b01_1100, 0b00_0010, 0b00_0010, 0b11_1100, __, __]),
    ('T',  [__, 0b11_1110, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, __, __]),
    ('U',  [__, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('V',  [__, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_0100, 0b00_1000, __, __]),
    ('W',  [__, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_1010, 0b10_1010, 0b11_0110, 0b10_0010, __, __]),
    ('X',  [__, 0b10_0010, 0b10_0010, 0b01_0100, 0b00_1000, 0b01_0100, 0b10_0010, 0b10_0010, __, __]),
    ('Y',  [__, 0b10_0010, 0b10_0010, 0b01_0100, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, __, __]),
    ('Z',  [__, 0b11_1110, 0b00_0010, 0b00_0100, 0b00_1000, 0b01_0000, 0b10_0000, 0b11_1110, __, __]),
    ('[',  [__, 0b01_1100, 0b01_0000, 0b01_0000, 0b01_0000, 0b01_0000, 0b01_0000, 0b01_1100, __, __]),
    ('\\', [__, 0b10_0000, 0b01_0000, 0b01_0000, 0b00_1000, 0b00_0100, 0b00_0100, 0b00_0010, __, __]),
    (']',  [__, 0b01_1100, 0b00_0100, 0b00_0100, 0b00_0100, 0b00_0100, 0b00_0100, 0b01_1100, __, __]),
    ('^',  [__, 0b00_1000, 0b01_0100, 0b10_0010, __, __, __, __, __, __]),
    ('_',  [__, __, __, __, __, __, __, __, 0b11_1110, __]),
    ('`',  [0b01_0000, 0b00_1000, __, __, __, __, __, __, __, __]),
    ('a',  [__, __, __, 0b01_1100, 0b00_0010, 0b01_1110, 0b10_0010, 0b01_1110, __, __]),
    ('b',  [__, 0b10_0000, 0b10_0000, 0b11_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b11_1100, __, __]),
    ('c',  [__, __, __, 0b01_1100, 0b10_0010, 0b10_0000, 0b10_0010, 0b01_1100, __, __]),
    ('d',  [__, 0b00_0010, 0b00_0010, 0b01_1110, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_1110, __, __]),
    ('e',  [__, __, __, 0b01_1100, 0b10_0010, 0b11_1110, 0b10_0000, 0b01_1100, __, __]),
    ('f',  [__, 0b00_1100, 0b01_0010, 0b01_0000, 0b11_1100, 0b01_0000, 0b01_0000, 0b01_0000, __, __]),
    ('g',  [__, __, __, 0b01_1110, 0b10_0010, 0b01_1110, 0b00_0010, 0b00_0010, 0b01_1100, __]),
    ('h',  [__, 0b10_0000, 0b10_0000, 0b11_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('i',  [__, 0b00_1000, __, 0b01_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b01_1100, __, __]),
    ('j',  [__, 0b00_0100, __, 0b00_1100, 0b00_0100, 0b00_0100, 0b00_0100, 0b00_0100, 0b01_1000, __]),
    ('k',  [__, 0b10_0000, 0b10_0000, 0b10_0100, 0b10_1000, 0b11_0000, 0b10_1000, 0b10_0100, __, __]),
    ('l',  [__, 0b01_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b01_1100, __, __]),
    ('m',  [__, __, __, 0b11_0100, 0b10_1010, 0b10_1010, 0b10_0010, 0b10_0010, __, __]),
    ('n',  [__, __, __, 0b11_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, __, __]),
    ('o',  [__, __, __, 0b01_1100, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_1100, __, __]),
    ('p',  [__, __, __, 0b11_1100, 0b10_0010, 0b10_0010, 0b11_1100, 0b10_0000, 0b10_0000, __]),
    ('q',  [__, __, __, 0b01_1110, 0b10_0010, 0b10_0010, 0b01_1110, 0b00_0010, 0b00_0010, __]),
    ('r',  [__, __, __, 0b10_1100, 0b11_0010, 0b10_0000, 0b10_0000, 0b10_0000, __, __]),
    ('s',  [__, __, __, 0b01_1110, 0b10_0000, 0b01_1100, 0b00_0010, 0b11_1100, __, __]),
    ('t',  [__, 0b01_0000, 0b01_0000, 0b11_1100, 0b01_0000, 0b01_0000, 0b01_0010, 0b00_1100, __, __]),
    ('u',  [__, __, __, 0b10_0010, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_1110, __, __]),
    ('v',  [__, __, __, 0b10_0010, 0b10_0010, 0b10_0010, 0b01_0100, 0b00_1000, __, __]),
    ('w',  [__, __, __, 0b10_0010, 0b10_0010, 0b10_1010, 0b10_1010, 0b01_0100, __, __]),
    ('x',  [__, __, __, 0b10_0010, 0b01_0100, 0b00_1000, 0b01_0100, 0b10_0010, __, __]),
    ('y',  [__, __, __, 0b10_0010, 0b10_0010, 0b01_1110, 0b00_0010, 0b00_0010, 0b01_1100, __]),
    ('z',  [__, __, __, 0b11_1110, 0b00_0100, 0b00_1000, 0b01_0000, 0b11_1110, __, __]),
    ('{',  [__, 0b00_0100, 0b00_1000, 0b00_1000, 0b01_0000, 0b00_1000, 0b00_1000, 0b00_0100, __, __]),
    ('|',  [__, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, 0b00_1000, __, __]),
    ('}',  [__, 0b01_0000, 0b00_1000, 0b00_1000, 0b00_0100, 0b00_1000, 0b00_1000, 0b01_0000, __, __]),
    ('~',  [__, __, __, 0b01_0010, 0b10_1010, 0b10_0100, __, __, __, __]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_w_positive() {
        assert!(glyph_w() > 0);
    }

    #[test]
    fn glyph_h_positive() {
        assert!(glyph_h() > 0);
    }

    #[test]
    fn text_width_zero_for_empty() {
        assert_eq!(text_width(""), 0);
    }

    #[test]
    fn text_width_one_glyph_no_trailing_gap() {
        assert_eq!(text_width("A"), glyph_w());
    }

    #[test]
    fn text_width_n_glyphs_includes_gaps() {
        assert_eq!(text_width("AB"), 2 * glyph_w() + 1);
        assert_eq!(text_width("ABC"), 3 * glyph_w() + 2);
    }

    #[test]
    fn text_width_hi_sane() {
        let w = text_width("hi");
        assert!(w > 0, "text_width(\"hi\") must be positive, got {w}");
    }

    #[test]
    fn draw_text_clips_offscreen() {
        let mut buf = vec![0u32; 20 * 20];
        draw_text(&mut buf, 20, 20, 100, 0, "HI", 0xFF_FFFF);
        draw_text(&mut buf, 20, 20, -50, 0, "HI", 0xFF_FFFF);
        draw_text(&mut buf, 20, 20, 0, 100, "HI", 0xFF_FFFF);
    }

    #[test]
    fn draw_text_writes_non_bg_pixels() {
        let bg = 0u32;
        let fg = 0xEE_EE_EE;
        let w = 200;
        let h = 40;
        let mut buf = vec![bg; w * h];
        draw_text(&mut buf, w, h, 0, 0, "Hi", fg);
        assert!(
            buf.iter().any(|&px| px != bg),
            "draw_text must write at least one non-background pixel"
        );
    }
}
