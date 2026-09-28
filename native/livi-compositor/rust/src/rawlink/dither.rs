//! the rgb565 quantiser every rawlink tier shares. calibration, then an ordered
//! 4x4 bayer dither anchored in screen space, never temporal, so a static pixel
//! always packs to the same value and rawlink's tile compare stays exact.

use crate::state::CalState;
use crate::tuning::BAYER4;

/// top value per channel, r g b.
const LEVELS: [u32; 3] = [31, 63, 31];
const CELLS: f32 = 16.0;

/// the dither for the gles tiers. `dither565` takes a calibrated colour and the
/// fragment coordinate and returns it snapped to 565 levels, so tier 1's own
/// conversion and tier 2's cpu pack both land on exactly those levels.
pub fn glsl() -> String {
    // mat4 is column major, so column x holds rows 0..3 of that x
    let cols: Vec<String> = (0..4)
        .map(|x| {
            let c: Vec<String> = (0..4).map(|y| format!("{:.1}", BAYER4[y][x])).collect();
            c.join(", ")
        })
        .collect();
    format!(
        r#"
const mat4 BAYER4 = mat4({cols});
const vec3 LEVELS565 = vec3({lr:.1}, {lg:.1}, {lb:.1});
vec3 dither565(vec3 c, vec2 frag) {{
    vec2 m = mod(floor(frag), 4.0);
    vec4 ex = vec4(equal(vec4(m.x), vec4(0.0, 1.0, 2.0, 3.0)));
    vec4 ey = vec4(equal(vec4(m.y), vec4(0.0, 1.0, 2.0, 3.0)));
    float t = (dot(ey, BAYER4 * ex) + 0.5) / {CELLS:.1};
    return min(floor(c * LEVELS565 + t), LEVELS565) / LEVELS565;
}}
"#,
        cols = cols.join(", "),
        lr = LEVELS[0],
        lg = LEVELS[1],
        lb = LEVELS[2],
    )
}

/// the calibration formula of the shader, per channel.
fn calibrate(cal: &CalState, c: f32, ch: usize) -> f32 {
    let c = c.powf(1.0 / cal.gamma);
    let c = (c - 0.5) * cal.contrast + 0.5;
    (c * cal.gain[ch]).clamp(0.0, 1.0)
}

fn quantise(c: f32, levels: u32, cell: u8) -> u16 {
    let t = (cell as f32 + 0.5) / CELLS;
    ((c * levels as f32 + t).floor() as u32).min(levels) as u16
}

/// calibration, dither and the 565 shift folded into one table per channel,
/// bayer cell and 8-bit input, so the pixman pass is three lookups a pixel.
pub struct Lut {
    key: [f32; 5],
    // [channel][cell][value], already shifted into place
    table: Vec<u16>,
}

impl Lut {
    fn key(cal: &CalState) -> [f32; 5] {
        [cal.gamma, cal.contrast, cal.gain[0], cal.gain[1], cal.gain[2]]
    }

    pub fn new(cal: &CalState) -> Self {
        const SHIFT: [u16; 3] = [11, 5, 0];
        let mut table = vec![0u16; 3 * 16 * 256];
        for ch in 0..3 {
            for cell in 0..16u8 {
                for v in 0..256 {
                    let c = calibrate(cal, v as f32 / 255.0, ch);
                    table[(ch * 16 + cell as usize) * 256 + v] = quantise(c, LEVELS[ch], cell) << SHIFT[ch];
                }
            }
        }
        Self { key: Self::key(cal), table }
    }

    pub fn matches(&self, cal: &CalState) -> bool {
        self.key == Self::key(cal)
    }

    #[inline]
    fn get(&self, ch: usize, cell: u8, v: u32) -> u16 {
        self.table[(ch * 16 + cell as usize) * 256 + v as usize]
    }
}

/// a rect in screen pixels.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

fn put(dst: &mut [u8], off: usize, px: u16) {
    dst[off..off + 2].copy_from_slice(&px.to_le_bytes());
}

/// pixman tier. `src` is the x8r8g8b8 composite of the whole screen.
pub fn pack_xrgb(lut: &Lut, src: &[u32], src_stride: usize, dst: &mut [u8], dst_stride: usize, r: Rect) {
    for y in r.y..r.y + r.h {
        let row = &BAYER4[y & 3];
        for x in r.x..r.x + r.w {
            let p = src[y * src_stride + x];
            let cell = row[x & 3];
            let px = lut.get(0, cell, (p >> 16) & 0xff) | lut.get(1, cell, (p >> 8) & 0xff) | lut.get(2, cell, p & 0xff);
            put(dst, y * dst_stride + x * 2, px);
        }
    }
}

/// gles8888 tier. `src` is the rect read back as tight rgba rows, already on
/// 565 levels, so packing is truncation.
pub fn pack_rgba(src: &[u8], dst: &mut [u8], dst_stride: usize, r: Rect) {
    for row in 0..r.h {
        for col in 0..r.w {
            let s = &src[(row * r.w + col) * 4..];
            let px = ((s[0] as u16 >> 3) << 11) | ((s[1] as u16 >> 2) << 5) | (s[2] as u16 >> 3);
            put(dst, (r.y + row) * dst_stride + (r.x + col) * 2, px);
        }
    }
}

/// gles565 tier. `src` is the rect read back as tight 565 rows.
pub fn copy_565(src: &[u8], dst: &mut [u8], dst_stride: usize, r: Rect) {
    let len = r.w * 2;
    for row in 0..r.h {
        let off = (r.y + row) * dst_stride + r.x * 2;
        dst[off..off + len].copy_from_slice(&src[row * len..(row + 1) * len]);
    }
}
