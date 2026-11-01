use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::xproto::{ConnectionExt as _, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{InputBackend, InputEvent, MouseButton};

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
    fn move_to(&mut self, x: i16, y: i16) -> Result<(), String> {
        self.fake(MOTION_NOTIFY, 0, x, y)
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
                InputEvent::Move { x, y } => self.move_to(*x, *y)?,
                InputEvent::Button {
                    at,
                    button,
                    pressed,
                } => {
                    if let Some(p) = *at {
                        self.move_to(p.x, p.y)?;
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
                        self.move_to(p.x, p.y)?;
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
