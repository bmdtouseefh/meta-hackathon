//! Real X11 backend: root-window capture + XTEST input (IMPLEMENTATION.md T1.2).
//!
//! One blocking TCP connection per call (via `spawn_blocking`): simple,
//! no stale-connection state, fast enough at ~1 action/sec + 5 fps views.
//! Coordinates are 1:1 screenshot pixels (fixed 1600x900 server geometry).

use async_trait::async_trait;
use image::codecs::jpeg::JpegEncoder;
use image::RgbImage;
use rose_core::{
    Action, Actor, DesktopBackend, DesktopInfo, Frame, FrameId, MouseButton, Result, RoseError,
    Target,
};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{xproto, xtest};
use x11rb::CURRENT_TIME;

const JPEG_QUALITY: u8 = 85;

pub struct X11Desktop {
    display: String,
}

impl X11Desktop {
    pub fn new(display: impl Into<String>) -> Self {
        Self {
            display: display.into(),
        }
    }

    pub fn display(&self) -> &str {
        &self.display
    }
}

// ---------- pure helpers (unit-tested, no display needed) ----------

/// Keysym for a key token: special names, modifiers, or a single char
/// (Latin-1 maps 1:1 onto keysyms; anything else is unsupported for now).
fn keysym_for_token(token: &str) -> Option<u32> {
    let lower = token.to_lowercase();
    let named: Option<u32> = match lower.as_str() {
        "return" | "enter" => Some(0xff0d),
        "tab" => Some(0xff09),
        "escape" | "esc" => Some(0xff1b),
        "backspace" | "back" => Some(0xff08),
        "delete" | "del" => Some(0xffff),
        "space" => Some(0x20),
        "left" => Some(0xff51),
        "up" => Some(0xff52),
        "right" => Some(0xff53),
        "down" => Some(0xff54),
        "home" => Some(0xff50),
        "end" => Some(0xff57),
        "pageup" | "page_up" => Some(0xff55),
        "pagedown" | "page_down" => Some(0xff56),
        "ctrl" | "control" => Some(0xffe3),
        "shift" => Some(0xffe1),
        "alt" => Some(0xffe4),
        "super" | "meta" | "win" => Some(0xffeb),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if lower.starts_with('f') && lower.len() <= 3 {
        if let Ok(n) = lower[1..].parse::<u32>() {
            if (1..=12).contains(&n) {
                return Some(0xffbe + n - 1);
            }
        }
    }
    let mut chars = token.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if (c as u32) < 0x100 => Some(c as u32),
        _ => None,
    }
}

fn is_modifier_token(token: &str) -> bool {
    matches!(
        token.to_lowercase().as_str(),
        "ctrl" | "control" | "shift" | "alt" | "super" | "meta" | "win"
    )
}

