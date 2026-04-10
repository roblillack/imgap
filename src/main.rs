use image::{DynamicImage, Rgba, RgbaImage};
use std::collections::HashMap;
use std::env;
use std::io::{self, BufWriter, Write};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();

    // Extract optional flags from arguments
    let mut renderer_override: Option<String> = None;
    let mut interactive = false;
    let mut filtered_args: Vec<&str> = Vec::new();
    let mut i = 1; // skip argv[0]
    while i < args.len() {
        if args[i] == "-r" {
            if i + 1 < args.len() {
                renderer_override = Some(args[i + 1].clone());
                i += 2;
                continue;
            } else {
                eprintln!("Error: -r requires a value (kitty, iterm2, sixel, ansi)");
                process::exit(1);
            }
        }
        if args[i] == "-i" {
            interactive = true;
            i += 1;
            continue;
        }
        filtered_args.push(&args[i]);
        i += 1;
    }

    // git diff external command passes 7 args:
    //   path old-file old-hex old-mode new-file new-hex new-mode
    let (path1, path2) = if filtered_args.len() == 7 {
        (filtered_args[1], filtered_args[4])
    } else if filtered_args.len() == 2 {
        (filtered_args[0], filtered_args[1])
    } else {
        eprintln!("Usage: imgap [-r <renderer>] [-i] <image1> <image2>");
        eprintln!("Renderers: kitty, iterm2, sixel, ansi");
        eprintln!("  -i  interactive TUI mode (swipe / onion-skin / 2-up)");
        eprintln!("Also works as: git diff --ext-diff (via diff.*.command)");
        eprintln!("               git difftool (auto-enables interactive mode)");
        process::exit(1);
    };

    // Auto-enable interactive mode when invoked via `git difftool`.
    // Difftool exports LOCAL/REMOTE as env vars for the tool command,
    // while `git diff` with an external driver sets GIT_DIFF_PATH_TOTAL instead.
    if !interactive
        && env::var("GIT_DIFF_PATH_TOTAL").is_err()
        && env::var("LOCAL").is_ok()
        && env::var("REMOTE").is_ok()
    {
        interactive = true;
    }

    let img1 = image::open(path1).unwrap_or_else(|e| {
        eprintln!("Failed to open '{}': {}", path1, e);
        process::exit(1);
    });
    let img2 = image::open(path2).unwrap_or_else(|e| {
        eprintln!("Failed to open '{}': {}", path2, e);
        process::exit(1);
    });

    let protocol = detect_protocol(renderer_override.as_deref());

    if interactive {
        run_interactive(&img1, &img2, &protocol).unwrap_or_else(|e| {
            eprintln!("Interactive mode failed: {}", e);
            process::exit(1);
        });
        return;
    }

    // For text mode each terminal cell is 1 char wide and 2 pixels tall (half-blocks),
    // so compute dimensions differently than for graphics protocols.
    let (term_px_w, term_px_h) = match protocol {
        Protocol::Ansi => terminal_char_size(5),
        _ => terminal_pixel_size(5),
    };

    let comparison = build_comparison(&img1, &img2, term_px_w, term_px_h);

    let stdout = io::stdout().lock();
    let mut w = BufWriter::new(stdout);
    match protocol {
        Protocol::Kitty => write_kitty(&comparison, &mut w),
        Protocol::Iterm2 => write_iterm2(&comparison, &mut w),
        Protocol::Sixel => write_sixel(&comparison, &mut w),
        Protocol::Ansi => write_text(&comparison, &mut w),
    }
    .unwrap_or_else(|e| {
        eprintln!("Failed to write image: {}", e);
        process::exit(1);
    });
}

#[derive(Clone, Copy, PartialEq)]
enum CompareMode {
    TwoUp,
    Swipe,
    OnionSkin,
    Left,
    Right,
}

impl CompareMode {
    /// Cycle to next comparison mode. Single-image modes are skipped;
    /// they're only reachable via the `s` key.
    fn next(self) -> Self {
        match self {
            Self::TwoUp => Self::Swipe,
            Self::Swipe => Self::OnionSkin,
            Self::OnionSkin => Self::TwoUp,
            Self::Left | Self::Right => Self::TwoUp,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::TwoUp => "2-up",
            Self::Swipe => "swipe",
            Self::OnionSkin => "onion skin",
            Self::Left => "left only",
            Self::Right => "right only",
        }
    }
    fn uses_slider(self) -> bool {
        matches!(self, Self::Swipe | Self::OnionSkin)
    }
    fn is_single(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

/// Lay out both images on a common canvas (max of both dimensions),
/// so that identical pixel coordinates line up for swipe/onion comparison.
fn normalize_pair(a: &DynamicImage, b: &DynamicImage) -> (RgbaImage, RgbaImage) {
    let w = a.width().max(b.width());
    let h = a.height().max(b.height());
    let mut out_a = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    let mut out_b = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 0]));
    image::imageops::overlay(&mut out_a, &a.to_rgba8(), 0, 0);
    image::imageops::overlay(&mut out_b, &b.to_rgba8(), 0, 0);
    (out_a, out_b)
}

