//! Root-window capture in pure Rust: GetImage + PNG encoding.
//!
//! Nothing here depends on ImageMagick, scrot or mss, so a screenshot works on
//! any machine that can connect to the nested display.

use anyhow::{anyhow, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

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

    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, screen.root, 0, 0, width, height, !0)
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

    let visual = conn
        .setup()
        .roots
        .iter()
        .flat_map(|s| s.allowed_depths.iter())
        .filter(|d| d.depth == reply.depth)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)
        .ok_or_else(|| anyhow!("cannot find the root visual for depth {}", reply.depth))?;

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
