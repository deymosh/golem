//! The mark: the README's block golem, `▟█▙` over `▜█▛`, with a lit row
//! across it - the spark.
//!
//! One grid is the whole definition. The terminal draws it as quarter-block
//! glyphs, the page and the executable's icon as pixels, and the tray will
//! too, so every surface shows the same cells. The spark is the one part
//! that changes: amber as the mark's own colour, or the engine's state
//! (serving, loading, stopped) where a surface reports one.
//!
//! Pure: pixels and file bytes in memory, written by whoever needs them
//! (`build.rs` for the icon).

/// The cells, top row first: `0` empty, `1` clay, `2` spark. Each cell is one
/// quarter of a terminal character, so the six by four grid is the two
/// lines of three glyphs in [`GLYPHS`].
pub const GRID: [&str; 4] = ["011110", "122221", "111111", "011110"];

/// The mark as the terminal draws it, one line per row of glyphs.
pub const GLYPHS: [&str; 2] = ["\u{259f}\u{2588}\u{2599}", "\u{259c}\u{2588}\u{259b}"];

/// The body's colour: a neutral stone grey, legible on a light and a dark
/// taskbar alike.
pub const CLAY: [u8; 3] = [0x80, 0x86, 0x8f];

/// What the spark shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spark {
    /// The mark's own amber: the icon, the page, the README.
    Lit,
    /// The engine answers.
    Serving,
    /// The engine is starting or a model is loading.
    Loading,
    /// No engine: the spark goes out and reads as body.
    Stopped,
}

impl Spark {
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Spark::Lit | Spark::Loading => [0xe3, 0xa3, 0x3b],
            Spark::Serving => [0x3f, 0xb0, 0x6a],
            Spark::Stopped => [0x4a, 0x56, 0x52],
        }
    }
}

/// The mark as `size` x `size` RGBA pixels, row-major from the top, on a
/// transparent ground.
///
/// Cells are whole pixels - a mark that is 16 pixels wide in a tray has no
/// room for a blurred edge - and a little taller than wide, like the
/// terminal's quarter-blocks. The grid is centred in what is left.
///
/// # Panics
/// Under 8 pixels, where a cell would be less than one pixel wide.
pub fn rgba(size: u32, spark: Spark) -> Vec<u8> {
    assert!(size >= 8, "the mark needs at least 8 pixels, got {size}");
    let cols = GRID[0].len() as u32;
    let rows = GRID.len() as u32;
    let usable = size * 15 / 16;
    let cw = usable / cols;
    let ch = (cw * 8 / 5).min(usable / rows);
    let (ox, oy) = ((size - cw * cols) / 2, (size - ch * rows) / 2);

    let mut px = vec![0u8; (size * size * 4) as usize];
    for (r, row) in GRID.iter().enumerate() {
        for (c, cell) in row.bytes().enumerate() {
            let rgb = match cell {
                b'1' => CLAY,
                b'2' => spark.rgb(),
                _ => continue,
            };
            for y in oy + r as u32 * ch..oy + (r as u32 + 1) * ch {
                for x in ox + c as u32 * cw..ox + (c as u32 + 1) * cw {
                    let i = ((y * size + x) * 4) as usize;
                    px[i..i + 3].copy_from_slice(&rgb);
                    px[i + 3] = 0xff;
                }
            }
        }
    }
    px
}

/// A Windows `.ico` holding the mark at each of `sizes`.
///
/// Each image is an uncompressed 32-bit bitmap, which every reader of the
/// format takes (the 256-pixel one included) and which needs no compressor:
/// the pixels bottom-up in BGRA, then the 1-bit transparency mask the format
/// still requires beside the alpha channel.
pub fn ico(sizes: &[u32], spark: Spark) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bitmap(s, spark)).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // 1 = icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        // 256 does not fit a byte, and the format spells it 0.
        let dim = if size >= 256 { 0 } else { size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(image.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        out.extend_from_slice(&image);
    }
    out
}

/// The mark as an SVG document: one rectangle per filled cell, each cell
/// ten units wide and sixteen tall, the proportions [`rgba`] draws.
///
/// The icon a Linux launcher shows: freedesktop menus take a scalable icon,
/// which needs no encoder and is sharp at every size.
pub fn svg(spark: Spark) -> String {
    let hex = |[r, g, b]: [u8; 3]| format!("#{r:02x}{g:02x}{b:02x}");
    let mut out = String::from(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 60 64\" shape-rendering=\"crispEdges\">",
    );
    for (r, row) in GRID.iter().enumerate() {
        for (c, cell) in row.bytes().enumerate() {
            let fill = match cell {
                b'1' => hex(CLAY),
                b'2' => hex(spark.rgb()),
                _ => continue,
            };
            out.push_str(&format!(
                "<rect x=\"{}\" y=\"{}\" width=\"10\" height=\"16\" fill=\"{fill}\"/>",
                c * 10,
                r * 16
            ));
        }
    }
    out.push_str("</svg>\n");
    out
}

