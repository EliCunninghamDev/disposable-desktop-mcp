//! Root-window capture in pure Rust: GetImage + PNG encoding.
//!
//! Nothing here depends on ImageMagick, scrot or mss, so a screenshot works on
//! any machine that can connect to the nested display.

use anyhow::{anyhow, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, top-down.
    pub rgba: Vec<u8>,
}

pub struct Screenshot {
    pub png: Vec<u8>,
    pub source_width: u32,
    pub source_height: u32,
    pub width: u32,
    pub height: u32,
}

impl Screenshot {
    pub fn scale(&self) -> f64 {
        if self.source_width == 0 {
            1.0
        } else {
            self.width as f64 / self.source_width as f64
        }
    }
}

pub fn capture(display: &str) -> Result<Frame> {
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    capture_from(&conn, screen_num)
}

pub fn capture_from(conn: &RustConnection, screen_num: usize) -> Result<Frame> {
    let screen = &conn.setup().roots[screen_num];
    let width = screen.width_in_pixels;
    let height = screen.height_in_pixels;
    if width == 0 || height == 0 {
        return Err(anyhow!("nested display has an empty root window"));
    }
    capture_drawable(conn, screen.root, 0, 0, width, height)
}

/// Capture an axis-aligned region of the root window.
pub fn capture_region(display: &str, x: u16, y: u16, width: u16, height: u16) -> Result<Frame> {
    if width == 0 || height == 0 {
        return Err(anyhow!("capture region must not be empty"));
    }
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let screen = &conn.setup().roots[screen_num];
    if u32::from(x) + u32::from(width) > u32::from(screen.width_in_pixels)
        || u32::from(y) + u32::from(height) > u32::from(screen.height_in_pixels)
    {
        return Err(anyhow!(
            "region {x},{y} {width}x{height} is outside the {}x{} display",
            screen.width_in_pixels,
            screen.height_in_pixels
        ));
    }
    capture_drawable(&conn, screen.root, x, y, width, height)
}

