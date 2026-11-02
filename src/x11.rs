use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{sleep, JoinHandle};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{InputBackend, InputEvent, InputOptions, MouseButton};

// Fake input's `type_`.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6; // mouse motion

// Wheel clicks.
const WHEEL_UP: u8 = 4;
const WHEEL_DOWN: u8 = 5;
const WHEEL_LEFT: u8 = 6;
const WHEEL_RIGHT: u8 = 7;

/// Timeout to type the next char.
const TYPE_DELAY: Duration = Duration::from_millis(10);

pub struct X11Input {
    conn: RustConnection,
    root: Window,
    /// keysym to keycode, read from the server once.
    keysyms: HashMap<u32, u8>,
    /// A keycode with no keysyms, temporarily bound to type other characters.
    scratch: Option<u8>,
    /// Mouse buttons currently down.
    buttons: Vec<u8>,
    /// Keys currently down.
    keys: Vec<u8>,
    /// Easing pointer motion.
    glide: Option<Glide>,
}

impl X11Input {
    /// Connect to an X display. `display` of `None` follows `$DISPLAY`.
    ///
    /// TODO: support multiple displays?
    pub fn new() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("connect: {e}"))?;
        let root = conn.setup().roots[screen].root;

        conn.xtest_get_version(2, 2)
            .map_err(|e| format!("xtest query: {e}"))?
            .reply()
            .map_err(|e| format!("xtest version: {e}"))?;

        let (keysyms, scratch) = keymap(&conn)?;
        Ok(Self {
            conn,
            root,
            keysyms,
            scratch,
            buttons: Vec::new(),
            keys: Vec::new(),
            glide: None,
        })
    }

    /// One XTEST call.
    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.conn
            .xtest_fake_input(type_, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(ReplyError::from)
            .and_then(|cookie| cookie.check())
            .map_err(|e| format!("fake input: {e}"))
    }

    /// Move the mouse to `(x, y)`.
    ///
    /// `smooth` applys spring smooth to the pointer motion
    fn move_to(&mut self, x: i16, y: i16, smooth: bool) -> Result<(), String> {
        let Some(glide) = self.glide.as_ref() else {
            return self.fake(MOTION_NOTIFY, 0, x, y);
        };

        let mut p = glide.state.lock().unwrap();
        p.target = (x, y);

        if smooth {
            return Ok(());
        }

        p.pos = (x, y);
        self.conn
            .xtest_fake_input(MOTION_NOTIFY, 0, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map(|_| ())
            .map_err(|e| format!("fake input: {e}"))?;
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }

    /// The position of the pointer on the root window.
    fn pointer(&self) -> Result<(i16, i16), String> {
        let reply = self
            .conn
            .query_pointer(self.root)
            .map_err(|e| format!("pointer: {e}"))?
            .reply()
            .map_err(|e| format!("pointer: {e}"))?;
        Ok((reply.root_x, reply.root_y))
    }

    /// Turn `notches` into wheel clicks.
    fn wheel(&mut self, button: u8, notches: i32) -> Result<(), String> {
        for _ in 0..notches {
            self.fake(BUTTON_PRESS, button, 0, 0)?; // wheel doesn't need x/y
            self.fake(BUTTON_RELEASE, button, 0, 0)?;
        }
        Ok(())
    }

    /// Type every character in order.
    fn type_text(&mut self, text: &str) -> Result<(), String> {
        for ch in text.chars() {
            let keysym = keysym_of(ch);
            match self.keysyms.get(&keysym).copied() {
                Some(keycode) => {
                    self.fake(KEY_PRESS, keycode, 0, 0)?;
                    self.fake(KEY_RELEASE, keycode, 0, 0)?;
                }
                None => self.type_unbound(keysym)?,
            }
        }
        Ok(())
    }

    /// Bind the spare keycode to a character the layout cannot type.
    fn type_unbound(&mut self, keysym: u32) -> Result<(), String> {
        let scratch = self.scratch.ok_or("no unused keycode to bind")?;
        self.rebind(scratch, keysym)?;
        self.fake(KEY_PRESS, scratch, 0, 0)?;
        sleep(TYPE_DELAY);
        self.fake(KEY_RELEASE, scratch, 0, 0)?;
        sleep(TYPE_DELAY);
        self.rebind(scratch, x11rb::NO_SYMBOL)
    }

    /// Point a keycode at a keysym, or back at nothing.
    fn rebind(&mut self, keycode: u8, keysym: u32) -> Result<(), String> {
        self.conn
            .change_keyboard_mapping(1, keycode, 1, &[keysym])
            .map_err(|e| format!("rebind: {e}"))?
            .check()
            .map_err(|e| format!("rebind: {e}"))?;
        self.conn.sync().map_err(|e| format!("rebind sync: {e}"))
    }
}

