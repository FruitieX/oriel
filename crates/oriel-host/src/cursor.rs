use std::hash::{DefaultHasher, Hash, Hasher};

use oriel_proto::CursorShape;

/// What the capture backend knows about the cursor right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorPos {
    pub visible: bool,
    pub captured: bool,
    /// Position of the hotspot in surface pixels.
    pub x: i32,
    pub y: i32,
}

pub fn shape_id(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    (width, height, hot_x, hot_y).hash(&mut h);
    rgba.hash(&mut h);
    h.finish()
}

pub fn make_shape(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: Vec<u8>) -> CursorShape {
    CursorShape {
        id: shape_id(width, height, hot_x, hot_y, &rgba),
        width,
        height,
        hotspot_x: hot_x,
        hotspot_y: hot_y,
        rgba,
    }
}

/// A plain arrow, used by the test pattern source.
pub fn arrow() -> CursorShape {
    const ART: [&str; 19] = [
        "X           ",
        "XX          ",
        "X.X         ",
        "X..X        ",
        "X...X       ",
        "X....X      ",
        "X.....X     ",
        "X......X    ",
        "X.......X   ",
        "X........X  ",
        "X.........X ",
        "X......XXXXX",
        "X...X..X    ",
        "X..XX..X    ",
        "X.X  X..X   ",
        "XX   X..X   ",
        "X     X..X  ",
        "      X..X  ",
        "       XX   ",
    ];
    let (w, h) = (12u32, ART.len() as u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in ART {
        for c in row.bytes() {
            rgba.extend_from_slice(match c {
                b'X' => &[0, 0, 0, 255],
                b'.' => &[255, 255, 255, 255],
                _ => &[0, 0, 0, 0],
            });
        }
    }
    make_shape(w, h, 0, 0, rgba)
}

/// Turn "invert the screen" pixels (monochrome and masked cursors) into
/// something visible on any background: black, with a white outline.
/// `invert` marks pixels that should invert; they are already opaque black
/// in `rgba`.
pub fn outline_inverted(width: u32, height: u32, rgba: &mut [u8], invert: &[bool]) {
    let (w, h) = (width as i32, height as i32);
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) as usize;
            if rgba[i * 4 + 3] != 0 {
                continue;
            }
            let near = (-1..=1).any(|dy| {
                (-1..=1).any(|dx| {
                    let (nx, ny) = (x + dx, y + dy);
                    nx >= 0 && ny >= 0 && nx < w && ny < h && invert[(ny * w + nx) as usize]
                })
            });
            if near {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
}