/// Scale two same-sized images by the same factor to fit a bounding box.
fn scale_pair(a: &RgbaImage, b: &RgbaImage, max_w: u32, max_h: u32) -> (RgbaImage, RgbaImage) {
    let (w, h) = (a.width(), a.height());
    if w <= max_w && h <= max_h {
        return (a.clone(), b.clone());
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = ((w as f64 * scale) as u32).max(1);
    let nh = ((h as f64 * scale) as u32).max(1);
    (
        image::imageops::resize(a, nw, nh, image::imageops::FilterType::Triangle),
        image::imageops::resize(b, nw, nh, image::imageops::FilterType::Triangle),
    )
}

fn compose_two_up(a: &RgbaImage, b: &RgbaImage, max_w: u32, max_h: u32) -> RgbaImage {
    let sep = 4u32;
    let half = max_w.saturating_sub(sep) / 2;
    let sa = scale_rgba(a, half, max_h);
    let sb = scale_rgba(b, half, max_h);
    let th = sa.height().max(sb.height());
    let canvas_w = sa.width() + sep + sb.width();
    let mut canvas = RgbaImage::new(canvas_w, th);
    let gray = Rgba([128, 128, 128, 255]);
    for y in 0..th {
        for x in sa.width()..(sa.width() + sep) {
            canvas.put_pixel(x, y, gray);
        }
    }
    let sa_w = sa.width();
    image::imageops::overlay(&mut canvas, &sa, 0, 0);
    image::imageops::overlay(&mut canvas, &sb, (sa_w + sep) as i64, 0);
    canvas
}

fn compose_swipe(a: &RgbaImage, b: &RgbaImage, t: f32) -> RgbaImage {
    let (w, h) = a.dimensions();
    let split = ((w as f32) * t.clamp(0.0, 1.0)) as u32;
    let mut out = a.clone();
    for y in 0..h {
        for x in split..w {
            out.put_pixel(x, y, *b.get_pixel(x, y));
        }
        if split < w {
            out.put_pixel(split, y, Rgba([255, 220, 0, 255]));
        }
    }
    out
}

fn compose_onion(a: &RgbaImage, b: &RgbaImage, t: f32) -> RgbaImage {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    let (w, h) = a.dimensions();
    let mut out = RgbaImage::new(w, h);
    let ra = a.as_raw();
    let rb = b.as_raw();
    let dst = out.as_mut();
    let mut i = 0;
    while i < ra.len() {
        dst[i] = (ra[i] as f32 * inv + rb[i] as f32 * t) as u8;
        dst[i + 1] = (ra[i + 1] as f32 * inv + rb[i + 1] as f32 * t) as u8;
        dst[i + 2] = (ra[i + 2] as f32 * inv + rb[i + 2] as f32 * t) as u8;
        dst[i + 3] = (ra[i + 3] as f32 * inv + rb[i + 3] as f32 * t) as u8;
        i += 4;
    }
    out
}

fn draw_status_bar(
    w: &mut impl Write,
    cols: u16,
    rows: u16,
    slider: f32,
    mode: CompareMode,
) -> io::Result<()> {
    let slider_row = rows.saturating_sub(2).max(1);
    let help_row = rows.saturating_sub(1).max(1);

    // Slider line
    write!(w, "\x1b[{};1H\x1b[2K", slider_row + 1)?;
    let pct = (slider * 100.0).round() as u32;
    let label = format!(" {:3}%", pct);
    let bar_width = (cols as usize).saturating_sub(label.len() + 2).max(10);
    let pos = ((bar_width as f32) * slider) as usize;
    let active = mode.uses_slider();
    write!(w, "[")?;
    for i in 0..bar_width {
        if active && i == pos.min(bar_width - 1) {
            write!(w, "\x1b[33m█\x1b[0m")?;
        } else if active && i < pos {
            write!(w, "=")?;
        } else if active {
            write!(w, "-")?;
        } else {
            write!(w, "·")?;
        }
    }
    write!(w, "]{}", label)?;

    // Help line
    write!(w, "\x1b[{};1H\x1b[2K", help_row + 1)?;
    write!(
        w,
        "mode: \x1b[1m{}\x1b[0m    \x1b[2m←/→ slider   m mode   q quit\x1b[0m",
        mode.label()
    )?;
    Ok(())
}

struct InteractiveCache {
    cols: u16,
    rows: u16,
    usable_rows: u16,
    term_px_w: u32,
    term_px_h: u32,
    /// Pixels per terminal cell row (graphics protocols) or 2 (ANSI half-block).
    cell_h: u32,
    scaled_a: RgbaImage,
    scaled_b: RgbaImage,
}

/// Reserve 3 bottom rows for spacer + slider + help.
const INTERACTIVE_RESERVE_ROWS: u16 = 3;

fn compute_interactive_cache(
    img1: &DynamicImage,
    img2: &DynamicImage,
    protocol: &Protocol,
) -> InteractiveCache {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let usable_rows = rows.saturating_sub(INTERACTIVE_RESERVE_ROWS).max(1);

    let (term_px_w, term_px_h, cell_h) = match protocol {
        Protocol::Ansi => (cols as u32, usable_rows as u32 * 2, 2u32),
        _ => {
            let (fw, fh) = query_pixel_size_csi().unwrap_or((cols as u32 * 8, rows as u32 * 16));
            let cell_h = (fh / rows.max(1) as u32).max(1);
            let usable_px_h = usable_rows as u32 * cell_h;
            (fw, usable_px_h, cell_h)
        }
    };

    let (na, nb) = normalize_pair(img1, img2);
    let (scaled_a, scaled_b) = scale_pair(&na, &nb, term_px_w, term_px_h);

    InteractiveCache {
        cols,
        rows,
        usable_rows,
        term_px_w,
        term_px_h,
        cell_h,
        scaled_a,
        scaled_b,
    }
}

/// Image height in terminal cell rows, given the composed image and cell pixel height.
fn image_cell_rows(img_h: u32, protocol: &Protocol, cell_h: u32) -> u32 {
    match protocol {
        Protocol::Ansi => img_h.div_ceil(2),
        _ => img_h.div_ceil(cell_h).max(1),
    }
}

fn render_interactive_frame(
    img1: &DynamicImage,
    img2: &DynamicImage,
    cached: &mut Option<InteractiveCache>,
    mode: CompareMode,
    slider: f32,
    protocol: &Protocol,
    w: &mut impl Write,
) -> io::Result<()> {
    // Clear screen and home cursor.
    write!(w, "\x1b[2J\x1b[H")?;
    // Kitty: also delete any prior image placements.
    if matches!(protocol, Protocol::Kitty) {
        write!(w, "\x1b_Ga=d;\x1b\\")?;
    }

    if cached.is_none() {
        *cached = Some(compute_interactive_cache(img1, img2, protocol));
    }
    let c = cached.as_ref().unwrap();

    let composed = match mode {
        CompareMode::TwoUp => compose_two_up(&c.scaled_a, &c.scaled_b, c.term_px_w, c.term_px_h),
        CompareMode::Swipe => compose_swipe(&c.scaled_a, &c.scaled_b, slider),
        CompareMode::OnionSkin => compose_onion(&c.scaled_a, &c.scaled_b, slider),
        CompareMode::Left => c.scaled_a.clone(),
        CompareMode::Right => c.scaled_b.clone(),
    };

    // Vertically center the image within the usable rows region.
    let img_rows = image_cell_rows(composed.height(), protocol, c.cell_h);
    let top_pad = (c.usable_rows as u32).saturating_sub(img_rows) / 2;
    write!(w, "\x1b[{};1H", top_pad + 1)?;

    match protocol {
        Protocol::Kitty => write_kitty(&composed, w)?,
        Protocol::Iterm2 => write_iterm2(&composed, w)?,
        Protocol::Sixel => write_sixel(&composed, w)?,
        Protocol::Ansi => write_text(&composed, w)?,
    }

    draw_status_bar(w, c.cols, c.rows, slider, mode)?;
    w.flush()
}

fn run_interactive(
    img1: &DynamicImage,
    img2: &DynamicImage,
    protocol: &Protocol,
) -> io::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyModifiers};
    use std::time::Duration;

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    crossterm::terminal::enable_raw_mode()?;
    // Alt screen + hide cursor.
    write!(out, "\x1b[?1049h\x1b[?25l")?;
    out.flush()?;

    let mut mode = CompareMode::Swipe;
    // The last non-single comparison mode, so `m` can return to it
    // after jumping into Left/Right via `s`.
    let mut last_compare_mode = CompareMode::Swipe;
    let mut slider: f32 = 0.5;
    let mut cached: Option<InteractiveCache> = None;
    let mut dirty = true;

    let step = 0.02_f32;

    let result: io::Result<()> = (|| loop {
        if dirty {
            render_interactive_frame(img1, img2, &mut cached, mode, slider, protocol, &mut out)?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(k) => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Char('m') | KeyCode::Char('M') => {
                        // From a single-image view, return to the last
                        // comparison mode. Otherwise cycle comparison modes.
                        mode = if mode.is_single() {
                            last_compare_mode
                        } else {
                            let next = mode.next();
                            last_compare_mode = next;
                            next
                        };
                        dirty = true;
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => {
                        mode = match mode {
                            CompareMode::Left => CompareMode::Right,
                            CompareMode::Right => CompareMode::Left,
                            other => {
                                last_compare_mode = other;
                                CompareMode::Left
                            }
                        };
                        dirty = true;
                    }
                    KeyCode::Left => {
                        let s = if k.modifiers.contains(KeyModifiers::SHIFT) {
                            step * 5.0
                        } else {
                            step
                        };
                        slider = (slider - s).max(0.0);
                        dirty = true;
                    }
                    KeyCode::Right => {
                        let s = if k.modifiers.contains(KeyModifiers::SHIFT) {
                            step * 5.0
                        } else {
                            step
                        };
                        slider = (slider + s).min(1.0);
                        dirty = true;
                    }
                    KeyCode::Home => {
                        slider = 0.0;
                        dirty = true;
                    }
                    KeyCode::End => {
                        slider = 1.0;
                        dirty = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => {
                    cached = None;
                    dirty = true;
                }
                _ => {}
            }
        }
    })();

    // Restore terminal state.
    let _ = write!(out, "\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
    let _ = crossterm::terminal::disable_raw_mode();
    result
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

/// Open a direct read+write handle to the controlling terminal,
/// bypassing stdin/stdout/stderr which may be redirected.
fn open_tty() -> Option<std::fs::File> {
    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()
    }
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        // On Windows, CONIN$ and CONOUT$ are separate; open CONOUT$ for write
        // and CONIN$ for read. For simplicity we open CONIN$ read+write — the
        // write end will target the console output.
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .ok()
    }
}

