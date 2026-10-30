//! Constant reference grid overlay: the pixel-side grounding.
//!
//! A fixed 64x36 grid (columns at 1/64 of the width, rows at 1/36 of the
//! height — 25x21 px cells on a 1600x762 frame) drawn on EVERY screenshot
//! the brain sees — there is no raw path. Thin lines plus axis numbers
//! only: column numbers 1..=64 along the top edge, row numbers 1..=36
//! down the left edge. Tabs in all 2304 cell interiors bury the page
//! (price labels, toolbar, chart); numbers on the two axes do not, and
//! they make every cell addressable: the brain picks a cell
//! (`g<col>,<row>`, resolved via `cell_bounds`) and can aim inside it
//! with a sub-cell fraction (`g<col>,<row>@fx,fy`) instead of
//! wild-guessing coordinates. (64x36 replaces 32x18: cell-centre clicks
//! on 50x42 px cells missed 15-25 px controls by up to a whole cell.)
//!
//! The grid is constant: same fractions every frame, every viewport.
//! `cell_bounds` pins that geometry.

use rose_core::Rect;

/// Grid columns (x). Lines sit at multiples of 1/64 of the frame width.
pub const GRID_COLS: u32 = 64;
/// Grid rows (y). Lines sit at multiples of 1/36 of the frame height.
pub const GRID_ROWS: u32 = 36;

/// Bounds of cell `(col, row)` (0-based) in a `width`x`height` frame.
/// Cells tile the frame exactly: the last column/row absorb any remainder
/// pixels. Pure geometry — the contract the brain relies on when it reads
/// coordinates off the lines.
pub fn cell_bounds(col: u32, row: u32, width: u32, height: u32) -> Option<Rect> {
    if col >= GRID_COLS || row >= GRID_ROWS || width == 0 || height == 0 {
        return None;
    }
    let cw = width / GRID_COLS;
    let ch = height / GRID_ROWS;
    let x = col * cw;
    let y = row * ch;
    let w = if col + 1 == GRID_COLS {
        width.saturating_sub(x)
    } else {
        cw
    };
    let h = if row + 1 == GRID_ROWS {
        height.saturating_sub(y)
    } else {
        ch
    };
    if w < 2 || h < 2 {
        return None;
    }
    Some(Rect {
        x,
        y,
        width: w,
        height: h,
    })
}

/// 5x7 bitmap digits (rows top-to-bottom, 5 bits each) drawn at 1x —
/// small single-pixel strokes, not the old scaled-up 3x5 blocks. The
/// only glyphs the axis labels need; hand-rolled, no font deps.
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ], // 0
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ], // 1
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ], // 2
    [
        0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
    ], // 3
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ], // 4
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ], // 5
    [
        0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ], // 6
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ], // 7
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ], // 8
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
    ], // 9
];
/// Each glyph is 5x7 px, advance 7 px.
const DIGIT_W: u32 = 5;
const DIGIT_H: u32 = 7;
const DIGIT_ADV: u32 = 7;

/// Draw `n` at (`x`,`y`) over a black tab so the number reads on light
/// and dark pages alike.
fn draw_label(img: &mut image::RgbImage, x: u32, y: u32, n: u32) {
    use image::Rgb;
    let (w, h) = (img.width(), img.height());
    let lw = (if n >= 10 { 2 } else { 1 }) * DIGIT_ADV - (DIGIT_ADV - DIGIT_W);
    for ty in y.saturating_sub(1)..(y + DIGIT_H + 1).min(h) {
        for tx in x.saturating_sub(1)..(x + lw + 1).min(w) {
            img.put_pixel(tx, ty, Rgb([0, 0, 0]));
        }
    }
    let mut cx = x;
    for ch in n.to_string().chars() {
        let glyph = DIGITS[(ch as u8 - b'0') as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (0b10000 >> col) != 0 {
                    let (px, py) = (cx + col, y + row as u32);
                    if px < w && py < h {
                        img.put_pixel(px, py, Rgb([255, 255, 255]));
                    }
                }
            }
        }
        cx += DIGIT_ADV;
    }
}

/// Parse a brain grid-cell pick (`g<col>,<row>`, `g<col>x<row>`, or a
/// bare `<col>,<row>`) into 1-based column/row. Anything else — DOM/
/// sidecar ids like `7`/`e-7`, out-of-range cells — is `None`.
pub fn grid_cell_of(raw: &str) -> Option<(u32, u32)> {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix('g')
        .or_else(|| trimmed.strip_prefix('G'))
        .unwrap_or(trimmed);
    let nums: Vec<u32> = body
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u32>())
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if nums.len() != 2 {
        return None;
    }
    let (col, row) = (nums[0], nums[1]);
    if (1..=GRID_COLS).contains(&col) && (1..=GRID_ROWS).contains(&row) {
        Some((col, row))
    } else {
        None
    }
}

