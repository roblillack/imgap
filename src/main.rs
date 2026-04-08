use image::{DynamicImage, Rgba, RgbaImage};
use std::collections::HashMap;
use std::env;
use std::io::{self, BufWriter, Write};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    // git diff external command passes 7 args:
    //   path old-file old-hex old-mode new-file new-hex new-mode
    let (path1, path2) = if args.len() == 8 {
        (args[2].as_str(), args[5].as_str())
    } else if args.len() == 3 {
        (args[1].as_str(), args[2].as_str())
    } else {
        eprintln!("Usage: imgap <image1> <image2>");
        eprintln!("Also works as: git diff --ext-diff (via diff.*.command)");
        process::exit(1);
    };

    let img1 = image::open(path1).unwrap_or_else(|e| {
        eprintln!("Failed to open '{}': {}", path1, e);
        process::exit(1);
    });
    let img2 = image::open(path2).unwrap_or_else(|e| {
        eprintln!("Failed to open '{}': {}", path2, e);
        process::exit(1);
    });

    // Get available terminal pixel area (reserving 3 rows for the prompt)
    let (term_px_w, term_px_h) = terminal_pixel_size(5);

    let comparison = build_comparison(&img1, &img2, term_px_w, term_px_h);

    let protocol = detect_protocol();
    let stdout = io::stdout().lock();
    let mut w = BufWriter::new(stdout);
    match protocol {
        Protocol::Kitty => write_kitty(&comparison, &mut w),
        Protocol::Iterm2 => write_iterm2(&comparison, &mut w),
        Protocol::Sixel => write_sixel(&comparison, &mut w),
    }
    .unwrap_or_else(|e| {
        eprintln!("Failed to write image: {}", e);
        process::exit(1);
    });
}

/// Query terminal size and return available pixel dimensions (width, height),
/// reserving `reserve_rows` text rows at the bottom.
fn terminal_pixel_size(reserve_rows: u16) -> (u32, u32) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let cols = (cols as u32).max(1);
    let rows = (rows as u32).max(1);
    let usable_rows = rows.saturating_sub(reserve_rows as u32);

    // Try CSI 14t query for actual pixel dimensions
    if let Some((qw, qh)) = query_pixel_size_csi() {
        let cell_h = qh / rows.max(1);
        let reserved_px = reserve_rows as u32 * cell_h;
        return (qw, qh.saturating_sub(reserved_px).max(1));
    }

    // Last resort: assume 8x16 cells
    (cols * 8, usable_rows * 16)
}