fn mouse_button_number(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

/// (button, presses): dy < 0 scrolls down (Phase 1 JSON uses dy=-600 for down).
fn scroll_presses(dy: i32) -> (u8, u32) {
    let button = if dy >= 0 { 4 } else { 5 };
    let reps = (dy.unsigned_abs() / 120).clamp(1, 8);
    (button, reps)
}

// ---------- blocking X11 work (runs inside spawn_blocking) ----------

struct Session {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
}

impl Session {
    fn open(display: &str) -> Result<Self> {
        let (conn, screen_idx) = x11rb::connect(Some(display))
            .map_err(|e| RoseError::Desktop(format!("connect {display}: {e}")))?;
        if conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| RoseError::Desktop(format!("XTEST query: {e}")))?
            .is_none()
        {
            return Err(RoseError::Desktop(
                "XTEST extension not available".to_string(),
            ));
        }
        let root = conn.setup().roots[screen_idx].root;
        Ok(Self { conn, root })
    }

    fn press(&self, detail: u8, is_key: bool) -> Result<()> {
        let event = if is_key {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::BUTTON_PRESS_EVENT
        };
        // deviceid 0 = core device, matching libXtst (the reference client).
        xtest::fake_input(&self.conn, event, detail, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| RoseError::Desktop(format!("input: {e}")))?
            .check()
            .map_err(|e| RoseError::Desktop(format!("input: {e}")))?;
        Ok(())
    }

    fn release(&self, detail: u8, is_key: bool) -> Result<()> {
        let event = if is_key {
            xproto::KEY_RELEASE_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        xtest::fake_input(&self.conn, event, detail, CURRENT_TIME, self.root, 0, 0, 0)
            .map_err(|e| RoseError::Desktop(format!("input: {e}")))?
            .check()
            .map_err(|e| RoseError::Desktop(format!("input: {e}")))?;
        Ok(())
    }

    fn motion(&self, x: u32, y: u32) -> Result<()> {
        xtest::fake_input(
            &self.conn,
            xproto::MOTION_NOTIFY_EVENT,
            0,
            CURRENT_TIME,
            self.root,
            x.min(i16::MAX as u32) as i16,
            y.min(i16::MAX as u32) as i16,
            0,
        )
        .map_err(|e| RoseError::Desktop(format!("motion: {e}")))?
        .check()
        .map_err(|e| RoseError::Desktop(format!("motion: {e}")))?;
        Ok(())
    }

    /// keysym -> (keycode, needs_shift) via the server's keyboard mapping.
    fn keycode_for(&self, keysym: u32) -> Result<(u8, bool)> {
        let setup = self.conn.setup();
        let (min, count) = (setup.min_keycode, setup.max_keycode - setup.min_keycode + 1);
        let reply = xproto::get_keyboard_mapping(&self.conn, min, count)
            .map_err(|e| RoseError::Desktop(format!("keymap: {e}")))?
            .reply()
            .map_err(|e| RoseError::Desktop(format!("keymap: {e}")))?;
        let per = reply.keysyms_per_keycode as usize;
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            if chunk.first() == Some(&keysym) {
                return Ok((min + i as u8, false));
            }
            if per > 1 && chunk.get(1) == Some(&keysym) {
                return Ok((min + i as u8, true));
            }
        }
        Err(RoseError::Desktop(format!(
            "no keycode for keysym U+{keysym:04X}"
        )))
    }

    fn tap_keycode(&self, keycode: u8, shift: u8) -> Result<()> {
        if shift != 0 {
            self.press(shift, true)?;
        }
        self.press(keycode, true)?;
        self.release(keycode, true)?;
        if shift != 0 {
            self.release(shift, true)?;
        }
        Ok(())
    }

    fn shift_keycode(&self) -> Result<u8> {
        Ok(self.keycode_for(0xffe1)?.0)
    }
}

fn capture_blocking(display: &str) -> Result<Frame> {
    let (conn, screen_idx) = x11rb::connect(Some(display))
        .map_err(|e| RoseError::Desktop(format!("connect {display}: {e}")))?;
    let root = conn.setup().roots[screen_idx].root;
    let geom = xproto::get_geometry(&conn, root)
        .map_err(|e| RoseError::Desktop(format!("geometry: {e}")))?
        .reply()
        .map_err(|e| RoseError::Desktop(format!("geometry: {e}")))?;
    let (w, h) = (geom.width as u32, geom.height as u32);
    let reply = xproto::get_image(
        &conn,
        xproto::ImageFormat::Z_PIXMAP,
        root,
        0,
        0,
        geom.width,
        geom.height,
        u32::MAX,
    )
    .map_err(|e| RoseError::Desktop(format!("get_image: {e}")))?
    .reply()
    .map_err(|e| RoseError::Desktop(format!("get_image: {e}")))?;

    // Depth-24 little-endian servers store pixels as B,G,R,pad.
    if reply.data.len() < (w * h * 4) as usize {
        return Err(RoseError::Desktop(format!(
            "short frame: {} bytes for {w}x{h}",
            reply.data.len()
        )));
    }
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let (chunks, _) = reply.data.as_chunks::<4>();
    for px in chunks.iter().take((w * h) as usize) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let img = RgbImage::from_raw(w, h, rgb)
        .ok_or_else(|| RoseError::Desktop("bad pixel buffer".to_string()))?;
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&img)
        .map_err(|e| RoseError::Desktop(format!("jpeg: {e}")))?;
    Ok(Frame {
        id: FrameId::new(),
        captured_at: chrono::Utc::now(),
        width: w,
        height: h,
        jpeg,
        active_window: active_window_blocking(&conn, root),
    })
}

