//! Sixel graphics encoding — real pixel images for terminals without
//! Kitty/iTerm2 support but with sixel (notably Windows Terminal ≥ 1.22,
//! xterm, mlterm, foot).
//!
//! The encoder fits the image into a cell area assuming a conservative
//! cell size of [`CELL_PX_W`]×[`CELL_PX_H`] physical pixels (undersizing
//! keeps the image inside its popup box on larger fonts), quantizes to a
//! fixed 6×7×6 RGB level palette (252 registers — within sixel's 256),
//! and emits one `#color` pass per 6-pixel band with run-length encoding.

use image::imageops::FilterType;

/// Assumed physical pixel size of one terminal cell. Conservative: real
/// cells are usually ≥ this, so the image renders slightly smaller than
/// the reserved cell box rather than overflowing it.
pub const CELL_PX_W: u32 = 8;
pub const CELL_PX_H: u32 = 16;

/// Background the alpha channel is blended against (matches the
/// half-block renderer's dark base).
const ALPHA_BG: (u16, u16, u16) = (18, 18, 18);

/// RGB quantization levels: 6×7×6 = 252 palette registers.
const R_LEVELS: u32 = 6;
const G_LEVELS: u32 = 7;
const B_LEVELS: u32 = 6;

fn quantize_channel(v: u8, levels: u32) -> u32 {
    (v as u32 * (levels - 1) + 127) / 255
}

fn palette_index(r: u8, g: u8, b: u8) -> u32 {
    let qr = quantize_channel(r, R_LEVELS);
    let qg = quantize_channel(g, G_LEVELS);
    let qb = quantize_channel(b, B_LEVELS);
    (qr * G_LEVELS + qg) * B_LEVELS + qb
}

/// Palette register value on sixel's 0–100 scale for a quantized level.
fn level_to_percent(level: u32, levels: u32) -> u32 {
    (level * 100) / (levels - 1)
}

/// Append `count` repetitions of sixel char `ch` with RLE (`!n` form for
/// runs the repeat introducer actually shortens).
fn push_run(out: &mut String, ch: char, count: u32) {
    if count == 0 {
        return;
    }
    if count > 3 {
        out.push('!');
        out.push_str(&count.to_string());
        out.push(ch);
    } else {
        for _ in 0..count {
            out.push(ch);
        }
    }
}

