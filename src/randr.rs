//! RandR display resizing for the nested server.
//!
//! Xephyr exposes a fixed list of modes (1600x1200, 1400x1050, 1280x1024,
//! 1024x768, ...) and can switch between them at runtime.  Xvfb exposes only
//! the mode it was started with, so it can be resized only to that same size;
//! the error explains this rather than silently doing nothing.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, Rotation};
use x11rb::protocol::xproto::ConnectionExt as _;

/// Resize the nested desktop to `width` x `height`.  Returns the RandR mode
/// used and the resulting root geometry.
pub fn resize(display: &str, width: u16, height: u16, refresh: Option<f32>) -> Result<Value> {
    if width < 64 || height < 64 {
        return Err(anyhow!("width and height must be at least 64 pixels"));
    }
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let root = conn.setup().roots[screen_num].root;

    randr::query_version(&conn, 1, 2)
        .context("RandR is not available on this display")?
        .reply()
        .context("RandR QueryVersion failed; this X server has no usable RandR")?;

    let resources = randr::get_screen_resources_current(&conn, root)?
        .reply()
        .context("RandR GetScreenResourcesCurrent failed")?;

    // Mode names live in one concatenated blob.
    let mode_name = |info: &randr::ModeInfo| -> String {
        let start = resources
            .modes
            .iter()
            .take_while(|candidate| candidate.id != info.id)
            .map(|candidate| usize::from(candidate.name_len))
            .sum::<usize>();
        let end = (start + usize::from(info.name_len)).min(resources.names.len());
        if start >= end {
            String::new()
        } else {
            String::from_utf8_lossy(&resources.names[start..end]).into_owned()
        }
    };

    let candidates: Vec<(&randr::ModeInfo, String)> = resources
        .modes
        .iter()
        .filter(|info| info.width == width && info.height == height)
        .map(|info| (info, mode_name(info)))
        .collect();

    if candidates.is_empty() {
        let mut available: Vec<String> = resources
            .modes
            .iter()
            .map(|info| format!("{}x{}", info.width, info.height))
            .collect();
        available.sort();
        available.dedup();
        return Err(anyhow!(
            "this X server has no {}x{} mode. Available modes: {}. \
             Xvfb only offers the size it was started with; use Xephyr (visible=true) \
             for runtime resizing.",
            width,
            height,
            available.join(", ")
        ));
    }

    // Prefer the requested refresh, then the highest refresh rate.
    let refresh_of = |info: &randr::ModeInfo| -> f32 {
        let total = u64::from(info.htotal) * u64::from(info.vtotal);
        if total == 0 {
            0.0
        } else {
            info.dot_clock as f32 / total as f32
        }
    };
    let (mode, name) = candidates
        .iter()
        .max_by(|(left, _), (right, _)| {
            let score = |info: &randr::ModeInfo| {
                let rate = refresh_of(info);
                let preferred = refresh
                    .map(|wanted| 1.0 - (rate - wanted).abs() / wanted.max(1.0))
                    .unwrap_or(0.0);
                preferred + rate / 1000.0
            };
            score(left).total_cmp(&score(right))
        })
        .map(|(info, name)| (*info, name.clone()))
        .ok_or_else(|| anyhow!("no mode matched"))?;

    // Pick the CRTC driving a connected output.  Xephyr has one; Xvfb has one
    // too.  Fall back to any CRTC if the output list is empty or disabled.
    let crtc = resources
        .outputs
        .iter()
        .filter_map(|output| {
            randr::get_output_info(&conn, *output, resources.config_timestamp)
                .ok()?
                .reply()
                .ok()
        })
        .find(|info| info.crtc != 0 && info.connection == randr::Connection::CONNECTED)
        .map(|info| info.crtc)
        .or_else(|| resources.crtcs.first().copied())
        .ok_or_else(|| anyhow!("this display has no RandR CRTC"))?;

    let crtc_info = randr::get_crtc_info(&conn, crtc, resources.config_timestamp)?
        .reply()
        .context("RandR GetCrtcInfo failed")?;
    let outputs = if crtc_info.outputs.is_empty() {
        resources.outputs.clone()
    } else {
        crtc_info.outputs.clone()
    };

    randr::set_crtc_config(
        &conn,
        crtc,
        0,
        resources.config_timestamp,
        0,
        0,
        mode.id,
        Rotation::ROTATE0,
        &outputs,
    )?
    .reply()
    .context("RandR SetCrtcConfig was rejected")?;
    conn.flush()?;

    // The screen size updates asynchronously in the server; wait briefly for
    // the root geometry to reach the requested size.
    let mut actual = (0u16, 0u16);
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(25));
        match conn.get_geometry(root) {
            Ok(cookie) => match cookie.reply() {
                Ok(geometry) => {
                    actual = (geometry.width, geometry.height);
                    if actual == (width, height) {
                        break;
                    }
                }
                Err(_) => break,
            },
            Err(_) => break,
        }
    }

    if actual != (width, height) {
        return Err(anyhow!(
            "RandR accepted mode {name} but the root stayed {}x{} (wanted {}x{})",
            actual.0,
            actual.1,
            width,
            height
        ));
    }

    Ok(json!({
        "display": display,
        "width": actual.0,
        "height": actual.1,
        "mode": name,
        "mode_id": mode.id,
        "refresh": (refresh_of(mode) * 100.0).round() / 100.0,
        "outputs": outputs,
    }))
}

/// List the modes the nested server offers, for diagnostics and errors.
pub fn modes(display: &str) -> Result<Vec<String>> {
    let (conn, screen_num) = x11rb::connect(Some(display))?;
    let root = conn.setup().roots[screen_num].root;
    let resources = randr::get_screen_resources_current(&conn, root)?.reply()?;
    Ok(resources
        .modes
        .iter()
        .map(|info| format!("{}x{}", info.width, info.height))
        .collect())
}