/// Send an escape sequence query to the terminal and read the response.
/// Uses /dev/tty (Unix) or CONIN$/CONOUT$ (Windows) so this works even when
/// stdin/stdout/stderr are redirected (e.g. git external diff commands).
fn query_terminal(query: &[u8], terminator: u8, timeout_ms: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::sync::mpsc;
    use std::time::Duration;

    let mut tty = open_tty()?;

    // Preserve existing raw-mode state: if we're already in raw mode
    // (e.g. inside the interactive TUI loop), don't disable it on the
    // way out — that would unexpectedly re-enable line buffering.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        crossterm::terminal::enable_raw_mode().ok()?;
    }

    tty.write_all(query).ok()?;
    tty.flush().ok()?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 128];
        let mut pos = 0;
        while pos < buf.len() {
            match tty.read(&mut buf[pos..]) {
                Ok(0) => break,
                Ok(n) => {
                    pos += n;
                    if buf[..pos].contains(&terminator) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(buf[..pos].to_vec());
    });

    let response = rx.recv_timeout(Duration::from_millis(timeout_ms)).ok();
    if !was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    response
}

/// Return terminal dimensions suitable for half-block text rendering.
/// Each cell is 1 pixel wide and 2 pixels tall (using ▄).
fn terminal_char_size(reserve_rows: u16) -> (u32, u32) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let usable_rows = (rows as u32).saturating_sub(reserve_rows as u32);
    (cols as u32, usable_rows * 2)
}