/// Query terminal pixel size using xterm CSI 14t escape.
/// Returns Some((width, height)) on success.
fn query_pixel_size_csi() -> Option<(u32, u32)> {
    use std::sync::mpsc;
    use std::time::Duration;

    crossterm::terminal::enable_raw_mode().ok()?;

    // Send CSI 14t (report text area pixel size)
    let _ = io::stderr().write_all(b"\x1b[14t");
    let _ = io::stderr().flush();

    // Read response in a background thread: crossterm's raw mode uses
    // blocking reads, so if the terminal doesn't support this query
    // the read would hang forever. The timeout lets us fall back gracefully.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 64];
        let mut pos = 0;
        while pos < buf.len() {
            match io::stdin().read(&mut buf[pos..]) {
                Ok(0) => break,
                Ok(n) => {
                    pos += n;
                    if buf[..pos].contains(&b't') {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(buf[..pos].to_vec());
    });

    let response = rx.recv_timeout(Duration::from_millis(200)).ok();
    let _ = crossterm::terminal::disable_raw_mode();

    // Parse response: ESC [ 4 ; <height> ; <width> t
    let data = response?;
    let resp = std::str::from_utf8(&data).ok()?;
    let start = resp.find("[4;")?;
    let rest = &resp[start + 3..];
    let end = rest.find('t')?;
    let nums = &rest[..end];
    let mut parts = nums.split(';');
    let h: u32 = parts.next()?.parse().ok()?;
    let w: u32 = parts.next()?.parse().ok()?;
    Some((w, h))
}

/// Scale a DynamicImage to fit within max_w x max_h, preserving aspect ratio.
fn scale_dynamic(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_w && h <= max_h {
        return img.clone();
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let new_w = ((w as f64 * scale) as u32).max(1);
    let new_h = ((h as f64 * scale) as u32).max(1);
    img.resize_exact(new_w, new_h, image::imageops::FilterType::Triangle)
}

fn build_comparison(
    img1: &DynamicImage,
    img2: &DynamicImage,
    term_w: u32,
    term_h: u32,
) -> RgbaImage {
    let sep = 4u32;

    // Layout: top 1/3 for before|after, bottom 2/3 for diff
    let top_h = (term_h.saturating_sub(sep)) / 3;
    let diff_h = term_h.saturating_sub(top_h).saturating_sub(sep);

    // Scale inputs for the top row (each gets half the width, 1/3 the height)
    let thumb_max_w = term_w.saturating_sub(sep) / 2;
    let thumb1 = scale_dynamic(img1, thumb_max_w, top_h);
    let thumb2 = scale_dynamic(img2, thumb_max_w, top_h);
    let tw1 = thumb1.width();
    let th1 = thumb1.height();
    let th2 = thumb2.height();
    let top_row_h = th1.max(th2);

    // Build diff heatmap at full resolution, then scale to fit bottom section
    let diff_img = build_diff_heatmap(img1, img2);
    let diff_scaled = scale_rgba(&diff_img, term_w, diff_h);
    let dw = diff_scaled.width();
    let dh = diff_scaled.height();

    // Canvas sized to actual content
    let top_row_w = tw1 + sep + thumb2.width();
    let canvas_w = top_row_w.max(dw);
    let canvas_h = top_row_h + sep + dh;

    let mut canvas = RgbaImage::new(canvas_w, canvas_h);

    // Draw separators
    let gray = Rgba([128, 128, 128, 255]);
    for y in 0..top_row_h {
        for x in tw1..(tw1 + sep) {
            canvas.put_pixel(x, y, gray);
        }
    }
    for x in 0..canvas_w {
        for y in top_row_h..(top_row_h + sep) {
            canvas.put_pixel(x, y, gray);
        }
    }

    // Blit thumbnails
    let rgba1 = thumb1.to_rgba8();
    let rgba2 = thumb2.to_rgba8();
    image::imageops::overlay(&mut canvas, &rgba1, 0, 0);
    image::imageops::overlay(&mut canvas, &rgba2, (tw1 + sep) as i64, 0);

    // Blit diff centered
    let diff_x_off = ((canvas_w.saturating_sub(dw)) / 2) as i64;
    let diff_y_off = (top_row_h + sep) as i64;
    image::imageops::overlay(&mut canvas, &diff_scaled, diff_x_off, diff_y_off);

    canvas
}

/// Build a diff heatmap at the native resolution of the two images.
fn build_diff_heatmap(img1: &DynamicImage, img2: &DynamicImage) -> RgbaImage {
    let w1 = img1.width();
    let h1 = img1.height();
    let w2 = img2.width();
    let h2 = img2.height();
    let diff_w = w1.max(w2);
    let diff_h = h1.max(h2);
    let common_w = w1.min(w2);
    let common_h = h1.min(h2);

    let rgba1 = img1.to_rgba8();
    let rgba2 = img2.to_rgba8();
    let raw1 = rgba1.as_raw();
    let raw2 = rgba2.as_raw();

    let mut out = RgbaImage::from_pixel(diff_w, diff_h, Rgba([255, 0, 255, 255]));
    let out_raw = out.as_mut();

    for y in 0..common_h {
        for x in 0..common_w {
            let i1 = ((y * w1 + x) * 4) as usize;
            let i2 = ((y * w2 + x) * 4) as usize;
            let dr = (raw1[i1] as i16 - raw2[i2] as i16).unsigned_abs() as u16;
            let dg = (raw1[i1 + 1] as i16 - raw2[i2 + 1] as i16).unsigned_abs() as u16;
            let db = (raw1[i1 + 2] as i16 - raw2[i2 + 2] as i16).unsigned_abs() as u16;
            let diff = (77 * dr + 151 * dg + 28 * db) as f32 / (255.0 * 256.0);
            let pixel = heatmap_color(diff);
            let oi = ((y * diff_w + x) * 4) as usize;
            out_raw[oi] = pixel[0];
            out_raw[oi + 1] = pixel[1];
            out_raw[oi + 2] = pixel[2];
            out_raw[oi + 3] = pixel[3];
        }
    }

    out
}

/// Scale an RgbaImage to fit within max_w x max_h, preserving aspect ratio.
fn scale_rgba(img: &RgbaImage, max_w: u32, max_h: u32) -> RgbaImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_w && h <= max_h {
        return img.clone();
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let new_w = ((w as f64 * scale) as u32).max(1);
    let new_h = ((h as f64 * scale) as u32).max(1);
    image::imageops::resize(img, new_w, new_h, image::imageops::FilterType::Triangle)
}

fn heatmap_color(t: f32) -> Rgba<u8> {
    let t = t.clamp(0.0, 1.0);
    let (r, g, b) = if t < 0.25 {
        let s = t / 0.25;
        (0.0, 0.0, s)
    } else if t < 0.5 {
        let s = (t - 0.25) / 0.25;
        (0.0, s, 1.0 - s)
    } else if t < 0.75 {
        let s = (t - 0.5) / 0.25;
        (s, 1.0, 0.0)
    } else {
        let s = (t - 0.75) / 0.25;
        (1.0, 1.0 - s, 0.0)
    };
    Rgba([(r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8, 255])
}

enum Protocol {
    Kitty,
    Iterm2,
    Sixel,
}

fn detect_protocol() -> Protocol {
    if let Ok(tp) = env::var("TERM_PROGRAM") {
        if tp.to_lowercase().contains("kitty") {
            return Protocol::Kitty;
        }
        if tp == "iTerm.app" || tp == "WezTerm" {
            return Protocol::Iterm2;
        }
    }

    if let Ok(term) = env::var("TERM") {
        if term.contains("kitty") {
            return Protocol::Kitty;
        }
    }

    if env::var("KITTY_WINDOW_ID").is_ok() {
        return Protocol::Kitty;
    }

    if let Ok(lc) = env::var("LC_TERMINAL") {
        if lc == "iTerm2" {
            return Protocol::Iterm2;
        }
    }
    if env::var("ITERM_SESSION_ID").is_ok() {
        return Protocol::Iterm2;
    }

    if env::var("WEZTERM_EXECUTABLE").is_ok() {
        return Protocol::Iterm2;
    }

    Protocol::Sixel
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut buf);
    img.write_with_encoder(encoder).expect("PNG encode failed");
    buf
}

fn write_kitty(img: &RgbaImage, w: &mut impl Write) -> io::Result<()> {
    let png_data = encode_png(img);
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png_data);

    let chunk_size = 4096;
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            write!(w, "\x1b_Gf=100,a=T,m={};{}\x1b\\", more, chunk)?;
        } else {
            write!(w, "\x1b_Gm={};{}\x1b\\", more, chunk)?;
        }
    }
    writeln!(w)?;
    w.flush()
}