impl InputBackend for X11Input {
    fn handle(&mut self, events: &[InputEvent]) -> Result<(), String> {
        for event in events {
            match event {
                InputEvent::Move { x, y } => self.move_to(*x, *y, true)?,
                InputEvent::Button {
                    at,
                    button,
                    pressed,
                } => {
                    if let Some(p) = *at {
                        self.move_to(p.x, p.y, false)?;
                    }
                    let code = mouse_code_of(*button);
                    let type_ = if *pressed {
                        BUTTON_PRESS
                    } else {
                        BUTTON_RELEASE
                    };
                    self.fake(type_, code, 0, 0)?;
                    track(&mut self.buttons, code, *pressed);
                }
                InputEvent::Scroll { at, dx, dy } => {
                    if let Some(p) = *at {
                        self.move_to(p.x, p.y, false)?;
                    }
                    self.wheel(WHEEL_UP, -*dy)?;
                    self.wheel(WHEEL_DOWN, *dy)?;
                    self.wheel(WHEEL_LEFT, -*dx)?;
                    self.wheel(WHEEL_RIGHT, *dx)?;
                }
                InputEvent::Key { code, pressed } => {
                    let keycode: Option<u8> =
                        keysym(code).and_then(|k| self.keysyms.get(&k).copied());
                    if let Some(keycode) = keycode {
                        let type_ = if *pressed { KEY_PRESS } else { KEY_RELEASE };
                        self.fake(type_, keycode, 0, 0)?;
                        track(&mut self.keys, keycode, *pressed);
                    }
                }
                InputEvent::Text { text } => self.type_text(text)?,
            }
        }
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }

    fn configure(&mut self, options: &InputOptions) -> Result<(), String> {
        if options.smooth == self.glide.is_some() {
            return Ok(());
        }

        self.glide = None;
        if !options.smooth {
            return Ok(());
        }

        let Some(tick) = frame(&self.conn, self.root) else {
            return Err("no frame rate to ease against".into());
        };
        let pos = self.pointer()?;
        self.glide = Some(Glide::start(self.root, tick, pos));
        Ok(())
    }

    fn release(&mut self) {
        for &button in &self.buttons {
            let _ = self.fake(BUTTON_RELEASE, button, 0, 0);
        }
        for &code in &self.keys {
            let _ = self.fake(KEY_RELEASE, code, 0, 0);
        }
        self.buttons.clear();
        self.keys.clear();
        let _ = self.conn.flush();
    }
}

/// The button number for a mouse button click.
fn mouse_code_of(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

/// Unshifted keysym for a `KeyboardEvent.code`.
fn keysym(code: &str) -> Option<u32> {
    if let Some(rest) = code.strip_prefix("Key") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_uppercase() {
                return Some(0x61 + (b - b'A') as u32);
            }
        }
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_digit() {
                return Some(0x30 + (b - b'0') as u32);
            }
        }
    }
    if let Some(rest) = code.strip_prefix("Numpad") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_digit() {
                return Some(0xffb0 + (b - b'0') as u32);
            }
        }
    }
    if let Some(rest) = code.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0xffbe + n - 1);
            }
        }
    }

    Some(match code {
        "Escape" => 0xff1b,
        "Tab" => 0xff09,
        "CapsLock" => 0xffe5,
        "Backspace" => 0xff08,
        "Enter" => 0xff0d,
        "Space" => 0x0020,

        "Insert" => 0xff63,
        "Delete" => 0xffff,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,

        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,

        "PrintScreen" => 0xff61,
        "ScrollLock" => 0xff14,
        "Pause" => 0xff13,
        "ContextMenu" => 0xff67,

        "ShiftLeft" => 0xffe1,
        "ShiftRight" => 0xffe2,
        "ControlLeft" => 0xffe3,
        "ControlRight" => 0xffe4,
        "AltLeft" => 0xffe9,
        "AltRight" => 0xffea,
        "MetaLeft" => 0xffeb,
        "MetaRight" => 0xffec,

        "Minus" => 0x002d,
        "Equal" => 0x003d,
        "BracketLeft" => 0x005b,
        "BracketRight" => 0x005d,
        "Backslash" => 0x005c,
        "Semicolon" => 0x003b,
        "Quote" => 0x0027,
        "Backquote" => 0x0060,
        "Comma" => 0x002c,
        "Period" => 0x002e,
        "Slash" => 0x002f,

        "NumLock" => 0xff7f,
        "NumpadEnter" => 0xff8d,
        "NumpadMultiply" => 0xffaa,
        "NumpadAdd" => 0xffab,
        "NumpadSubtract" => 0xffad,
        "NumpadDecimal" => 0xffae,
        "NumpadDivide" => 0xffaf,
        "NumpadEqual" => 0xffbd,

        _ => return None,
    })
}

