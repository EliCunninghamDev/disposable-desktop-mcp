//! Wayland pointer and keyboard injection through compositor virtual devices.
//!
//! Sway exposes `zwlr_virtual_pointer_v1` and `zwp_virtual_keyboard_v1`.  Both
//! create synthetic devices inside the compositor, so events cannot escape to
//! the host and no ydotool/uinput device is involved.
//!
//! The connection and the virtual devices are kept alive for the whole
//! session: a held mouse button or key is compositor state that only persists
//! while the protocol object exists.  One connection per session is stored in
//! a process-global registry keyed by the Wayland socket path.

use crate::vk::virtual_keyboard::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use crate::wayland::now_ms;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_keyboard, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use xkbcommon::xkb;

/// Linux evdev codes for the printable part of a PC keyboard.  Literal text is
/// placed on these keys so clients that fall back to a key's physical meaning
/// (Chromium) still see printable keys.  Escape, Backspace, Tab, Enter, and the
/// modifiers are deliberately excluded.
const TEXT_KEYCODES: [u32; 48] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 57,
];

/// Wayland key events use evdev scancodes plus 8.
const KEY_OFFSET: u32 = 8;

/// Linux input button codes.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_FORWARD: u32 = 0x115;
const BTN_BACK: u32 = 0x116;

/// Modifier state supplied by a viewer's toolkit, independent of the XKB
/// modifier-mask layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct ModBits {
    shift: u32,
    ctrl: u32,
    alt: u32,
    super_key: u32,
}

/// Plain-data view of the keymap we hand to the compositor.  The `xkb` objects
/// are not `Send`, so only the compiled text and a keysym lookup table survive
/// construction.
struct KeyTable {
    text: String,
    by_sym: HashMap<u32, (u16, u32)>,
    mods: ModBits,
}

fn build_key_table() -> Result<KeyTable> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkb::COMPILE_NO_FLAGS)
            .ok_or_else(|| {
                anyhow!("cannot compile the default US keymap (is xkeyboard-config installed?)")
            })?;

    let mut by_sym: HashMap<u32, (u16, u32)> = HashMap::new();
    for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let keycode = xkb::Keycode::new(raw);
        if keymap.key_get_name(keycode).is_none() {
            continue;
        }
        for layout in 0..keymap.num_layouts_for_key(keycode) {
            for level in 0..keymap.num_levels_for_key(keycode, layout) {
                let syms = keymap.key_get_syms_by_level(keycode, layout, level);
                if syms.len() != 1 {
                    continue;
                }
                let mut masks = [0u32; 16];
                let count = keymap.key_get_mods_for_level(keycode, layout, level, &mut masks);
                let mask = masks[..count]
                    .iter()
                    .copied()
                    .min_by_key(|mask| mask.count_ones())
                    .unwrap_or(0);
                by_sym.entry(syms[0].raw()).or_insert((raw as u16, mask));
            }
        }
    }

    let mut mods = ModBits::default();
    for index in 0..keymap.num_mods() {
        let bit = 1u32 << index;
        match keymap.mod_get_name(index) {
            "Shift" => mods.shift = bit,
            "Control" => mods.ctrl = bit,
            "Mod1" => mods.alt = bit,
            "Mod4" => mods.super_key = bit,
            _ => {}
        }
    }
    if mods.shift == 0 || mods.ctrl == 0 || mods.alt == 0 || mods.super_key == 0 {
        bail!("the default keymap is missing a standard modifier; cannot synthesize shortcuts");
    }

    Ok(KeyTable {
        text: keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
        by_sym,
        mods,
    })
}

/// Keep the keymap sent to the compositor in sync with the keycodes we send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeymapKind {
    Base,
    Dynamic,
}