/// Resolve a grid-cell pick to a pixel in a `width`x`height` frame (the
/// Stepper's click point). Plain `g<col>,<row>` clicks the cell centre;
/// `g<col>,<row>@fx,fy` (fractions 0..=1 across/down the cell) aims
/// inside the cell — a 25x21 px cell still swallows a toolbar icon
/// whole, and the fraction is what pins it. A malformed fraction is
/// `None` (replan), never a silent centre click.
pub fn resolve_grid_cell(raw: &str, width: u32, height: u32) -> Option<(u32, u32)> {
    let (head, frac) = match raw.trim().split_once('@') {
        Some((head, frac)) => (head, Some(frac)),
        None => (raw.trim(), None),
    };
    let (col, row) = grid_cell_of(head)?;
    let cell = cell_bounds(col - 1, row - 1, width, height)?;
    let (fx, fy) = match frac {
        None => (0.5, 0.5),
        Some(frac) => {
            let mut parts = frac.split(',');
            let fx: f32 = parts.next()?.trim().parse().ok()?;
            let fy: f32 = parts.next()?.trim().parse().ok()?;
            if parts.next().is_some() || !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
                return None;
            }
            (fx, fy)
        }
    };
    let x = cell.x + (fx * cell.width.saturating_sub(1) as f32).round() as u32;
    let y = cell.y + (fy * cell.height.saturating_sub(1) as f32).round() as u32;
    Some((x, y))
}