fn write_iterm2(img: &RgbaImage, w: &mut impl Write) -> io::Result<()> {
    let png_data = encode_png(img);
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png_data);

    write!(
        w,
        "\x1b]1337;File=inline=1;size={};width=auto;height=auto;preserveAspectRatio=1:{}\x07",
        png_data.len(),
        b64
    )?;
    writeln!(w)?;
    w.flush()
}

fn write_sixel(img: &RgbaImage, w: &mut impl Write) -> io::Result<()> {
    let (width, height) = img.dimensions();

    // Quantize to a 256-color palette using median cut on sampled pixels
    let palette = build_palette(img, 255);

    // Build pixel-to-palette-index map with a cache for repeated colors
    let mut indexed = vec![0u8; (width * height) as usize];
    let mut cache: HashMap<(u8, u8, u8), u8> = HashMap::new();
    for y in 0..height {
        for x in 0..width {
            let p = img.get_pixel(x, y);
            let key = (p[0], p[1], p[2]);
            let idx = *cache
                .entry(key)
                .or_insert_with(|| nearest_color(&palette, p));
            indexed[(y * width + x) as usize] = idx;
        }
    }

    // Pre-compute which colors appear in each band to avoid scanning all 255 colors
    let num_bands = ((height + 5) / 6) as usize;
    let mut band_colors: Vec<Vec<u8>> = Vec::with_capacity(num_bands);
    for band in 0..num_bands {
        let y_start = (band as u32) * 6;
        let y_end = (y_start + 6).min(height);
        let mut seen = [false; 256];
        for y in y_start..y_end {
            for x in 0..width {
                seen[indexed[(y * width + x) as usize] as usize] = true;
            }
        }
        let colors: Vec<u8> = (0..palette.len() as u8)
            .filter(|&c| seen[c as usize])
            .collect();
        band_colors.push(colors);
    }

    // Build sixel data into a buffer
    let mut buf = Vec::with_capacity(width as usize * height as usize);

    // Header
    // P2=1 selects transparent background; raster attributes set 1:1 aspect ratio
    write!(buf, "\x1bP0;1q\"1;1;{};{}", width, height)?;

    // Define colors
    for (i, &(r, g, b)) in palette.iter().enumerate() {
        let rp = (r as u32 * 100) / 255;
        let gp = (g as u32 * 100) / 255;
        let bp = (b as u32 * 100) / 255;
        write!(buf, "#{};2;{};{};{}", i, rp, gp, bp)?;
    }

    // Sixel data
    for (band, colors) in band_colors.iter().enumerate() {
        let y_start = (band as u32) * 6;
        for &color_idx in colors {
            // Build sixel row for this color
            buf.push(b'#');
            // Write color index as ASCII digits
            write!(buf, "{}", color_idx)?;

            let mut row_data = Vec::with_capacity(width as usize);
            for x in 0..width {
                let mut sixel_val = 0u8;
                for bit in 0..6u32 {
                    let y = y_start + bit;
                    if y < height && indexed[(y * width + x) as usize] == color_idx {
                        sixel_val |= 1 << bit;
                    }
                }
                row_data.push(sixel_val + 63);
            }

            // RLE compress
            let mut i = 0;
            while i < row_data.len() {
                let ch = row_data[i];
                let mut count = 1usize;
                while i + count < row_data.len() && row_data[i + count] == ch {
                    count += 1;
                }
                if count >= 3 {
                    write!(buf, "!{}{}", count, ch as char)?;
                } else {
                    for _ in 0..count {
                        buf.push(ch);
                    }
                }
                i += count;
            }
            buf.push(b'$');
        }
        buf.push(b'-');
    }

    // End sixel stream
    buf.extend_from_slice(b"\x1b\\");

    w.write_all(&buf)?;
    writeln!(w)?;
    w.flush()
}