/// Query terminal pixel size using xterm CSI 14t escape.
/// Returns Some((width, height)) on success.
fn query_pixel_size_csi() -> Option<(u32, u32)> {
    let data = query_terminal(b"\x1b[14t", b't', 200)?;
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

/// Query Sixel support using DA1 (Device Attributes) escape sequence.
/// Sends `ESC [ c` and checks if `4` (Sixel) appears among the reported attributes.
fn query_sixel_support() -> bool {
    let data = match query_terminal(b"\x1b[c", b'c', 200) {
        Some(d) => d,
        None => return false,
    };
    let resp = match std::str::from_utf8(&data) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let start = match resp.find("[?") {
        Some(i) => i + 2,
        None => return false,
    };
    let end = match resp[start..].find('c') {
        Some(i) => start + i,
        None => return false,
    };
    resp[start..end].split(';').any(|p| p.trim() == "4")
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
            let dr = (raw1[i1] as i16 - raw2[i2] as i16).unsigned_abs();
            let dg = (raw1[i1 + 1] as i16 - raw2[i2 + 1] as i16).unsigned_abs();
            let db = (raw1[i1 + 2] as i16 - raw2[i2 + 2] as i16).unsigned_abs();
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
    Ansi,
}

fn parse_renderer(name: &str) -> Option<Protocol> {
    match name.to_lowercase().as_str() {
        "kitty" => Some(Protocol::Kitty),
        "iterm2" => Some(Protocol::Iterm2),
        "sixel" => Some(Protocol::Sixel),
        "ansi" => Some(Protocol::Ansi),
        _ => None,
    }
}

fn detect_protocol(renderer_override: Option<&str>) -> Protocol {
    // -r flag takes highest priority
    if let Some(name) = renderer_override {
        if let Some(p) = parse_renderer(name) {
            return p;
        }
        eprintln!(
            "Unknown renderer '{}'. Valid: kitty, iterm2, sixel, ansi",
            name
        );
        process::exit(1);
    }

    // Then environment variable
    if let Ok(val) = env::var("IMGAP_RENDERER")
        && let Some(p) = parse_renderer(&val)
    {
        return p;
    }

    let in_tmux = env::var("TMUX").is_ok();

    // Inside tmux, Kitty and iTerm2 protocols are not forwarded.
    // Only Sixel (tmux >= 3.4) and ANSI work.
    if !in_tmux {
        if let Ok(tp) = env::var("TERM_PROGRAM") {
            if tp.to_lowercase().contains("kitty") {
                return Protocol::Kitty;
            }
            if tp == "iTerm.app" || tp == "WezTerm" {
                return Protocol::Iterm2;
            }
        }

        if let Ok(term) = env::var("TERM")
            && term.contains("kitty")
        {
            return Protocol::Kitty;
        }

        if env::var("KITTY_WINDOW_ID").is_ok() {
            return Protocol::Kitty;
        }

        if let Ok(lc) = env::var("LC_TERMINAL")
            && lc == "iTerm2"
        {
            return Protocol::Iterm2;
        }
        if env::var("ITERM_SESSION_ID").is_ok() {
            return Protocol::Iterm2;
        }

        if env::var("WEZTERM_EXECUTABLE").is_ok() {
            return Protocol::Iterm2;
        }
    }

    // Probe for Sixel support via DA1 (Device Attributes) query.
    // This works both inside and outside tmux — tmux >= 3.4 will
    // report Sixel capability if its own support is enabled.
    if query_sixel_support() {
        return Protocol::Sixel;
    }

    Protocol::Ansi
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
    let num_bands = height.div_ceil(6) as usize;
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

/// Render image as colored Unicode half-block characters (▄).
/// Each terminal row encodes two pixel rows: background color for the top pixel,
/// foreground color for the bottom pixel.
fn write_text(img: &RgbaImage, w: &mut impl Write) -> io::Result<()> {
    let (width, height) = img.dimensions();

    for y in (0..height).step_by(2) {
        for x in 0..width {
            let top = img.get_pixel(x, y);
            let bottom = if y + 1 < height {
                img.get_pixel(x, y + 1)
            } else {
                top
            };

            let (tr, tg, tb) = blend_alpha_rgb(top);
            let (br, bg, bb) = blend_alpha_rgb(bottom);

            // ESC[48;2;R;G;Bm = background (top pixel)
            // ESC[38;2;R;G;Bm = foreground (bottom pixel)
            write!(w, "\x1b[48;2;{tr};{tg};{tb}m\x1b[38;2;{br};{bg};{bb}m▄")?;
        }
        writeln!(w, "\x1b[0m")?;
    }
    w.flush()
}

/// Blend RGBA pixel against a black background, returning opaque RGB.
fn blend_alpha_rgb(pixel: &Rgba<u8>) -> (u8, u8, u8) {
    let a = pixel[3] as f32 / 255.0;
    (
        (a * pixel[0] as f32) as u8,
        (a * pixel[1] as f32) as u8,
        (a * pixel[2] as f32) as u8,
    )
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

type Rgb = (u8, u8, u8);

fn split_bucket(mut pixels: Vec<Rgb>) -> (Vec<Rgb>, Vec<Rgb>) {
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