#[derive(Default)]
struct InputObjects {
    seat: Option<wl_seat::WlSeat>,
    pointer: Option<ZwlrVirtualPointerV1>,
    keyboard: Option<ZwpVirtualKeyboardV1>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: zwlr_virtual_pointer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: crate::vk::virtual_keyboard::zwp_virtual_keyboard_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for InputObjects {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: zwp_virtual_keyboard_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

pub struct WaylandInput {
    conn: Connection,
    queue: EventQueue<InputObjects>,
    objects: InputObjects,
    table: KeyTable,
    keymap: KeymapKind,
    width: u32,
    height: u32,
    /// Mask from modifier-only key_down calls.
    held_mods: u32,
    /// key_down calls that include a non-modifier key.
    held_keys: HashMap<String, (u16, u32)>,
    /// One-shot key presses from a viewer, keyed by keysym, with their level
    /// and modifier mask, so the matching release restores state.
    held_syms: HashMap<u32, (u16, u32)>,
}

impl WaylandInput {
    fn connect(socket: &Path, width: u32, height: u32) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to Wayland socket {}", socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let conn = Connection::from_socket(stream)
            .with_context(|| format!("cannot initialize Wayland client on {}", socket.display()))?;

        let table = build_key_table()?;
        let (globals, mut queue) = registry_queue_init::<InputObjects>(&conn)
            .context("cannot read the compositor global list")?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=9, ())
            .map_err(|err| anyhow!("the compositor did not advertise a wl_seat: {err}"))?;
        let pointer_manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|err| anyhow!("wlr-virtual-pointer is not available: {err}"))?;
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|err| anyhow!("zwp_virtual_keyboard_v1 is not available: {err}"))?;

        let mut objects = InputObjects::default();
        queue
            .roundtrip(&mut objects)
            .context("failed while reading seat information")?;
        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        objects.seat = Some(seat);
        objects.pointer = Some(pointer);
        objects.keyboard = Some(keyboard);

        let mut input = WaylandInput {
            conn,
            queue,
            objects,
            table,
            keymap: KeymapKind::Base,
            width: width.max(1),
            height: height.max(1),
            held_mods: 0,
            held_keys: HashMap::new(),
            held_syms: HashMap::new(),
        };
        input.upload_keymap(&input.table.text.clone())?;
        input.keymap = KeymapKind::Base;
        input.pump()?;
        Ok(input)
    }

    pub fn set_extent(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    fn pointer(&self) -> Result<&ZwlrVirtualPointerV1> {
        self.objects
            .pointer
            .as_ref()
            .ok_or_else(|| anyhow!("the virtual pointer is gone"))
    }

    fn keyboard(&self) -> Result<&ZwpVirtualKeyboardV1> {
        self.objects
            .keyboard
            .as_ref()
            .ok_or_else(|| anyhow!("the virtual keyboard is gone"))
    }

    /// Drain pending events and flush requests.  Seat events are not needed,
    /// but the queue must not grow without bound.
    fn pump(&mut self) -> Result<()> {
        self.queue
            .dispatch_pending(&mut self.objects)
            .context("Wayland dispatch failed")?;
        self.conn.flush().context("cannot flush Wayland requests")?;
        Ok(())
    }

    fn upload_keymap(&mut self, text: &str) -> Result<()> {
        let mut bytes = text.as_bytes().to_vec();
        if !bytes.ends_with(&[0]) {
            bytes.push(0);
        }
        let name = c"xephyr-mcp-keymap";
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("memfd_create failed");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let written = unsafe {
            libc::write(
                fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len() as libc::size_t,
            )
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error()).context("cannot write the keymap fd");
        }
        if written as usize != bytes.len() {
            bail!(
                "short write to the keymap fd: {written} of {} bytes",
                bytes.len()
            );
        }
        self.keyboard()?.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            fd.as_fd(),
            bytes.len() as u32,
        );
        self.conn.flush().context("cannot flush the keymap")?;
        Ok(())
    }

    fn ensure_base_keymap(&mut self) -> Result<()> {
        if self.keymap == KeymapKind::Base {
            return Ok(());
        }
        let text = self.table.text.clone();
        self.upload_keymap(&text)?;
        self.keymap = KeymapKind::Base;
        Ok(())
    }

    fn dynamic_keymap(&mut self, chars: &[char]) -> Result<()> {
        let text = unicode_keymap_text(chars);
        self.upload_keymap(&text)?;
        self.keymap = KeymapKind::Dynamic;
        Ok(())
    }

    fn effective_mods(&self) -> u32 {
        self.held_mods
            | self
                .held_keys
                .values()
                .fold(0, |mask, (_, contributed)| mask | contributed)
            | self
                .held_syms
                .values()
                .fold(0, |mask, (_, contributed)| mask | contributed)
    }

    fn send_modifiers(&mut self, mask: u32) -> Result<()> {
        self.keyboard()?.modifiers(mask, 0, 0, 0);
        self.conn.flush().context("cannot flush modifiers")?;
        Ok(())
    }

    /// Send one key event.  `xkb_keycode` is in xkbcommon's keycode space
    /// (evdev scancode + 8); the Wayland event carries the evdev scancode.
    fn key_event(&mut self, xkb_keycode: u32, pressed: bool) -> Result<()> {
        let keycode = xkb_keycode
            .checked_sub(KEY_OFFSET)
            .ok_or_else(|| anyhow!("invalid XKB keycode {xkb_keycode}"))?;
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        self.keyboard()?.key(now_ms(), keycode, state.into());
        self.conn.flush().context("cannot flush a key event")?;
        Ok(())
    }

    // ---- pointer -------------------------------------------------------

    pub fn move_to(&mut self, x: i32, y: i32) -> Result<Value> {
        self.pump()?;
        let (cx, cy) = self.clamp_point(x, y);
        self.pointer()?
            .motion_absolute(now_ms(), cx, cy, self.width, self.height);
        self.pointer()?.frame();
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "move",
            "x": cx,
            "y": cy,
            "backend": "wayland",
        }))
    }

    fn clamp_point(&self, x: i32, y: i32) -> (u32, u32) {
        let max_x = self.width.saturating_sub(1) as i32;
        let max_y = self.height.saturating_sub(1) as i32;
        (x.clamp(0, max_x) as u32, y.clamp(0, max_y) as u32)
    }

    fn button_code(button: u32) -> Result<u32> {
        Ok(match button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            8 => BTN_BACK,
            9 => BTN_FORWARD,
            other => bail!(
                "mouse button {other} is not a pointer button on Wayland; use 1 (left), 2 (middle), 3 (right), 8, or 9"
            ),
        })
    }

    fn modifier_bits(&self, modifiers: &[String]) -> Result<u32> {
        let mut mask = 0u32;
        for modifier in modifiers {
            mask |= crate::input::modifier_key(modifier)
                .map(|name| self.table.mods.for_name(name))
                .unwrap_or(0);
        }
        Ok(mask)
    }

    pub fn click(
        &mut self,
        x: i32,
        y: i32,
        button: u32,
        count: u32,
        modifiers: &[String],
    ) -> Result<Value> {
        self.pump()?;
        let (cx, cy) = self.clamp_point(x, y);
        self.pointer()?
            .motion_absolute(now_ms(), cx, cy, self.width, self.height);
        self.pointer()?.frame();
        let mask = self.effective_mods() | self.modifier_bits(modifiers)?;
        self.send_modifiers(mask)?;
        if (4..=7).contains(&button) {
            let value = 10.0 * count.max(1) as f64;
            let (axis, value) = match button {
                4 => (wl_pointer::Axis::VerticalScroll, -value),
                5 => (wl_pointer::Axis::VerticalScroll, value),
                6 => (wl_pointer::Axis::HorizontalScroll, -value),
                _ => (wl_pointer::Axis::HorizontalScroll, value),
            };
            self.pointer()?.axis_source(wl_pointer::AxisSource::Wheel);
            self.pointer()?.axis(now_ms(), axis, value);
            self.pointer()?.frame();
        } else {
            let code = Self::button_code(button)?;
            for index in 0..count.max(1) {
                self.pointer()?
                    .button(now_ms(), code, wl_pointer::ButtonState::Pressed);
                self.pointer()?
                    .button(now_ms(), code, wl_pointer::ButtonState::Released);
                if index + 1 < count {
                    std::thread::sleep(Duration::from_millis(12));
                }
            }
            self.pointer()?.frame();
        }
        self.send_modifiers(self.effective_mods())?;
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "click",
            "x": cx,
            "y": cy,
            "button": button,
            "count": count.max(1),
            "backend": "wayland",
        }))
    }

    pub fn button_at(&mut self, at: Option<(i32, i32)>, button: u32, down: bool) -> Result<Value> {
        self.pump()?;
        if let Some((x, y)) = at {
            let (cx, cy) = self.clamp_point(x, y);
            self.pointer()?
                .motion_absolute(now_ms(), cx, cy, self.width, self.height);
            self.pointer()?.frame();
        }
        let code = Self::button_code(button)?;
        let state = if down {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer()?.button(now_ms(), code, state);
        self.pointer()?.frame();
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": if down { "mouse_down" } else { "mouse_up" },
            "button": button,
            "backend": "wayland",
        }))
    }

    pub fn drag(
        &mut self,
        from: (i32, i32),
        to: (i32, i32),
        button: u32,
        duration_ms: u32,
    ) -> Result<Value> {
        self.pump()?;
        let code = Self::button_code(button)?;
        let (fx, fy) = self.clamp_point(from.0, from.1);
        self.pointer()?
            .motion_absolute(now_ms(), fx, fy, self.width, self.height);
        self.pointer()?.frame();
        self.pointer()?
            .button(now_ms(), code, wl_pointer::ButtonState::Pressed);
        self.pointer()?.frame();

        let steps = 8u32;
        let delay = (duration_ms.max(40) as f64 / steps as f64 / 1000.0).min(0.15);
        for step in 1..=steps {
            let x = from.0 + (to.0 - from.0) * step as i32 / steps as i32;
            let y = from.1 + (to.1 - from.1) * step as i32 / steps as i32;
            let (cx, cy) = self.clamp_point(x, y);
            self.pointer()?
                .motion_absolute(now_ms(), cx, cy, self.width, self.height);
            self.pointer()?.frame();
            self.conn.flush().context("cannot flush a drag step")?;
            std::thread::sleep(Duration::from_secs_f64(delay));
        }
        self.pointer()?
            .button(now_ms(), code, wl_pointer::ButtonState::Released);
        self.pointer()?.frame();
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "drag",
            "from": { "x": from.0, "y": from.1 },
            "to": { "x": to.0, "y": to.1 },
            "button": button,
            "backend": "wayland",
        }))
    }

    pub fn scroll(&mut self, at: Option<(i32, i32)>, up: bool, amount: u32) -> Result<Value> {
        self.pump()?;
        if let Some((x, y)) = at {
            let (cx, cy) = self.clamp_point(x, y);
            self.pointer()?
                .motion_absolute(now_ms(), cx, cy, self.width, self.height);
            self.pointer()?.frame();
        }
        let amount = amount.clamp(1, 100);
        let value = if up {
            -10.0 * amount as f64
        } else {
            10.0 * amount as f64
        };
        self.pointer()?.axis_source(wl_pointer::AxisSource::Wheel);
        self.pointer()?
            .axis(now_ms(), wl_pointer::Axis::VerticalScroll, value);
        self.pointer()?.frame();
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "scroll",
            "direction": if up { "up" } else { "down" },
            "amount": amount,
            "backend": "wayland",
        }))
    }

    // ---- keyboard ------------------------------------------------------

    fn lookup(&self, token: &str) -> Result<(u16, u32)> {
        let keysym = match token.chars().count() {
            1 => xkb::Keysym::from_char(token.chars().next().unwrap()),
            _ => {
                let canonical = canonical_key_name(token);
                xkb::keysym_from_name(&canonical, xkb::KEYSYM_NO_FLAGS)
            }
        };
        if keysym == xkb::Keysym::NoSymbol {
            bail!("unknown key name {token:?}");
        }
        self.table
            .by_sym
            .get(&keysym.raw())
            .copied()
            .ok_or_else(|| anyhow!("key {token:?} has no keycode in the default US keymap"))
    }

    fn parse_combo(&self, combo: &str) -> Result<ParsedCombo> {
        let mut mask = 0u32;
        let mut key = None;
        for token in combo.split('+').map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(bit) = self.table.mods.bit_for_token(token) {
                mask |= bit;
            } else if key.is_some() {
                bail!("key combo {combo:?} names more than one non-modifier key");
            } else {
                let (keycode, level_mask) = self.lookup(token)?;
                key = Some((token.to_string(), keycode, level_mask));
            }
        }
        if key.is_none() && mask == 0 {
            bail!("key combo must name a key or modifier");
        }
        Ok((mask, key))
    }

    /// Forward a toolkit key event by keysym.  Modifier key events themselves
    /// are represented by `modifiers`, not sent as ordinary keys.
    pub fn key_sym(&mut self, keysym: u32, modifiers: Modifiers, pressed: bool) -> Result<Value> {
        self.pump()?;
        self.ensure_base_keymap()?;
        let (keycode, level_mask) =
            self.table.by_sym.get(&keysym).copied().ok_or_else(|| {
                anyhow!("the default keymap has no keycode for keysym {keysym:#x}")
            })?;
        if pressed {
            let mask = self.table.mods.mask_for_modifiers(modifiers) | level_mask;
            self.held_syms.insert(keysym, (keycode, mask));
            self.send_modifiers(self.effective_mods())?;
            self.key_event(keycode as u32, true)?;
        } else {
            if let Some((code, _)) = self.held_syms.remove(&keysym) {
                self.key_event(code as u32, false)?;
            }
            self.send_modifiers(self.effective_mods())?;
        }
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": if pressed { "key_press" } else { "key_release" },
            "keysym": keysym,
            "backend": "wayland",
        }))
    }

    pub fn key_combo(&mut self, combo: &str, delay_ms: u32) -> Result<Value> {
        self.pump()?;
        self.ensure_base_keymap()?;
        let (mod_mask, key) = self.parse_combo(combo)?;
        let (name, keycode, level_mask) =
            key.ok_or_else(|| anyhow!("key combo {combo:?} needs a non-modifier key"))?;
        let pressed = self.effective_mods() | mod_mask | level_mask;
        self.send_modifiers(pressed)?;
        self.key_event(keycode as u32, true)?;
        std::thread::sleep(Duration::from_millis(delay_ms.min(1000) as u64));
        self.key_event(keycode as u32, false)?;
        self.send_modifiers(self.effective_mods())?;
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "key",
            "combo": combo,
            "key": name,
            "backend": "wayland",
        }))
    }

    pub fn key_hold(&mut self, combo: &str, down: bool) -> Result<Value> {
        self.pump()?;
        self.ensure_base_keymap()?;
        let (mod_mask, key) = self.parse_combo(combo)?;
        match key {
            None => {
                if down {
                    self.held_mods |= mod_mask;
                } else {
                    self.held_mods &= !mod_mask;
                }
            }
            Some((_, keycode, level_mask)) => {
                let contributed = (mod_mask | level_mask) & !self.effective_mods();
                if down {
                    if self.held_keys.contains_key(combo) {
                        bail!("key {combo:?} is already held");
                    }
                    self.held_keys
                        .insert(combo.to_string(), (keycode, contributed));
                    self.send_modifiers(self.effective_mods())?;
                    self.key_event(keycode as u32, true)?;
                } else {
                    self.held_keys.remove(combo);
                    self.key_event(keycode as u32, false)?;
                    self.send_modifiers(self.effective_mods())?;
                }
            }
        }
        if key.is_none() {
            self.send_modifiers(self.effective_mods())?;
        }
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": if down { "key_down" } else { "key_up" },
            "key": combo,
            "modifiers": self.effective_mods(),
            "backend": "wayland",
        }))
    }

    pub fn type_text(&mut self, text: &str, delay_ms: u32) -> Result<Value> {
        if text.is_empty() {
            bail!("text must not be empty");
        }
        self.pump()?;
        let saved_mods = self.held_mods;
        self.held_mods = 0;
        self.send_modifiers(self.effective_mods())?;

        let chars: Vec<char> = text.chars().collect();
        let mut printable: Vec<char> = Vec::new();
        for ch in &chars {
            if ch.is_control() {
                continue;
            }
            printable.push(*ch);
        }
        let mut index = 0usize;
        let mut typed = 0usize;
        while index < printable.len() {
            let end = (index + TEXT_KEYCODES.len()).min(printable.len());
            let chunk = &printable[index..end];
            self.dynamic_keymap(chunk)?;
            for (offset, _) in chunk.iter().enumerate() {
                let keycode = TEXT_KEYCODES[offset] + KEY_OFFSET;
                self.key_event(keycode, true)?;
                self.key_event(keycode, false)?;
                typed += 1;
                if delay_ms > 0 && typed < printable.len() {
                    std::thread::sleep(Duration::from_millis(delay_ms.min(1000) as u64));
                }
            }
            index = end;
        }

        // Newlines and tabs need their real keys, because they are not
        // printable and have no Unicode keysym.
        self.ensure_base_keymap()?;
        let mut newlines = 0usize;
        for ch in &chars {
            let token = match ch {
                '\n' | '\r' => "Return",
                '\t' => "Tab",
                _ => continue,
            };
            let (keycode, mask) = self.lookup(token)?;
            self.send_modifiers(mask)?;
            self.key_event(keycode as u32, true)?;
            self.key_event(keycode as u32, false)?;
            newlines += 1;
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms.min(1000) as u64));
            }
        }
        self.send_modifiers(self.effective_mods())?;
        self.held_mods = saved_mods;
        self.send_modifiers(self.effective_mods())?;
        self.pump()?;
        Ok(json!({
            "ok": true,
            "action": "type",
            "characters": typed + newlines,
            "backend": "wayland",
        }))
    }
}

