use image::{DynamicImage, RgbaImage};
use std::io;
use std::path::Path;

use resvg::{tiny_skia, usvg};

/// Embedded Latin fallback font. resvg has no built-in font, so text in an SVG
/// would otherwise render as blanks when no system-fonts feature is enabled.
const DEFAULT_FONT: &[u8] = include_bytes!("../fonts/PublicSans-Regular.ttf");
const DEFAULT_FONT_FAMILY: &str = "Public Sans";

/// Path extension is the cheap filter we use at load time to decide between
/// `image::open` and the SVG pipeline.
pub fn is_svg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("svg") | Some("svgz")
    )
}

/// Read an SVG from disk and rasterize it into an RGBA `DynamicImage` that
/// fits within `max_w` × `max_h` while preserving aspect ratio.
pub fn rasterize(path: &Path, max_w: u32, max_h: u32) -> io::Result<DynamicImage> {
    let data = std::fs::read(path)?;

    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_font_data(DEFAULT_FONT.to_vec());
    opt.font_family = DEFAULT_FONT_FAMILY.to_string();

    let tree = usvg::Tree::from_data(&data, &opt)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("Invalid SVG: {}", e)))?;

    let size = tree.size();
    let svg_w = size.width();
    let svg_h = size.height();
    if svg_w <= 0.0 || svg_h <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SVG has zero dimensions",
        ));
    }

    let scale = (max_w as f32 / svg_w).min(max_h as f32 / svg_h);
    let out_w = (svg_w * scale).max(1.0).round() as u32;
    let out_h = (svg_h * scale).max(1.0).round() as u32;

    let mut pixmap = tiny_skia::Pixmap::new(out_w, out_h).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot allocate {}x{} pixmap", out_w, out_h),
        )
    })?;

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia stores premultiplied RGBA; `image` expects unpremultiplied.
    let rgba = demultiply(pixmap.data(), out_w, out_h);
    let img = RgbaImage::from_raw(out_w, out_h, rgba)
        .ok_or_else(|| io::Error::other("rgba buffer size mismatch"))?;
    Ok(DynamicImage::ImageRgba8(img))
}

fn demultiply(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for px in data.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let (ur, ug, ub) = match a {
            0 => (0, 0, 0),
            255 => (r, g, b),
            _ => {
                let a32 = a as u32;
                let half = a32 / 2;
                (
                    ((r as u32 * 255 + half) / a32).min(255) as u8,
                    ((g as u32 * 255 + half) / a32).min(255) as u8,
                    ((b as u32 * 255 + half) / a32).min(255) as u8,
                )
            }
        };
        out.extend_from_slice(&[ur, ug, ub, a]);
    }
    out
}