/// A character's keysym.
fn keysym_of(ch: char) -> u32 {
    let c = ch as u32;

    if matches!(ch,
        'a'..='z' |
        '0'..='9' |
        ' ' | '`' | '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/'
    ) {
        c
    } else {
        c | 0x0100_0000
    }
}

/// One step to move a `delta`.
fn step(delta: i16, f: f64) -> i16 {
    match (delta as f64 * f).round() as i16 {
        0 if delta != 0 => delta.signum(),
        s => s,
    }
}

/// One frame, from the mode driving the fastest enabled CRTC.
fn frame(conn: &RustConnection, root: Window) -> Option<Duration> {
    let resources = conn.randr_get_screen_resources(root).ok()?.reply().ok()?;
    let mut fastest = 0.0_f64;

    for &crtc in &resources.crtcs {
        let Ok(cookie) = conn.randr_get_crtc_info(crtc, x11rb::CURRENT_TIME) else {
            continue;
        };
        let Ok(info) = cookie.reply() else {
            continue;
        };
        // A disabled CRTC has no mode, and no mode id is 0.
        let Some(mode) = resources.modes.iter().find(|m| m.id == info.mode) else {
            continue;
        };

        let lines = mode.htotal as f64 * mode.vtotal as f64;
        let hz = mode.dot_clock as f64 / lines;
        if lines > 0.0 && hz.is_finite() && hz > fastest {
            fastest = hz;
        }
    }

    (fastest > 0.0).then(|| Duration::from_secs_f64(1.0 / fastest))
}

/// A pointer position and the target it's easing towards.
struct Pointer {
    pos: (i16, i16),
    target: (i16, i16),
}

/// A thread easing the pointer towards each new target.
struct Glide {
    state: Arc<Mutex<Pointer>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Glide {
    fn start(root: Window, tick: Duration, pos: (i16, i16)) -> Self {
        let state = Arc::new(Mutex::new(Pointer { pos, target: pos }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            std::thread::spawn(move || ease(root, tick, state, stop))
        };

        Self {
            state,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Glide {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Step the pointer towards its target once per frame.
fn ease(root: Window, tick: Duration, state: Arc<Mutex<Pointer>>, stop: Arc<AtomicBool>) {
    let Ok((conn, _)) = x11rb::connect(None) else {
        eprintln!("ease: no connection");
        return;
    };
    let mut last = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        sleep(tick);
        let now = Instant::now();
        // A stalled thread must not make the pointer jump.
        let dt = (now - last).min(tick);
        last = now;

        let mut p = state.lock().unwrap();
        let (dx, dy) = (p.target.0 - p.pos.0, p.target.1 - p.pos.1);
        if dx == 0 && dy == 0 {
            continue;
        }

        let f = 1.0 - (-(dt.as_secs_f64() / TYPE_DELAY.as_secs_f64())).exp();
        p.pos = (p.pos.0 + step(dx, f), p.pos.1 + step(dy, f));
        let _ = conn
            .xtest_fake_input(
                MOTION_NOTIFY,
                0,
                x11rb::CURRENT_TIME,
                root,
                p.pos.0,
                p.pos.1,
                0,
            )
            .map(|_| ())
            .map_err(|e| format!("fake input: {e}"));
        let _ = conn.flush();
    }
}

/// Keep a pressed set in step with the events.
fn track(held: &mut Vec<u8>, code: u8, pressed: bool) {
    if pressed {
        if !held.contains(&code) {
            held.push(code);
        }
    } else {
        held.retain(|&h| h != code);
    }
}

/// Read the first level of each key, and find a keycode with no keysyms.
fn keymap(conn: &RustConnection) -> Result<(HashMap<u32, u8>, Option<u8>), String> {
    let setup = conn.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode - setup.min_keycode + 1;
    let reply = conn
        .get_keyboard_mapping(first, count)
        .map_err(|e| format!("keymap: {e}"))?
        .reply()
        .map_err(|e| format!("keymap: {e}"))?;

    let per = reply.keysyms_per_keycode as usize;
    let mut keysyms = HashMap::new();
    let mut scratch = None;
    for (i, row) in reply.keysyms.chunks(per).enumerate() {
        let code = first + i as u8;
        if row.iter().all(|&s| s == x11rb::NO_SYMBOL) {
            if scratch.is_none() {
                scratch = Some(code);
            }
        } else if row[0] != x11rb::NO_SYMBOL {
            keysyms.entry(row[0]).or_insert(code);
        }
    }
    Ok((keysyms, scratch))
}