impl ModBits {
    fn for_name(&self, name: &str) -> u32 {
        match name {
            "ctrl" | "control" => self.ctrl,
            "shift" => self.shift,
            "alt" | "option" => self.alt,
            "super" | "meta" | "win" | "logo" => self.super_key,
            _ => 0,
        }
    }

    fn mask_for_modifiers(&self, modifiers: Modifiers) -> u32 {
        (if modifiers.shift { self.shift } else { 0 })
            | (if modifiers.ctrl { self.ctrl } else { 0 })
            | (if modifiers.alt { self.alt } else { 0 })
            | (if modifiers.super_key {
                self.super_key
            } else {
                0
            })
    }

    fn bit_for_token(&self, token: &str) -> Option<u32> {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(self.ctrl),
            "shift" => Some(self.shift),
            "alt" | "option" => Some(self.alt),
            "super" | "meta" | "win" | "logo" | "cmd" | "command" => Some(self.super_key),
            _ => None,
        }
    }
}

/// `(modifier mask, optional (key name, xkb keycode, level mask))`.
type ParsedCombo = (u32, Option<(String, u16, u32)>);

/// Keymap used for literal text: each character gets the keysym at level 0 of
/// one physical printable key, matching wtype's Chromium-safe layout.
fn unicode_keymap_text(chars: &[char]) -> String {
    let mut text = String::with_capacity(256 + chars.len() * 48);
    text.push_str("xkb_keymap {\n");
    text.push_str("xkb_keycodes \"(unnamed)\" {\nminimum = 8;\nmaximum = ");
    text.push_str(&(TEXT_KEYCODES[TEXT_KEYCODES.len() - 1] + KEY_OFFSET).to_string());
    text.push_str(";\n");
    for (index, code) in TEXT_KEYCODES.iter().enumerate() {
        text.push_str(&format!("<K{}> = {};\n", index + 1, code + KEY_OFFSET));
    }
    text.push_str("};\n");
    text.push_str("xkb_types \"(unnamed)\" { include \"complete\" };\n");
    text.push_str("xkb_compatibility \"(unnamed)\" { include \"complete\" };\n");
    text.push_str("xkb_symbols \"(unnamed)\" {\n");
    for (index, ch) in chars.iter().enumerate() {
        text.push_str(&format!(
            "key <K{}> {{ [ U{:04X} ] }};\n",
            index + 1,
            *ch as u32
        ));
    }
    text.push_str("};\n};\n");
    text
}

