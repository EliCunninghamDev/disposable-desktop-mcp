//! Vendored pointer image for screenshot and viewer overlays.
//!
//! The asset is CC BY-NC 4.0, not MIT; see
//! `assets/cursors/posy/README.md` before redistributing this crate.

use std::sync::OnceLock;

/// A decoded pointer image.  Pixels are straight-alpha RGBA in row-major
/// order, which is what the screenshot compositor blends directly.
pub struct CursorImage {
    pub width: u32,
    pub height: u32,
    /// Offset from the image's top-left corner to the pointer hotspot.
    pub hotspot: (i32, i32),
    pub rgba: Vec<u8>,
}

const POSY_LEFT_PTR_PNG: &[u8] = include_bytes!("../assets/cursors/posy/left_ptr-32.png");
const POSY_LEFT_PTR_HOTSPOT: (i32, i32) = (0, 0);

static LEFT_PTR: OnceLock<Option<CursorImage>> = OnceLock::new();

/// The vendored left-pointer arrow.  Returns `None` if the embedded PNG
/// cannot be decoded, letting callers keep the crosshair fallback.
pub fn default_cursor() -> Option<&'static CursorImage> {
    LEFT_PTR.get_or_init(load).as_ref()
}

fn load() -> Option<CursorImage> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(POSY_LEFT_PTR_PNG))
        .read_info()
        .ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut rgba).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    rgba.truncate(info.buffer_size());
    // Xcursor stores premultiplied ARGB; the screenshot compositor and the
    // viewer's straight-alpha path both expect unpremultiplied RGBA.
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
    Some(CursorImage {
        width: info.width,
        height: info.height,
        hotspot: POSY_LEFT_PTR_HOTSPOT,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_cursor_decodes_as_rgba() {
        let cursor = default_cursor().expect("vendored cursor must decode");
        assert_eq!((cursor.width, cursor.height), (32, 32));
        assert_eq!(cursor.hotspot, (0, 0));
        assert_eq!(cursor.rgba.len(), 32 * 32 * 4);
        assert!(cursor.rgba.chunks(4).any(|pixel| pixel[3] > 0));
    }
}