/// One icon image: a BITMAPINFOHEADER, the colour pixels and the mask.
fn bitmap(size: u32, spark: Spark) -> Vec<u8> {
    let px = rgba(size, spark);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes()); // header size
    out.extend_from_slice(&(size as i32).to_le_bytes());
    // The height counts the colour image and the mask together.
    out.extend_from_slice(&(2 * size as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // uncompressed
    out.extend_from_slice(&(size * size * 4 + mask_row * size).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution and palette: unused
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_row as usize];
        for x in 0..size {
            // A set bit is a transparent pixel.
            if px[((y * size + x) * 4 + 3) as usize] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(px: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2], px[i + 3]]
    }

    #[test]
    fn the_svg_is_the_grid_cell_for_cell() {
        let svg = svg(Spark::Lit);
        let filled = GRID.iter().flat_map(|row| row.bytes()).filter(|&b| b != b'0').count();
        assert_eq!(svg.matches("<rect ").count(), filled);
        // The spark row in amber, the body in clay, nothing for an empty cell.
        assert_eq!(svg.matches("fill=\"#e3a33b\"").count(), 4);
        assert!(svg.contains("<rect x=\"10\" y=\"0\" width=\"10\" height=\"16\" fill=\"#80868f\"/>"), "{svg}");
        assert!(!svg.contains("x=\"0\" y=\"0\""), "the top-left cell is empty");
        assert!(svg.starts_with("<svg ") && svg.ends_with("</svg>\n"));
    }

    #[test]
    fn the_glyphs_are_the_grid() {
        // Each glyph is a 2x2 block of cells; a filled quadrant is any
        // non-empty cell. Decoding the glyphs must give back the grid's shape.
        let quadrants = |g: char| -> [bool; 4] {
            match g {
                '\u{259f}' => [false, true, true, true],
                '\u{2588}' => [true, true, true, true],
                '\u{2599}' => [true, false, true, true],
                '\u{259c}' => [true, true, false, true],
                '\u{259b}' => [true, true, true, false],
                other => panic!("unexpected glyph {other:?}"),
            }
        };
        for (line, glyphs) in GLYPHS.iter().enumerate() {
            for (col, g) in glyphs.chars().enumerate() {
                let q = quadrants(g);
                for (k, filled) in q.iter().enumerate() {
                    let (r, c) = (line * 2 + k / 2, col * 2 + k % 2);
                    let cell = GRID[r].as_bytes()[c];
                    assert_eq!(*filled, cell != b'0', "glyph {g:?} cell ({r},{c})");
                }
            }
        }
    }

    #[test]
    fn a_tray_sized_mark_has_whole_pixel_cells_and_a_lit_row() {
        let px = rgba(16, Spark::Serving);
        assert_eq!(px.len(), 16 * 16 * 4);
        // 16 px: cells of 2 x 3, a 12 x 12 grid with a 2 px margin.
        assert_eq!(pixel(&px, 16, 0, 0)[3], 0, "the corner is transparent");
        assert_eq!(pixel(&px, 16, 2, 2)[3], 0, "the grid's empty top-left cell");
        assert_eq!(&pixel(&px, 16, 4, 2)[..3], &CLAY, "the top row is clay");
        assert_eq!(&pixel(&px, 16, 4, 5)[..3], &Spark::Serving.rgb(), "the second row is the spark");
        assert_eq!(&pixel(&px, 16, 2, 5)[..3], &CLAY, "and the spark row's ends are clay");
    }

    #[test]
    fn every_icon_size_fits_its_square() {
        for size in [16, 20, 24, 32, 48, 64, 256] {
            let px = rgba(size, Spark::Lit);
            let lit = px.chunks(4).filter(|p| p[3] == 0xff).count();
            assert!(lit > 0, "{size}: something is drawn");
        }
    }

    #[test]
    fn the_ico_directory_points_at_each_image() {
        let sizes = [16, 32, 256];
        let ico = ico(&sizes, Spark::Lit);
        let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]);
        assert_eq!((u16_at(0), u16_at(2), u16_at(4)), (0, 1, 3));
        let mut end = 6 + 16 * 3;
        for (k, &size) in sizes.iter().enumerate() {
            let entry = 6 + 16 * k;
            assert_eq!(ico[entry], if size == 256 { 0 } else { size as u8 });
            let (len, offset) = (u32_at(entry + 8) as usize, u32_at(entry + 12) as usize);
            assert_eq!(offset, end, "images follow each other");
            // 40-byte header, BGRA pixels, a mask row of whole 4-byte words.
            let mask_row = (size as usize).div_ceil(32) * 4;
            assert_eq!(len, 40 + size as usize * size as usize * 4 + mask_row * size as usize);
            assert_eq!(u32_at(offset), 40);
            assert_eq!(u32_at(offset + 8), 2 * size, "the height counts the mask");
            end = offset + len;
        }
        assert_eq!(end, ico.len());
    }
}