fn build_palette(img: &RgbaImage, max_colors: usize) -> Vec<(u8, u8, u8)> {
    let (width, height) = img.dimensions();
    let total = (width * height) as usize;

    // Sample at most 20K pixels for palette building
    let max_samples = 20_000;
    let step = if total > max_samples {
        total / max_samples
    } else {
        1
    };

    let raw = img.as_raw();
    let mut pixels: Vec<(u8, u8, u8)> = Vec::with_capacity(total / step + 1);
    let mut offset = 0;
    while offset < total {
        let base = offset * 4;
        pixels.push((raw[base], raw[base + 1], raw[base + 2]));
        offset += step;
    }

    let mut buckets: Vec<Vec<(u8, u8, u8)>> = vec![pixels];

    while buckets.len() < max_colors {
        let mut best_idx = 0;
        let mut best_range = 0u32;
        for (i, bucket) in buckets.iter().enumerate() {
            if bucket.len() < 2 {
                continue;
            }
            let range = channel_range(bucket);
            if range > best_range {
                best_range = range;
                best_idx = i;
            }
        }
        if best_range == 0 {
            break;
        }

        let bucket = buckets.swap_remove(best_idx);
        let (a, b) = split_bucket(bucket);
        if !a.is_empty() {
            buckets.push(a);
        }
        if !b.is_empty() {
            buckets.push(b);
        }
    }

    buckets
        .iter()
        .map(|bucket| {
            let (mut sr, mut sg, mut sb) = (0u64, 0u64, 0u64);
            for &(r, g, b) in bucket {
                sr += r as u64;
                sg += g as u64;
                sb += b as u64;
            }
            let n = bucket.len() as u64;
            ((sr / n) as u8, (sg / n) as u8, (sb / n) as u8)
        })
        .collect()
}