/// Draw the constant grid onto a copy of `frame_jpeg`: thin lines plus
/// axis numbers (columns 1..=64 across the top, rows 1..=36 down the
/// left). Cell interiors stay clean. Returns JPEG quality 85, same as
/// captures.
pub fn annotate_grid_jpeg(frame_jpeg: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use image::{codecs::jpeg::JpegEncoder, ImageFormat, Rgb, RgbImage};

    let mut img: RgbImage = image::load_from_memory_with_format(frame_jpeg, ImageFormat::Jpeg)
        .map_err(|e| format!("grid annotate decode: {e}"))?
        .to_rgb8();
    let (w, h) = (img.width(), img.height());
    if w < GRID_COLS || h < GRID_ROWS {
        return Err("grid annotate: frame smaller than the grid".to_string());
    }
    // Bright enough to survive JPEG compression and dark chart
    // backgrounds, thin enough to stay a reference, not a mask.
    let line = Rgb([220, 60, 60]);

    let put = |img: &mut RgbImage, x: u32, y: u32| {
        if x < w && y < h {
            img.put_pixel(x, y, line);
        }
    };
    // Vertical lines at every column edge (last edge is the frame edge).
    for c in 0..=GRID_COLS {
        let x = (c * w / GRID_COLS).min(w.saturating_sub(1));
        for y in 0..h {
            put(&mut img, x, y);
        }
    }
    // Horizontal lines at every row edge.
    for r in 0..=GRID_ROWS {
        let y = (r * h / GRID_ROWS).min(h.saturating_sub(1));
        for x in 0..w {
            put(&mut img, x, y);
        }
    }
    // Axis numbers: columns along the top of row 1, rows down the left
    // of column 1 (row 1's label sits under the column band). A cell is
    // then nameable as g<col>,<row> and pixels are readable off counts.
    for c in 0..GRID_COLS {
        let cell = cell_bounds(c, 0, w, h).expect("grid cell");
        draw_label(&mut img, cell.x + 2, 2, c + 1);
    }
    for r in 0..GRID_ROWS {
        let cell = cell_bounds(0, r, w, h).expect("grid cell");
        let y = if r == 0 {
            (DIGIT_H + 5).min(cell.y + cell.height.saturating_sub(DIGIT_H + 1))
        } else {
            cell.y + 2
        };
        draw_label(&mut img, 2, y, r + 1);
    }

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85)
        .encode_image(&img)
        .map_err(|e| format!("grid annotate encode: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_geometry_is_constant() {
        // 25px cells at server geometry, tiling exactly.
        assert_eq!(
            cell_bounds(0, 0, 1600, 900),
            Some(Rect {
                x: 0,
                y: 0,
                width: 25,
                height: 25
            })
        );
        assert_eq!(
            cell_bounds(63, 35, 1600, 900),
            Some(Rect {
                x: 1575,
                y: 875,
                width: 25,
                height: 25
            })
        );
        // Out of range.
        assert_eq!(cell_bounds(64, 0, 1600, 900), None);
        assert_eq!(cell_bounds(0, 36, 1600, 900), None);
        assert_eq!(cell_bounds(0, 0, 0, 900), None);
    }

    #[test]
    fn grid_scales_but_fractions_hold() {
        // CDP viewport: cells shrink, tiling stays exact, no gaps.
        let last = cell_bounds(63, 35, 1600, 762).expect("cell");
        assert_eq!(last.x + last.width, 1600);
        assert_eq!(last.y + last.height, 762);
        // Same fractions regardless of size: col 11 starts at 11/64 width.
        let c = cell_bounds(11, 4, 1600, 762).expect("cell");
        assert_eq!(c.x, 11 * (1600 / 64));
        assert_eq!(c.y, 4 * (762 / 36));
    }

    #[test]
    fn annotate_keeps_size_and_draws_lines_with_axis_numbers() {
        use image::codecs::jpeg::JpegEncoder;
        let img = image::RgbImage::from_pixel(800, 450, image::Rgb([40, 40, 40]));
        let mut raw = Vec::new();
        JpegEncoder::new_with_quality(&mut raw, 85)
            .encode_image(&img)
            .expect("encode");
        let out = annotate_grid_jpeg(&raw).expect("annotate");
        assert!(out.len() > 4 && out[0] == 0xff && out[1] == 0xd8);
        let back: image::RgbImage =
            image::load_from_memory_with_format(&out, image::ImageFormat::Jpeg)
                .expect("decode")
                .to_rgb8();
        assert_eq!((back.width(), back.height()), (800, 450));
        // Grid line present at first column edge (x = 800/64 = 12).
        let lined = (0..450).any(|y| {
            let p = back.get_pixel(12, y);
            p[0] > 100 && p[1] < 80 && p[2] < 80
        });
        assert!(lined, "grid line drawn");
        // And no badge tabs in cell interiors: nothing near-white away
        // from the lines and axes (this window sits inside cells).
        let tabbed = (127..148).any(|x| {
            (113..134).any(|y| {
                let p = back.get_pixel(x, y);
                p[0] > 200 && p[1] > 200 && p[2] > 200
            })
        });
        assert!(!tabbed, "numbers on the axes only, never in cell interiors");
        // Axis numbers ARE drawn small (5x7 digits): near-white pixels
        // inside the top band (first row, y 2..9) and the left band.
        let top_numbered = (0..800).any(|x| {
            (2..9).any(|y| {
                let p = back.get_pixel(x, y);
                p[0] > 200 && p[1] > 200 && p[2] > 200
            })
        });
        assert!(top_numbered, "column numbers along the top");
        let left_numbered = (2..16).any(|x| {
            (0..450).any(|y| {
                let p = back.get_pixel(x, y);
                p[0] > 200 && p[1] > 200 && p[2] > 200
            })
        });
        assert!(left_numbered, "row numbers down the left");
    }

    #[test]
    fn grid_cell_names_parse() {
        assert_eq!(grid_cell_of("g14,3"), Some((14, 3)));
        assert_eq!(grid_cell_of("g1x18"), Some((1, 18)));
        assert_eq!(grid_cell_of("14,3"), Some((14, 3)));
        assert_eq!(grid_cell_of("g64,36"), Some((64, 36)));
        assert_eq!(grid_cell_of("g65,3"), None);
        assert_eq!(grid_cell_of("g14,37"), None);
        assert_eq!(grid_cell_of("g0,3"), None);
        assert_eq!(grid_cell_of("7"), None);
        assert_eq!(grid_cell_of("e-7"), None);
        assert_eq!(grid_cell_of("nope"), None);
    }

    #[test]
    fn grid_cells_resolve_centre_and_subcell() {
        // 1600x762 frame: cells 25x21 (last row taller).
        // Centre of cell (1,1): (12, 10).
        assert_eq!(resolve_grid_cell("g1,1", 1600, 762), Some((12, 10)));
        // Sub-cell aim: cell (3,7) spans x 50..75, y 126..147;
        // @0,1 is its bottom-left inner corner.
        assert_eq!(resolve_grid_cell("g3,7@0,1", 1600, 762), Some((50, 146)));
        assert_eq!(
            resolve_grid_cell("g3,7@0.5,0.5", 1600, 762),
            Some((62, 136))
        );
        // Malformed fractions replan, never silently centre-click.
        assert_eq!(resolve_grid_cell("g3,7@2,0", 1600, 762), None);
        assert_eq!(resolve_grid_cell("g3,7@0.5", 1600, 762), None);
        assert_eq!(resolve_grid_cell("g3,7@a,b", 1600, 762), None);
        assert_eq!(resolve_grid_cell("g65,7", 1600, 762), None);
    }
}
