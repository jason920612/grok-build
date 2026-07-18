use std::path::{Path, PathBuf};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::content::{format_bytes, format_mime};
use super::geometry::{overlay_geometry, plan_image_preview};
use super::*;
use crate::terminal::image::{GraphicsProtocol, set_protocol_for_test};

fn png_header() -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
}

fn sample_image(path: Option<&str>, pixels: bool) -> PastedImage {
    let encoded_bytes = pixels.then(png_header);
    let preview = encoded_bytes
        .as_ref()
        .map(|bytes| {
            crate::prompt_images::PromptImagePreview::ready_for_test(bytes.clone(), (640, 480))
        })
        .unwrap_or_default();
    PastedImage {
        element_id: xai_ratatui_textarea::ElementId::from_raw(1),
        display_number: 1,
        mime_type: "image/png".into(),
        dimensions: Some((640, 480)),
        byte_len: 1536,
        encoded_bytes: encoded_bytes.map(Into::into),
        source_path: path.map(PathBuf::from),
        staged_temp_path: None,
        session_image_path: None,
        preview,
    }
}

fn render_to_string(image: &PastedImage, area: Rect) -> (Option<ImageOverlayRender>, String) {
    let mut buf = Buffer::empty(area);
    let render = render_image_overlay_inner(
        &mut buf,
        area,
        image,
        Color::Black,
        Color::White,
        Color::Gray,
    );
    let rendered = (area.y..area.y + area.height)
        .map(|y| {
            (area.x..area.x + area.width)
                .filter_map(|x| buf.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (render, rendered)
}

#[test]
fn plan_covers_pixels_by_path_matrix() {
    for (protocol, path, pixels, expected_pixels, expected_path) in [
        (
            GraphicsProtocol::Kitty,
            Some("/tmp/logo.png"),
            true,
            true,
            Some(Path::new("/tmp/logo.png")),
        ),
        (GraphicsProtocol::Kitty, None, true, true, None),
        // Without a graphics protocol the half-block text renderer takes
        // the pixel-box path whenever the encoded bytes are in memory.
        (
            GraphicsProtocol::None,
            Some("/tmp/logo.png"),
            true,
            true,
            Some(Path::new("/tmp/logo.png")),
        ),
        (GraphicsProtocol::None, None, true, true, None),
        (
            GraphicsProtocol::None,
            Some("/tmp/logo.png"),
            false,
            false,
            Some(Path::new("/tmp/logo.png")),
        ),
        (GraphicsProtocol::None, None, false, false, None),
    ] {
        let image = sample_image(path, pixels);
        let plan = plan_image_preview(&image, protocol);
        assert_eq!(plan.show_pixels, expected_pixels);
        assert_eq!(plan.display_path, expected_path);
    }
}

#[test]
fn plan_displays_only_user_visible_source_path() {
    let mut image = sample_image(None, true);
    image.source_path = Some(PathBuf::from("/Users/me/original.png"));
    image.session_image_path = Some(PathBuf::from("/tmp/session/image-uuid.png"));
    assert_eq!(
        plan_image_preview(&image, GraphicsProtocol::None).display_path,
        Some(Path::new("/Users/me/original.png"))
    );
    image.source_path = None;
    assert!(
        plan_image_preview(&image, GraphicsProtocol::None)
            .display_path
            .is_none()
    );
}

#[test]
fn paint_pixels_with_path_returns_footer_and_exact_transmission() {
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    crate::terminal::overlay::reset_owner();
    let image = sample_image(Some("/tmp/logo.png"), true);
    let (render, text) = render_to_string(&image, Rect::new(10, 5, 60, 20));
    let render = render.unwrap();
    let placement = render.image_placement.unwrap();
    let escapes = render.escapes.unwrap();
    assert!(text.contains("Image #1"));
    assert!(
        text.contains("Path: /tmp/logo.png"),
        "rendered footer missing path: {text:?}",
    );
    assert!(escapes.as_str().starts_with(&format!(
        "\x1b[{};{}H",
        placement.y + 1,
        placement.x + 1
    )));
    assert!(
        escapes
            .as_str()
            .contains(&format!("c={},r={}", placement.cols, placement.rows))
    );
}

#[test]
fn paint_pixels_without_path_has_no_footer() {
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    let (render, text) = render_to_string(&sample_image(None, true), Rect::new(0, 0, 60, 20));
    assert!(render.unwrap().image_placement.is_some());
    assert!(!text.contains("Path:"));
}

#[test]
fn paint_metadata_with_path_shows_all_fields() {
    // Metadata box renders when no pixel path exists: protocol None AND
    // no in-memory bytes (with bytes, the half-block renderer takes over).
    let _guard = set_protocol_for_test(GraphicsProtocol::None);
    let (render, text) = render_to_string(
        &sample_image(Some("/tmp/logo.png"), false),
        Rect::new(0, 0, 60, 20),
    );
    assert!(render.unwrap().image_placement.is_none());
    assert!(text.contains("Format: PNG"));
    assert!(text.contains("Dimensions: 640 x 480"));
    assert!(text.contains("Path:"));
    assert!(text.contains("logo.png"));
}

#[test]
fn failed_preview_uses_stable_metadata_fallback() {
    let _guard = set_protocol_for_test(GraphicsProtocol::Kitty);
    let mut image = sample_image(Some("/tmp/photo.jpg"), false);
    image.mime_type = "image/jpeg".into();
    image.preview.mark_failed();
    let (render, text) = render_to_string(&image, Rect::new(0, 0, 80, 30));
    assert!(render.unwrap().image_placement.is_none());
    assert!(text.contains("Format: JPEG"));
    assert!(text.contains("Preview unavailable"));
    assert!(!text.contains("Loading..."));
}

#[test]
fn geometry_keeps_metadata_compact_and_pixels_larger() {
    let area = Rect::new(0, 0, 100, 40);
    let metadata = overlay_geometry(area, false, true, (640, 480)).unwrap();
    let pixels = overlay_geometry(area, true, true, (640, 480)).unwrap();
    assert_eq!(
        metadata.overlay_rect.y + metadata.overlay_rect.height,
        area.y + area.height
    );
    assert!(metadata.overlay_rect.height <= 8);
    assert!(
        pixels.overlay_rect.height > metadata.overlay_rect.height
            || pixels.overlay_rect.width > metadata.overlay_rect.width
    );
}

#[test]
fn geometry_honors_plan_specific_minima() {
    assert!(overlay_geometry(Rect::new(0, 0, 20, 20), false, true, (640, 480)).is_none());
    assert!(overlay_geometry(Rect::new(0, 0, 60, 7), true, false, (640, 480)).is_none());
    for height in [6, 7] {
        let geometry =
            overlay_geometry(Rect::new(0, 0, 60, height), false, true, (640, 480)).unwrap();
        assert_eq!(geometry.overlay_rect.height, 6);
    }
}

#[test]
fn formatting_helpers_cover_known_and_unknown_values() {
    assert_eq!(format_mime("image/png"), "PNG");
    assert_eq!(
        format_mime("application/octet-stream"),
        "application/octet-stream"
    );
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1536), "1.5 KB");
    assert_eq!(format_bytes(2_500_000), "2.4 MB");
}

/// Encode a tiny RGBA image as PNG bytes (real decodable PNG — not the
/// bare 8-byte signature from [`png_header`], which half-block cannot decode).
fn real_png_2x2() -> Vec<u8> {
    let mut img = image::RgbaImage::new(2, 2);
    img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
    img.put_pixel(1, 0, image::Rgba([0, 255, 0, 255]));
    img.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
    img.put_pixel(1, 1, image::Rgba([255, 255, 0, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn sample_image_with_real_png() -> PastedImage {
    let png = real_png_2x2();
    let byte_len = png.len();
    PastedImage {
        element_id: xai_ratatui_textarea::ElementId::from_raw(1),
        display_number: 1,
        mime_type: "image/png".into(),
        dimensions: Some((2, 2)),
        byte_len,
        encoded_bytes: Some(png.into()),
        source_path: None,
        staged_temp_path: None,
        session_image_path: None,
        preview: Default::default(),
    }
}

/// After `persist_to_session` clears `encoded_bytes`, half-block hover must
/// still draw from `session_image_path` under `GraphicsProtocol::None`.
#[test]
fn halfblock_hover_still_draws_after_persist_to_session() {
    let _guard = set_protocol_for_test(GraphicsProtocol::None);
    let mut image = sample_image_with_real_png();
    let area = Rect::new(0, 0, 60, 20);

    // Pre-persist: half-block pixel path with in-memory bytes.
    let (render_pre, text_pre) = render_to_string(&image, area);
    let render_pre = render_pre.expect("pre-persist overlay should render");
    assert!(
        render_pre.image_placement.is_some(),
        "pre-persist should use pixel-box geometry"
    );
    assert!(
        text_pre.contains('\u{2580}'),
        "pre-persist should draw half-block cells, got: {text_pre:?}"
    );
    assert!(
        !text_pre.contains("Format: PNG"),
        "pre-persist must not fall back to metadata box, got: {text_pre:?}"
    );

    let dir = tempfile::tempdir().unwrap();
    crate::prompt_images::persist_to_session(&mut image, dir.path()).unwrap();
    assert!(
        image.encoded_bytes.is_none(),
        "persist must clear encoded_bytes"
    );
    assert!(
        image.session_image_path.is_some(),
        "persist must set session_image_path"
    );

    // Post-persist: same half-block path via dual-source (session file).
    let (render_post, text_post) = render_to_string(&image, area);
    let render_post = render_post.expect("post-persist overlay should render");
    assert!(
        render_post.image_placement.is_some(),
        "post-persist should still use pixel-box geometry"
    );
    assert!(
        text_post.contains('\u{2580}'),
        "post-persist should still draw half-block cells, got: {text_post:?}"
    );
    assert!(
        !text_post.contains("Format: PNG"),
        "post-persist must not fall back to metadata box, got: {text_post:?}"
    );
}

/// Plan gate: session path alone is enough for half-block pixel geometry
/// under no graphics protocol (mirrors post-persist shape).
#[test]
fn plan_show_pixels_true_when_only_session_image_path_under_none() {
    let mut image = sample_image(None, false);
    image.session_image_path = Some(PathBuf::from("/tmp/session/image-uuid.png"));
    image.dimensions = Some((640, 480));
    let plan = plan_image_preview(&image, GraphicsProtocol::None);
    assert!(
        plan.show_pixels,
        "session_image_path alone must enable show_pixels under GraphicsProtocol::None"
    );
}