fn canonical_key_name(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    match lowered.as_str() {
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" | "spacebar" => "space",
        "backspace" | "bksp" => "BackSpace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" | "prior" => "Prior",
        "pagedown" | "pgdn" | "next" => "Next",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "print" | "printscreen" | "print_screen" => "Print",
        "pause" => "Pause",
        "menu" => "Menu",
        "capslock" | "caps_lock" => "Caps_Lock",
        "numlock" | "num_lock" => "Num_Lock",
        "scrolllock" | "scroll_lock" => "Scroll_Lock",
        "plus" => "plus",
        "minus" => "minus",
        "equal" | "equals" => "equal",
        "comma" => "comma",
        "period" | "dot" => "period",
        "slash" => "slash",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "apostrophe" | "quote" => "apostrophe",
        "grave" | "backtick" => "grave",
        "bracketleft" => "bracketleft",
        "bracketright" => "bracketright",
        "kp_enter" | "kpenter" => "KP_Enter",
        other => return other.to_string(),
    }
    .to_string()
}

static INPUTS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<WaylandInput>>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<WaylandInput>>>> {
    INPUTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One virtual pointer/keyboard pair per session socket.  The object is reused
/// across tool calls so held buttons and keys keep their compositor state.
pub fn shared(
    runtime_dir: &Path,
    display: &str,
    width: u32,
    height: u32,
) -> Result<Arc<Mutex<WaylandInput>>> {
    let socket = runtime_dir.join(display);
    let mut guard = registry()
        .lock()
        .map_err(|_| anyhow!("the Wayland input registry is poisoned"))?;
    if let Some(input) = guard.get(&socket) {
        input
            .lock()
            .map_err(|_| anyhow!("the Wayland input state is poisoned"))?
            .set_extent(width, height);
        return Ok(input.clone());
    }
    let input = Arc::new(Mutex::new(WaylandInput::connect(&socket, width, height)?));
    guard.insert(socket, input.clone());
    Ok(input)
}

/// Drop the input connection when its session stops.  Closing the protocol
/// objects releases any buttons or keys still held.
pub fn disconnect(runtime_dir: &Path, display: &str) {
    let socket = runtime_dir.join(display);
    if let Ok(mut guard) = registry().lock() {
        guard.remove(&socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_common_key_aliases() {
        assert_eq!(canonical_key_name("Enter"), "Return");
        assert_eq!(canonical_key_name("ESC"), "Escape");
        assert_eq!(canonical_key_name("PageUp"), "Prior");
        assert_eq!(canonical_key_name("f12"), "f12");
    }

    #[test]
    fn dynamic_keymap_covers_unicode() {
        let text = unicode_keymap_text(&['a', '\u{2713}', '\u{1F600}']);
        assert!(text.contains("<K1> = 10;"));
        assert!(text.contains("U0061"));
        assert!(text.contains("U2713"));
        assert!(text.contains("U1F600"));
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        assert!(
            xkb::Keymap::new_from_string(
                &context,
                text,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::COMPILE_NO_FLAGS
            )
            .is_some(),
            "the generated Unicode keymap must compile"
        );
    }

    #[test]
    fn base_keymap_maps_common_keys() {
        let Ok(table) = build_key_table() else {
            // xkeyboard-config is not installed in this environment.
            return;
        };
        let lower = xkb::Keysym::from_char('a');
        let (code, mask) = table.by_sym.get(&lower.raw()).copied().expect("letter a");
        assert_eq!(code, 38); // evdev 30 + 8
        assert_eq!(mask, 0);

        let upper = xkb::Keysym::from_char('A');
        let (code, mask) = table.by_sym.get(&upper.raw()).copied().expect("letter A");
        assert_eq!(code, 38);
        assert_eq!(mask & table.mods.shift, table.mods.shift);
        assert_ne!(table.mods.ctrl, 0);
        assert_ne!(table.mods.super_key, 0);
    }
}
