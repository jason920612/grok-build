//! Half-block text rendering of images — the graphics fallback for
//! terminals without Kitty/iTerm2 support (notably Windows ConPTY).
//!
//! Each terminal cell shows two vertically stacked pixels using the upper
//! half block `▀`: the glyph's foreground colour is the top pixel, the
//! cell background is the bottom pixel. With truecolor this yields a
//! usable preview at cell resolution (cols × rows×2 pixels) out of plain
//! text — no terminal graphics protocol required.

use image::imageops::FilterType;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Background the image's alpha channel is blended against. Dark, close
/// to typical terminal themes, so transparent PNGs don't glow.
const ALPHA_BLEND_BG: (u16, u16, u16) = (18, 18, 18);

fn blend(px: image::Rgba<u8>) -> Color {
    let a = px.0[3] as u16;
    let mix = |c: u8, bg: u16| -> u8 { ((c as u16 * a + bg * (255 - a)) / 255) as u8 };
    Color::Rgb(
        mix(px.0[0], ALPHA_BLEND_BG.0),
        mix(px.0[1], ALPHA_BLEND_BG.1),
        mix(px.0[2], ALPHA_BLEND_BG.2),
    )
}

/// Render encoded image bytes into ratatui lines of `▀` cells fitting
/// within `max_cols` × `max_rows` terminal cells, preserving aspect ratio
/// (one cell is two vertical pixels). Returns `None` when the image cannot
/// be decoded or the area is empty.
///
/// Cost: one decode + resize per call — callers cache per (cols, rows),
/// e.g. [`crate::prompt_images::ImageViewerState::halfblock_lines`].
pub fn render_halfblock_lines(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
) -> Option<Vec<Line<'static>>> {
    if max_cols == 0 || max_rows == 0 {
        return None;
    }
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let max_px_w = max_cols as u32;
    let max_px_h = (max_rows as u32) * 2;
    let scale = f64::min(max_px_w as f64 / w as f64, max_px_h as f64 / h as f64);
    let out_w = ((w as f64 * scale).round() as u32).clamp(1, max_px_w);
    let out_h = ((h as f64 * scale).round() as u32).clamp(1, max_px_h);
    let resized = img.resize_exact(out_w, out_h, FilterType::Triangle).to_rgba8();

    let rows = out_h.div_ceil(2);
    let mut lines = Vec::with_capacity(rows as usize);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(out_w as usize);
        for x in 0..out_w {
            let top = blend(*resized.get_pixel(x, row * 2));
            let bottom = if row * 2 + 1 < out_h {
                blend(*resized.get_pixel(x, row * 2 + 1))
            } else {
                Color::Rgb(
                    ALPHA_BLEND_BG.0 as u8,
                    ALPHA_BLEND_BG.1 as u8,
                    ALPHA_BLEND_BG.2 as u8,
                )
            };
            // "▀" is 'static — spans borrow it, no per-cell allocation.
            spans.push(Span::styled(
                "\u{2580}",
                Style::default().fg(top).bg(bottom),
            ));
        }
        lines.push(Line::from(spans));
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny RGBA image as PNG bytes.
    fn png_of(pixels: &[[u8; 4]], w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        for (i, p) in pixels.iter().enumerate() {
            img.put_pixel(i as u32 % w, i as u32 / w, image::Rgba(*p));
        }
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn two_pixels_become_one_halfblock_cell() {
        // 1×2 image into a 1×1 cell area: red on top, blue on bottom →
        // one cell, fg red bg blue. (Larger areas upscale to fill.)
        let png = png_of(&[[255, 0, 0, 255], [0, 0, 255, 255]], 1, 2);
        let lines = render_halfblock_lines(&png, 1, 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        let span = &lines[0].spans[0];
        assert_eq!(span.content, "\u{2580}");
        assert_eq!(span.style.fg, Some(Color::Rgb(255, 0, 0)));
        assert_eq!(span.style.bg, Some(Color::Rgb(0, 0, 255)));
    }

    #[test]
    fn aspect_ratio_fits_within_bounds() {
        // 100×50 image into 20×20 cells (=20×40 px): width binds → 20×10 px
        // → 5 cell rows of 20 cells.
        let img = image::RgbaImage::from_pixel(100, 50, image::Rgba([0, 255, 0, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        let lines = render_halfblock_lines(&out.into_inner(), 20, 20).unwrap();
        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|l| l.spans.len() == 20));
    }

    #[test]
    fn transparent_pixels_blend_to_dark() {
        let png = png_of(&[[255, 255, 255, 0], [255, 255, 255, 0]], 1, 2);
        let lines = render_halfblock_lines(&png, 4, 4).unwrap();
        let span = &lines[0].spans[0];
        assert_eq!(
            span.style.fg,
            Some(Color::Rgb(
                ALPHA_BLEND_BG.0 as u8,
                ALPHA_BLEND_BG.1 as u8,
                ALPHA_BLEND_BG.2 as u8
            ))
        );
    }

    #[test]
    fn garbage_bytes_return_none() {
        assert!(render_halfblock_lines(b"not an image", 10, 10).is_none());
        let png = png_of(&[[1, 2, 3, 255]], 1, 1);
        assert!(render_halfblock_lines(&png, 0, 10).is_none());
    }
}