/// Capture a window's own pixels (not the part of the root behind it).
pub fn capture_window(display: &str, window: u32) -> Result<Frame> {
    let (conn, _screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let geometry = conn
        .get_geometry(window)
        .context("GetGeometry failed")?
        .reply()
        .context("window does not exist or is not a drawable")?;
    if geometry.width == 0 || geometry.height == 0 {
        return Err(anyhow!("window {window} has an empty geometry"));
    }
    capture_drawable(&conn, window, 0, 0, geometry.width, geometry.height)
}

/// Root coordinates of a window's top-left client corner.
pub fn window_origin(display: &str, window: u32) -> Result<(i32, i32)> {
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .translate_coordinates(window, root, 0, 0)
        .context("TranslateCoordinates failed")?
        .reply()
        .context("window does not exist")?;
    Ok((i32::from(reply.dst_x), i32::from(reply.dst_y)))
}

fn capture_drawable(
    conn: &RustConnection,
    drawable: u32,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
) -> Result<Frame> {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            drawable,
            i16::try_from(x).unwrap_or(i16::MAX),
            i16::try_from(y).unwrap_or(i16::MAX),
            width,
            height,
            !0,
        )
        .context("GetImage failed")?
        .reply()
        .context("GetImage returned an error")?;

    let format = conn
        .setup()
        .pixmap_formats
        .iter()
        .find(|f| f.depth == reply.depth)
        .ok_or_else(|| anyhow!("no pixmap format for depth {}", reply.depth))?;
    if format.bits_per_pixel != 32 && format.bits_per_pixel != 16 {
        return Err(anyhow!(
            "unsupported pixel format: {} bits per pixel (depth {})",
            format.bits_per_pixel,
            reply.depth
        ));
    }

    // A window (unlike the root) can use a visual other than the root visual,
    // so pick any visual with the reply's depth and use its channel masks.
    let visual = conn
        .setup()
        .roots
        .iter()
        .flat_map(|s| s.allowed_depths.iter())
        .filter(|d| d.depth == reply.depth)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.red_mask != 0 || v.green_mask != 0 || v.blue_mask != 0)
        .ok_or_else(|| anyhow!("cannot find a visual for depth {}", reply.depth))?;

    let (red_mask, green_mask, blue_mask) = (visual.red_mask, visual.green_mask, visual.blue_mask);

    let bpp = format.bits_per_pixel as usize;
    let pad = format.scanline_pad.max(8) as usize;
    let stride = (width as usize * bpp).div_ceil(pad) * (pad / 8);
    let needed = stride
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("screen dimensions overflow"))?;
    if reply.data.len() < needed {
        return Err(anyhow!(
            "short image reply: {} bytes, expected {needed}",
            reply.data.len()
        ));
    }

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row = &reply.data[y * stride..y * stride + width as usize * (bpp / 8)];
        match bpp {
            32 => {
                for px in row.chunks_exact(4) {
                    let value = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
                    rgba.push(channel(value, red_mask));
                    rgba.push(channel(value, green_mask));
                    rgba.push(channel(value, blue_mask));
                    rgba.push(255);
                }
            }
            16 => {
                for px in row.chunks_exact(2) {
                    let value = u16::from_le_bytes([px[0], px[1]]) as u32;
                    rgba.push(channel(value, red_mask));
                    rgba.push(channel(value, green_mask));
                    rgba.push(channel(value, blue_mask));
                    rgba.push(255);
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(Frame {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

fn channel(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let component = (value & mask) >> shift;
    ((component * 255 + max / 2) / max) as u8
}

pub fn fit(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

/// Box-filter downscale (and nearest-neighbour upscale).  The input is RGBA8.
pub fn resize(frame: &Frame, dst_width: u32, dst_height: u32) -> Vec<u8> {
    if dst_width == frame.width && dst_height == frame.height {
        return frame.rgba.clone();
    }
    let dst_width = dst_width.max(1) as usize;
    let dst_height = dst_height.max(1) as usize;
    let src_width = frame.width as usize;
    let src_height = frame.height as usize;
    let mut out = vec![0u8; dst_width * dst_height * 4];

    if dst_width <= src_width && dst_height <= src_height {
        // Average the source rectangle covered by each destination pixel.
        for dy in 0..dst_height {
            let y0 = dy * src_height / dst_height;
            let y1 = (((dy + 1) * src_height).div_ceil(dst_height)).max(y0 + 1);
            for dx in 0..dst_width {
                let x0 = dx * src_width / dst_width;
                let x1 = (((dx + 1) * src_width).div_ceil(dst_width)).max(x0 + 1);
                let mut acc = [0u32; 4];
                let mut count = 0u32;
                for y in y0..y1.min(src_height) {
                    for x in x0..x1.min(src_width) {
                        let i = (y * src_width + x) * 4;
                        acc[0] += frame.rgba[i] as u32;
                        acc[1] += frame.rgba[i + 1] as u32;
                        acc[2] += frame.rgba[i + 2] as u32;
                        acc[3] += frame.rgba[i + 3] as u32;
                        count += 1;
                    }
                }
                let o = (dy * dst_width + dx) * 4;
                if count == 0 {
                    continue;
                }
                for c in 0..4 {
                    out[o + c] = (acc[c] / count) as u8;
                }
            }
        }
    } else {
        // Bilinear for upscaling (or mixed axes), so a resized viewer window
        // stays smooth instead of showing hard nearest-neighbour blocks.
        let x_ratio = src_width as f64 / dst_width as f64;
        let y_ratio = src_height as f64 / dst_height as f64;
        for dy in 0..dst_height {
            let sy = ((dy as f64 + 0.5) * y_ratio - 0.5).max(0.0);
            let y0 = sy.floor() as usize;
            let y1 = (y0 + 1).min(src_height - 1);
            let fy = sy - y0 as f64;
            for dx in 0..dst_width {
                let sx = ((dx as f64 + 0.5) * x_ratio - 0.5).max(0.0);
                let x0 = sx.floor() as usize;
                let x1 = (x0 + 1).min(src_width - 1);
                let fx = sx - x0 as f64;

                let i00 = (y0 * src_width + x0) * 4;
                let i10 = (y0 * src_width + x1) * 4;
                let i01 = (y1 * src_width + x0) * 4;
                let i11 = (y1 * src_width + x1) * 4;
                let di = (dy * dst_width + dx) * 4;
                for channel in 0..4 {
                    let top = frame.rgba[i00 + channel] as f64 * (1.0 - fx)
                        + frame.rgba[i10 + channel] as f64 * fx;
                    let bottom = frame.rgba[i01 + channel] as f64 * (1.0 - fx)
                        + frame.rgba[i11 + channel] as f64 * fx;
                    out[di + channel] = (top * (1.0 - fy) + bottom * fy).round() as u8;
                }
            }
        }
    }
    out
}

/// Crop a region from a frame.  The region is clamped to the frame bounds;
/// an empty result is an error.
pub fn crop(frame: &Frame, x: u32, y: u32, width: u32, height: u32) -> Result<Frame> {
    let x = x.min(frame.width.saturating_sub(1));
    let y = y.min(frame.height.saturating_sub(1));
    let right = (x + width).min(frame.width);
    let bottom = (y + height).min(frame.height);
    let width = right.saturating_sub(x);
    let height = bottom.saturating_sub(y);
    if width == 0 || height == 0 {
        return Err(anyhow!("crop region is outside the captured frame"));
    }
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for row in y..y + height {
        let start = ((row * frame.width + x) * 4) as usize;
        let end = start + (width * 4) as usize;
        rgba.extend_from_slice(&frame.rgba[start..end]);
    }
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

/// Crop a captured toplevel to the compositor's window geometry.
///
/// The foreign-toplevel capture source returns the client surface, which for
/// client-side-decorated toolkits includes symmetric shadow margins around the
/// window geometry that sway reports.  Removing the margins keeps the image
/// origin aligned with `list_windows` coordinates (and with X11 capture).
/// Returns the cropped frame and the removed left/top margin.
pub fn crop_centered(frame: &Frame, width: u32, height: u32) -> Result<(Frame, u32, u32)> {
    if width == 0 || height == 0 {
        return Err(anyhow!("window geometry is empty"));
    }
    let width = width.min(frame.width);
    let height = height.min(frame.height);
    let left = (frame.width - width) / 2;
    let top = (frame.height - height) / 2;
    if left == 0 && top == 0 && width == frame.width && height == frame.height {
        return Ok((frame.clone(), 0, 0));
    }
    let cropped = crop(frame, left, top, width, height)?;
    Ok((cropped, left, top))
}

/// Cheap content hash used by the wait tools.  FNV-1a is fast enough to run
/// over a full frame every poll and has no collision concerns for change
/// detection.
pub fn hash(frame: &Frame) -> u64 {
    let mut value: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in &frame.rgba {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x1000_0000_01b3);
    }
    value
}

/// Cursor overlay source.  XFixes gives the real cursor image; some minimal
/// servers (notably Xvfb) reject `GetCursorImage`, in which case a high
/// contrast crosshair at the pointer position is drawn instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMethod {
    Xfixes,
    Crosshair,
}

/// Draw the pointer onto a frame.  `origin_x`/`origin_y` are the root
/// coordinates of the frame's top-left corner (0,0 for a full-screen capture).
pub fn overlay_cursor(
    display: &str,
    frame: &mut Frame,
    origin_x: i32,
    origin_y: i32,
) -> Result<Option<CursorMethod>> {
    let (conn, _screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;

    if let Ok(cookie) = x11rb::protocol::xfixes::get_cursor_image(&conn) {
        if let Ok(reply) = cookie.reply() {
            if reply.width == 0 || reply.height == 0 || reply.cursor_image.is_empty() {
                return fallback_crosshair(&conn, frame, origin_x, origin_y);
            }
            let cursor_width = i32::from(reply.width);
            let base_x = i32::from(reply.x) - i32::from(reply.xhot) - origin_x;
            let base_y = i32::from(reply.y) - i32::from(reply.yhot) - origin_y;

            for (index, pixel) in reply.cursor_image.iter().enumerate() {
                let alpha = (pixel >> 24) & 0xff;
                if alpha == 0 {
                    continue;
                }
                let x = base_x + (index as i32 % cursor_width);
                let y = base_y + (index as i32 / cursor_width);
                if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
                    continue;
                }
                let src = [(pixel >> 16) & 0xff, (pixel >> 8) & 0xff, pixel & 0xff];
                let offset = ((y as u32 * frame.width + x as u32) * 4) as usize;
                for (channel, source) in src.iter().enumerate() {
                    let dst = frame.rgba[offset + channel] as u32;
                    frame.rgba[offset + channel] =
                        ((source * alpha + dst * (255 - alpha)) / 255) as u8;
                }
                frame.rgba[offset + 3] = 255;
            }
            return Ok(Some(CursorMethod::Xfixes));
        }
    }

    fallback_crosshair(&conn, frame, origin_x, origin_y)
}

fn fallback_crosshair(
    conn: &RustConnection,
    frame: &mut Frame,
    origin_x: i32,
    origin_y: i32,
) -> Result<Option<CursorMethod>> {
    let root = conn.setup().roots[0].root;
    let pointer = conn.query_pointer(root)?.reply()?;
    let x = i32::from(pointer.root_x) - origin_x;
    let y = i32::from(pointer.root_y) - origin_y;
    if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
        return Ok(None);
    }
    let arm = 9i32;
    for offset in -arm..=arm {
        for (px, py) in [(x + offset, y), (x, y + offset)] {
            if px >= 0 && py >= 0 && px < frame.width as i32 && py < frame.height as i32 {
                blend(frame, px as u32, py as u32, (255, 255, 255), 0.85);
            }
        }
    }
    for dy in -1..=1 {
        for dx in -1..=1 {
            let px = x + dx;
            let py = y + dy;
            if px >= 0 && py >= 0 && px < frame.width as i32 && py < frame.height as i32 {
                blend(frame, px as u32, py as u32, (235, 64, 52), 0.95);
            }
        }
    }
    Ok(Some(CursorMethod::Crosshair))
}

/// Draw a 100 px coordinate grid over the frame, with stronger lines every
/// 500 px.  Alpha blending keeps the underlying UI readable.
pub fn overlay_grid(frame: &mut Frame, spacing: u32) {
    let spacing = spacing.clamp(10, 1000);
    let major = spacing.saturating_mul(5);
    for x in (0..frame.width).step_by(spacing as usize) {
        let (color, alpha) = if x % major == 0 {
            ((90, 150, 240), 0.55)
        } else {
            ((110, 140, 190), 0.25)
        };
        draw_line(frame, x, 0, x, frame.height, color, alpha);
    }
    for y in (0..frame.height).step_by(spacing as usize) {
        let (color, alpha) = if y % major == 0 {
            ((90, 150, 240), 0.55)
        } else {
            ((110, 140, 190), 0.25)
        };
        draw_line(frame, 0, y, frame.width, y, color, alpha);
    }
}

fn draw_line(
    frame: &mut Frame,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    color: (u8, u8, u8),
    alpha: f32,
) {
    if y0 == y1 {
        for x in x0..x1 {
            blend(frame, x, y0, color, alpha);
        }
    } else if x0 == x1 {
        for y in y0..y1 {
            blend(frame, x0, y, color, alpha);
        }
    }
}

fn blend(frame: &mut Frame, x: u32, y: u32, color: (u8, u8, u8), alpha: f32) {
    if x >= frame.width || y >= frame.height {
        return;
    }
    let offset = ((y * frame.width + x) * 4) as usize;
    let src = [color.0, color.1, color.2];
    for (channel, source) in src.iter().enumerate() {
        let dst = frame.rgba[offset + channel] as f32;
        frame.rgba[offset + channel] = (*source as f32 * alpha + dst * (1.0 - alpha)).round() as u8;
    }
}

/// Draw numbered boxes around accessibility targets.  Each tuple is
/// `(mark, x, y, width, height)` in root coordinates; `origin` is the root
/// coordinate of the frame's top-left corner.
pub fn draw_marks(
    frame: &mut Frame,
    marks: &[(u32, i32, i32, u32, u32)],
    origin_x: i32,
    origin_y: i32,
) {
    for &(mark, x, y, width, height) in marks {
        let left = x - origin_x;
        let top = y - origin_y;
        let right = left + width as i32 - 1;
        let bottom = top + height as i32 - 1;
        if right < 0 || bottom < 0 || left >= frame.width as i32 || top >= frame.height as i32 {
            continue;
        }
        let color = (0, 220, 160);
        for offset in 0..2 {
            stroke_rect(
                frame,
                left - offset,
                top - offset,
                right + offset,
                bottom + offset,
                color,
            );
        }

        // Label sits just above the box unless that would leave the frame.
        let digits = mark.to_string();
        let scale = 2u32;
        let label_width = digits.len() as u32 * 6 * scale + 6;
        let label_height = 7 * scale + 4;
        let mut label_x = left.max(0);
        let mut label_y = top - label_height as i32 - 1;
        if label_y < 0 {
            label_y = top + 1;
        }
        if label_x + label_width as i32 > frame.width as i32 {
            label_x = frame.width as i32 - label_width as i32;
        }
        if label_x >= 0 && label_y >= 0 {
            fill_rect(
                frame,
                label_x,
                label_y,
                label_width as i32,
                label_height as i32,
                (12, 24, 20),
            );
            draw_digits(
                frame,
                &digits,
                label_x as u32 + 3,
                label_y as u32 + 2,
                scale,
            );
        }
    }
}

fn stroke_rect(
    frame: &mut Frame,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    color: (u8, u8, u8),
) {
    for x in left.max(0)..=right.min(frame.width as i32 - 1) {
        if top >= 0 {
            blend(frame, x as u32, top as u32, color, 0.9);
        }
        if bottom >= 0 && bottom < frame.height as i32 {
            blend(frame, x as u32, bottom as u32, color, 0.9);
        }
    }
    for y in top.max(0)..=bottom.min(frame.height as i32 - 1) {
        if left >= 0 {
            blend(frame, left as u32, y as u32, color, 0.9);
        }
        if right >= 0 && right < frame.width as i32 {
            blend(frame, right as u32, y as u32, color, 0.9);
        }
    }
}

fn fill_rect(frame: &mut Frame, x: i32, y: i32, width: i32, height: i32, color: (u8, u8, u8)) {
    for py in y..y + height {
        for px in x..x + width {
            if px >= 0 && py >= 0 && px < frame.width as i32 && py < frame.height as i32 {
                blend(frame, px as u32, py as u32, color, 0.92);
            }
        }
    }
}

fn draw_digits(frame: &mut Frame, digits: &str, x: u32, y: u32, scale: u32) {
    let mut cursor = x;
    for ch in digits.chars() {
        let Some(digit) = ch.to_digit(10) else {
            continue;
        };
        let rows = DIGITS[digit as usize];
        for (row, bits) in rows.iter().enumerate() {
            for column in 0..5u32 {
                if bits & (1 << (4 - column)) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = cursor + column * scale + dx;
                            let py = y + row as u32 * scale + dy;
                            if px < frame.width && py < frame.height {
                                blend(frame, px, py, (240, 255, 250), 1.0);
                            }
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;
    }
}

/// Five-pixel-wide digit bitmaps.
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
    ],
];

pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buffer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header().context("PNG header failed")?;
        writer
            .write_image_data(rgba)
            .context("PNG encoding failed")?;
    }
    Ok(buffer)
}

/// Capture the nested display and encode a PNG, downscaling to fit the given
/// bounds.  An initial connect/property probe also proves the display is live.
pub fn capture_png(display: &str, max_width: u32, max_height: u32) -> Result<Screenshot> {
    let frame = capture(display)?;
    let (width, height) = fit(frame.width, frame.height, max_width, max_height);
    let scaled = resize(&frame, width, height);
    let png = encode_png(width, height, &scaled)?;
    Ok(Screenshot {
        png,
        source_width: frame.width,
        source_height: frame.height,
        width,
        height,
    })
}

/// Read the session marker the server writes on the nested root window.
pub fn session_marker(conn: &RustConnection, screen_num: usize) -> Option<String> {
    let marker = crate::x11::atom(conn, crate::state::ROOT_MARKER).ok()?;
    let len = conn
        .get_property(
            false,
            conn.setup().roots[screen_num].root,
            marker,
            AtomEnum::STRING,
            0,
            1024,
        )
        .ok()?
        .reply()
        .ok()?;
    if len.value.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&len.value).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        let mut rgba = Vec::new();
        for y in 0..4u8 {
            for x in 0..4u8 {
                rgba.extend_from_slice(&[x * 16, y * 16, 7, 255]);
            }
        }
        Frame {
            width: 4,
            height: 4,
            rgba,
        }
    }

    #[test]
    fn crop_extracts_rows_and_columns() {
        let cropped = crop(&frame(), 1, 1, 2, 2).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(&cropped.rgba[0..4], &[16, 16, 7, 255]);
        assert_eq!(&cropped.rgba[4..8], &[32, 16, 7, 255]);
    }

    #[test]
    fn crop_clamps_to_frame() {
        let cropped = crop(&frame(), 3, 3, 100, 100).unwrap();
        assert_eq!((cropped.width, cropped.height), (1, 1));
    }

    #[test]
    fn hash_changes_with_pixels() {
        let mut other = frame();
        let before = hash(&other);
        other.rgba[0] ^= 0xff;
        assert_ne!(before, hash(&other));
    }
}

#[cfg(test)]
mod crop_tests {
    use super::*;

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            rgba: vec![7; (width * height * 4) as usize],
        }
    }

    #[test]
    fn crop_centered_removes_shadow_margins() {
        let (out, left, top) = crop_centered(&frame(344, 307), 300, 263).unwrap();
        assert_eq!((out.width, out.height), (300, 263));
        assert_eq!((left, top), (22, 22));
    }

    #[test]
    fn crop_centered_is_a_noop_when_sizes_match() {
        let (out, left, top) = crop_centered(&frame(300, 263), 300, 263).unwrap();
        assert_eq!((out.width, out.height), (300, 263));
        assert_eq!((left, top), (0, 0));
    }

    #[test]
    fn crop_centered_clamps_to_the_frame() {
        let (out, left, top) = crop_centered(&frame(100, 80), 300, 263).unwrap();
        assert_eq!((out.width, out.height), (100, 80));
        assert_eq!((left, top), (0, 0));
    }

    #[test]
    fn crop_centered_rejects_empty_geometry() {
        assert!(crop_centered(&frame(10, 10), 0, 10).is_err());
    }
}