fn channel_range(pixels: &[(u8, u8, u8)]) -> u32 {
    let (mut rmin, mut rmax) = (255u8, 0u8);
    let (mut gmin, mut gmax) = (255u8, 0u8);
    let (mut bmin, mut bmax) = (255u8, 0u8);
    for &(r, g, b) in pixels {
        rmin = rmin.min(r);
        rmax = rmax.max(r);
        gmin = gmin.min(g);
        gmax = gmax.max(g);
        bmin = bmin.min(b);
        bmax = bmax.max(b);
    }
    let rd = (rmax - rmin) as u32;
    let gd = (gmax - gmin) as u32;
    let bd = (bmax - bmin) as u32;
    rd.max(gd).max(bd)
}

fn split_bucket(mut pixels: Vec<(u8, u8, u8)>) -> (Vec<(u8, u8, u8)>, Vec<(u8, u8, u8)>) {
    let (mut rmin, mut rmax) = (255u8, 0u8);
    let (mut gmin, mut gmax) = (255u8, 0u8);
    let (mut bmin, mut bmax) = (255u8, 0u8);
    for &(r, g, b) in &pixels {
        rmin = rmin.min(r);
        rmax = rmax.max(r);
        gmin = gmin.min(g);
        gmax = gmax.max(g);
        bmin = bmin.min(b);
        bmax = bmax.max(b);
    }
    let rd = (rmax - rmin) as u32;
    let gd = (gmax - gmin) as u32;
    let bd = (bmax - bmin) as u32;

    if rd >= gd && rd >= bd {
        pixels.sort_unstable_by_key(|p| p.0);
    } else if gd >= bd {
        pixels.sort_unstable_by_key(|p| p.1);
    } else {
        pixels.sort_unstable_by_key(|p| p.2);
    }

    let mid = pixels.len() / 2;
    let b = pixels.split_off(mid);
    (pixels, b)
}

fn nearest_color(palette: &[(u8, u8, u8)], pixel: &Rgba<u8>) -> u8 {
    let (r, g, b) = (pixel[0] as i32, pixel[1] as i32, pixel[2] as i32);
    let mut best = 0u8;
    let mut best_dist = i32::MAX;
    for (i, &(pr, pg, pb)) in palette.iter().enumerate() {
        let dr = r - pr as i32;
        let dg = g - pg as i32;
        let db = b - pb as i32;
        let dist = dr * dr + dg * dg + db * db;
        if dist < best_dist {
            best_dist = dist;
            best = i as u8;
        }
    }
    best
}