/// Encode image bytes as a sixel sequence scaled to fit `cols`×`rows`
/// terminal cells. Returns `None` when the bytes don't decode or the
/// area is empty. The sequence draws at the current cursor position.
pub fn render_sixel_image(image_data: &[u8], cols: u16, rows: u16) -> Option<String> {
    if cols == 0 || rows == 0 {
        return None;
    }
    let img = image::load_from_memory(image_data).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let max_w = cols as u32 * CELL_PX_W;
    let max_h = rows as u32 * CELL_PX_H;
    let scale = f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64);
    let out_w = ((w as f64 * scale).round() as u32).clamp(1, max_w);
    let out_h = ((h as f64 * scale).round() as u32).clamp(1, max_h);
    let rgba = img
        .resize_exact(out_w, out_h, FilterType::Lanczos3)
        .to_rgba8();

    // Per-pixel palette indices + the set of used registers.
    let mut indices = vec![0u32; (out_w * out_h) as usize];
    let mut used = vec![false; (R_LEVELS * G_LEVELS * B_LEVELS) as usize];
    for (i, px) in rgba.pixels().enumerate() {
        let a = px.0[3] as u16;
        let blend = |c: u8, bg: u16| -> u8 { ((c as u16 * a + bg * (255 - a)) / 255) as u8 };
        let idx = palette_index(
            blend(px.0[0], ALPHA_BG.0),
            blend(px.0[1], ALPHA_BG.1),
            blend(px.0[2], ALPHA_BG.2),
        );
        indices[i] = idx;
        used[idx as usize] = true;
    }

    // DCS q, aspect 1:1, raster attributes announce the pixel size.
    let mut out = String::with_capacity(64 * 1024);
    out.push_str("\x1bP0;1;0q");
    out.push_str(&format!("\"1;1;{out_w};{out_h}"));

    // Palette definitions for used registers only (RGB percent scale).
    for (idx, in_use) in used.iter().enumerate() {
        if !*in_use {
            continue;
        }
        let idx = idx as u32;
        let qb = idx % B_LEVELS;
        let qg = (idx / B_LEVELS) % G_LEVELS;
        let qr = idx / (B_LEVELS * G_LEVELS);
        out.push_str(&format!(
            "#{};2;{};{};{}",
            idx,
            level_to_percent(qr, R_LEVELS),
            level_to_percent(qg, G_LEVELS),
            level_to_percent(qb, B_LEVELS),
        ));
    }

    // Bands of 6 pixel rows; one pass per color used within the band.
    let bands = out_h.div_ceil(6);
    for band in 0..bands {
        let y0 = band * 6;
        let band_rows = (out_h - y0).min(6);
        // Colors present in this band, in first-seen order.
        let mut band_colors: Vec<u32> = Vec::new();
        let mut seen = vec![false; used.len()];
        for dy in 0..band_rows {
            let row = (y0 + dy) * out_w;
            for x in 0..out_w {
                let idx = indices[(row + x) as usize];
                if !seen[idx as usize] {
                    seen[idx as usize] = true;
                    band_colors.push(idx);
                }
            }
        }
        for (ci, color) in band_colors.iter().enumerate() {
            if ci > 0 {
                out.push('$'); // carriage return within the band
            }
            out.push_str(&format!("#{color}"));
            let mut run_char: Option<char> = None;
            let mut run_len: u32 = 0;
            for x in 0..out_w {
                let mut bits: u8 = 0;
                for dy in 0..band_rows {
                    let idx = indices[(((y0 + dy) * out_w) + x) as usize];
                    if idx == *color {
                        bits |= 1 << dy;
                    }
                }
                let ch = (b'?' + bits) as char;
                match run_char {
                    Some(prev) if prev == ch => run_len += 1,
                    Some(prev) => {
                        push_run(&mut out, prev, run_len);
                        run_char = Some(ch);
                        run_len = 1;
                    }
                    None => {
                        run_char = Some(ch);
                        run_len = 1;
                    }
                }
            }
            if let Some(prev) = run_char {
                // Trailing all-empty runs can be dropped entirely.
                if prev != '?' {
                    push_run(&mut out, prev, run_len);
                }
            }
        }
        out.push('-'); // next band
    }
    out.push_str("\x1b\\");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_solid(r: u8, g: u8, b: u8, w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([r, g, b, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn solid_image_produces_valid_sixel_envelope() {
        let png = png_solid(255, 0, 0, 8, 12);
        let six = render_sixel_image(&png, 4, 4).unwrap();
        assert!(six.starts_with("\x1bP0;1;0q"), "DCS header");
        assert!(six.ends_with("\x1b\\"), "ST terminator");
        // Pure red quantizes to full-red register: 100;0;0.
        assert!(six.contains(";2;100;0;0"), "red palette entry: {six}");
        assert!(six.contains('-'), "at least one band");
    }

    #[test]
    fn output_size_announced_fits_cell_budget() {
        let png = png_solid(0, 255, 0, 1000, 1000);
        let six = render_sixel_image(&png, 10, 5).unwrap();
        // 10×5 cells at 8×16 px = 80×80 max → square image fits at 80×80.
        assert!(six.contains("\"1;1;80;80"), "raster attributes: {six}");
    }

    #[test]
    fn rle_compresses_solid_rows() {
        let png = png_solid(0, 0, 255, 640, 6);
        let six = render_sixel_image(&png, 80, 1).unwrap();
        // A solid 6-row band across 80+ px must use the `!n` repeat form.
        assert!(six.contains("!"), "expected RLE repeat introducer: {six}");
        assert!(six.contains('~'), "full 6-bit column char: {six}");
    }

    #[test]
    fn garbage_and_empty_area_return_none() {
        assert!(render_sixel_image(b"nope", 10, 10).is_none());
        let png = png_solid(1, 2, 3, 4, 4);
        assert!(render_sixel_image(&png, 0, 4).is_none());
    }
}
