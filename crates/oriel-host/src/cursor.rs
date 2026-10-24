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

/// `shape` resized by `factor` (bilinear, on premultiplied colour), with
/// its hotspot moved along.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn scale_shape(shape: &CursorShape, factor: f32) -> CursorShape {
    let (sw, sh) = (shape.width as usize, shape.height as usize);
    let w = ((sw as f32 * factor).round() as usize).max(1);
    let h = ((sh as f32 * factor).round() as usize).max(1);
    let px = |x: usize, y: usize| {
        let p = &shape.rgba[(y * sw + x) * 4..][..4];
        let a = p[3] as f32 / 255.0;
        [
            p[0] as f32 * a,
            p[1] as f32 * a,
            p[2] as f32 * a,
            p[3] as f32,
        ]
    };
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        // Sample centres, in source pixels.
        let fy = ((y as f32 + 0.5) / factor - 0.5).clamp(0.0, (sh - 1) as f32);
        let (y0, ty) = (fy.floor() as usize, fy.fract());
        let y1 = (y0 + 1).min(sh - 1);
        for x in 0..w {
            let fx = ((x as f32 + 0.5) / factor - 0.5).clamp(0.0, (sw - 1) as f32);
            let (x0, tx) = (fx.floor() as usize, fx.fract());
            let x1 = (x0 + 1).min(sw - 1);
            let mut c = [0f32; 4];
            for (p, wgt) in [
                (px(x0, y0), (1.0 - tx) * (1.0 - ty)),
                (px(x1, y0), tx * (1.0 - ty)),
                (px(x0, y1), (1.0 - tx) * ty),
                (px(x1, y1), tx * ty),
            ] {
                for i in 0..4 {
                    c[i] += p[i] * wgt;
                }
            }
            let a = c[3] / 255.0;
            let un = |v: f32| {
                if a > 0.0 {
                    (v / a).round().clamp(0.0, 255.0) as u8
                } else {
                    0
                }
            };
            rgba.extend_from_slice(&[un(c[0]), un(c[1]), un(c[2]), c[3].round() as u8]);
        }
    }
    let hot = |v: u32, max: usize| ((v as f32 * factor).round() as u32).min(max as u32 - 1);
    make_shape(
        w as u32,
        h as u32,
        hot(shape.hotspot_x, w),
        hot(shape.hotspot_y, h),
        rgba,
    )
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
#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_keeps_the_shape_and_moves_the_hotspot() {
        let arrow = arrow();
        let big = scale_shape(&arrow, 1.25);
        assert_eq!((big.width, big.height), (15, 24));
        assert_eq!((big.hotspot_x, big.hotspot_y), (0, 0));
        // The arrow's tip stays opaque black, the far corner transparent.
        assert_eq!(&big.rgba[..4], &[0, 0, 0, 255]);
        assert_eq!(big.rgba[(15 * 24 - 1) * 4 + 3], 0);
        let same = scale_shape(&arrow, 1.0);
        assert_eq!(same.rgba, arrow.rgba);
    }
}