/// Best-effort EWMH active window title; never fails the frame.
fn active_window_blocking(
    conn: &x11rb::rust_connection::RustConnection,
    root: u32,
) -> Option<String> {
    let active_atom = xproto::intern_atom(conn, false, b"_NET_ACTIVE_WINDOW")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let win_reply = xproto::get_property(
        conn,
        false,
        root,
        active_atom,
        xproto::AtomEnum::WINDOW,
        0,
        1,
    )
    .ok()?
    .reply()
    .ok()?;
    let win: u32 = win_reply.value32()?.next()?;
    if win == 0 {
        return None;
    }
    // Prefer UTF-8 _NET_WM_NAME, fall back to WM_NAME.
    let utf8 = xproto::intern_atom(conn, false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let name_atom = xproto::intern_atom(conn, false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if let Ok(reply) = xproto::get_property(conn, false, win, name_atom, utf8, 0, 256)
        .ok()?
        .reply()
    {
        if !reply.value.is_empty() {
            return Some(String::from_utf8_lossy(&reply.value).into_owned());
        }
    }
    let legacy = xproto::intern_atom(conn, false, b"WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let reply = xproto::get_property(conn, false, win, legacy, xproto::AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&reply.value).into_owned())
}

fn act_blocking(display: &str, action: &Action) -> Result<()> {
    let session = Session::open(display)?;
    match action {
        Action::Click { target, button } => {
            let (x, y) = point_of(target)?;
            session.motion(x, y)?;
            let b = mouse_button_number(*button);
            session.press(b, false)?;
            session.release(b, false)?;
        }
        Action::DoubleClick { target } => {
            let (x, y) = point_of(target)?;
            session.motion(x, y)?;
            for _ in 0..2 {
                session.press(1, false)?;
                session.release(1, false)?;
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
        Action::Type { text } => {
            let shift = session.shift_keycode()?;
            for ch in text.chars() {
                let token = ch.to_string();
                let ks = keysym_for_token(if ch == '\n' { "Return" } else { &token })
                    .ok_or_else(|| RoseError::Desktop(format!("cannot type {ch:?} (no keysym)")))?;
                let (kc, need_shift) = session.keycode_for(ks)?;
                session.tap_keycode(kc, if need_shift { shift } else { 0 })?;
            }
        }
        Action::PressKey { keys } => {
            let mut mods = Vec::new();
            let mut mains = Vec::new();
            for k in keys {
                let ks = keysym_for_token(k)
                    .ok_or_else(|| RoseError::Desktop(format!("unknown key {k:?}")))?;
                let (kc, need_shift) = session.keycode_for(ks)?;
                if is_modifier_token(k) {
                    mods.push(kc);
                } else {
                    mains.push((kc, need_shift));
                }
            }
            let shift = session.shift_keycode()?;
            for m in &mods {
                session.press(*m, true)?;
            }
            for (kc, need_shift) in &mains {
                session.tap_keycode(*kc, if *need_shift { shift } else { 0 })?;
            }
            for m in mods.iter().rev() {
                session.release(*m, true)?;
            }
        }
        Action::Scroll { target, dy } => {
            let (x, y) = point_of(target)?;
            session.motion(x, y)?;
            let (button, reps) = scroll_presses(*dy);
            for _ in 0..reps {
                session.press(button, false)?;
                session.release(button, false)?;
            }
        }
        Action::Drag { from, to } => {
            let (x0, y0) = point_of(from)?;
            let (x1, y1) = point_of(to)?;
            session.motion(x0, y0)?;
            session.press(1, false)?;
            session.motion(x1, y1)?;
            session.release(1, false)?;
        }
        Action::Wait { ms } => {
            std::thread::sleep(std::time::Duration::from_millis((*ms).min(10_000) as u64));
        }
    }
    session
        .conn
        .flush()
        .map_err(|e| RoseError::Desktop(format!("flush: {e}")))?;
    Ok(())
}

fn point_of(target: &Target) -> Result<(u32, u32)> {
    match target {
        Target::Point { x, y } => Ok((*x, *y)),
        Target::Element(id) => Err(RoseError::Desktop(format!(
            "element targets need grounding (T1.4+), got {id}"
        ))),
    }
}

async fn blocking<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| RoseError::Internal(format!("join: {e}")))?
}

#[async_trait]
impl DesktopBackend for X11Desktop {
    async fn info(&self) -> Result<DesktopInfo> {
        let display = self.display.clone();
        blocking(move || {
            let (conn, screen_idx) = x11rb::connect(Some(display.as_str()))
                .map_err(|e| RoseError::Desktop(format!("connect {display}: {e}")))?;
            let root = conn.setup().roots[screen_idx].root;
            let geom = xproto::get_geometry(&conn, root)
                .map_err(|e| RoseError::Desktop(format!("geometry: {e}")))?
                .reply()
                .map_err(|e| RoseError::Desktop(format!("geometry: {e}")))?;
            Ok(DesktopInfo {
                display,
                width: geom.width as u32,
                height: geom.height as u32,
                active_window: active_window_blocking(&conn, root),
            })
        })
        .await
    }

    async fn capture(&self) -> Result<Frame> {
        let display = self.display.clone();
        blocking(move || capture_blocking(&display)).await
    }

    async fn act(&self, action: &Action, _actor: Actor) -> Result<()> {
        // Actor is recorded by the agent loop/store, not the wire: both ride
        // the same XTEST path by design.
        tracing::info!(?action, display = %self.display, "xtest act");
        let display = self.display.clone();
        let action = action.clone();
        blocking(move || act_blocking(&display, &action)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_table() {
        assert_eq!(keysym_for_token("Return"), Some(0xff0d));
        assert_eq!(keysym_for_token("esc"), Some(0xff1b));
        assert_eq!(keysym_for_token("ctrl"), Some(0xffe3));
        assert_eq!(keysym_for_token("F5"), Some(0xffc2));
        assert_eq!(keysym_for_token("a"), Some(0x61));
        assert_eq!(keysym_for_token(" "), Some(0x20));
        assert_eq!(keysym_for_token("nope"), None);
        assert_eq!(keysym_for_token("é"), Some(0xe9)); // Latin-1 maps 1:1
        assert_eq!(keysym_for_token("€"), None); // outside Latin-1: unsupported
        assert!(is_modifier_token("Shift"));
        assert!(!is_modifier_token("Return"));
    }

    #[test]
    fn mouse_and_scroll_mapping() {
        assert_eq!(mouse_button_number(MouseButton::Left), 1);
        assert_eq!(mouse_button_number(MouseButton::Middle), 2);
        assert_eq!(mouse_button_number(MouseButton::Right), 3);
        // dy < 0 scrolls down (button 5); reps scale, min 1, max 8.
        assert_eq!(scroll_presses(-600), (5, 5));
        assert_eq!(scroll_presses(100), (4, 1));
        assert_eq!(scroll_presses(-100_000), (5, 8));
    }

    #[tokio::test]
    async fn bogus_display_fails_fast() {
        let d = X11Desktop::new(":999");
        assert!(d.capture().await.is_err());
        assert!(d.info().await.is_err());
    }
}
