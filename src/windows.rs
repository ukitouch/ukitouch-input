//! Windows input via `SendInput`.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, sleep, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_0, VK_A, VK_ADD, VK_APPS, VK_BACK,
    VK_CAPITAL, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME,
    VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MULTIPLY, VK_NEXT,
    VK_NUMLOCK, VK_NUMPAD0, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7,
    VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE, VK_PRIOR, VK_RCONTROL,
    VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SCROLL, VK_SNAPSHOT, VK_SPACE,
    VK_SUBTRACT, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SetCursorPos, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WHEEL_DELTA, XBUTTON1, XBUTTON2,
};

use super::{InputBackend, InputEvent, InputOptions, MouseButton};

pub struct WindowsInput {
    /// Virtual desktop origin. Input coordinates are relative to this.
    x0: i32,
    /// Virtual desktop origin. Input coordinates are relative to this.
    y0: i32,
    keysyms: HashMap<u32, u16>,
    /// Buttons currently held down, released on disconnect.
    buttons: Vec<MouseButton>,
    /// Keys currently held down, released on disconnect.
    keys: Vec<u16>,
    /// Spring-smooth pointer motion.
    glide: Option<Glide>,
}

impl WindowsInput {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            x0: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            y0: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            keysyms: keymap(),
            buttons: Vec::new(),
            keys: Vec::new(),
            glide: None,
        })
    }

    fn warp(&self, x: i16, y: i16) -> Result<(), String> {
        let ok = unsafe { SetCursorPos(x as i32 + self.x0, y as i32 + self.y0) };
        if ok == 0 {
            return Err(format!("SetCursorPos: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Move the mouse to `(x, y)`.
    ///
    /// `smooth` applies spring smooth to the pointer motion.
    fn move_to(&mut self, x: i16, y: i16, smooth: bool) -> Result<(), String> {
        let Some(glide) = self.glide.as_ref() else {
            return self.warp(x, y);
        };

        {
            let mut p = glide.state.lock().unwrap();
            p.target = (x as f64, y as f64);
            if !smooth {
                p.pos = (x as f64, y as f64);
                p.vel = (0.0, 0.0);
                p.last_move = (x, y);
            }
        }

        if smooth {
            Ok(())
        } else {
            self.warp(x, y)
        }
    }

    /// The position of the pointer on the virtual desktop.
    fn pointer(&self) -> Result<(i16, i16), String> {
        let mut p = POINT { x: 0, y: 0 };
        let ok = unsafe { GetCursorPos(&mut p) };
        if ok == 0 {
            return Err(format!("GetCursorPos: {}", std::io::Error::last_os_error()));
        }
        Ok(((p.x - self.x0) as i16, (p.y - self.y0) as i16))
    }

    fn button(button: MouseButton, pressed: bool) -> Vec<INPUT> {
        let (flags, data) = button_flags(button, pressed);
        return vec![mouse(flags, data)];
    }

    /// Turn `notches` into wheel clicks.
    fn wheel(flags: u32, notches: i32) -> Vec<INPUT> {
        let data = (WHEEL_DELTA as i32 * notches.signum()) as u32;
        (0..notches.abs()).map(|_| mouse(flags, data)).collect()
    }

    /// Type every character in order.
    fn type_text(&self, text: &str) -> Result<Vec<INPUT>, String> {
        let mut all = Vec::new();
        for ch in text.chars() {
            let keysym = keysym_of(ch);
            match self.keysyms.get(&keysym).copied() {
                Some(vk) => {
                    all.push(key(vk, 0, 0));
                    all.push(key(vk, 0, KEYEVENTF_KEYUP));
                }
                None => {
                    let mut buf = [0u16; 2];
                    for &unit in ch.encode_utf16(&mut buf).iter() {
                        if unit == 0 {
                            break;
                        }
                        all.push(key(0, unit, KEYEVENTF_UNICODE));
                        all.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
                    }
                }
            }
        }
        Ok(all)
    }

    fn send(&self, inputs: Vec<INPUT>) -> Result<(), String> {
        if inputs.is_empty() {
            return Ok(());
        }

        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent != inputs.len() as u32 {
            return Err(format!("SendInput: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl InputBackend for WindowsInput {
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
                    self.send(Self::button(*button, *pressed))?;
                    track(&mut self.buttons, *button, *pressed);
                }
                InputEvent::Scroll { at, dx, dy } => {
                    if let Some(p) = *at {
                        self.move_to(p.x, p.y, false)?;
                    }
                    self.send(Self::wheel(MOUSEEVENTF_WHEEL, -*dy))?;
                    self.send(Self::wheel(MOUSEEVENTF_HWHEEL, *dx))?;
                }
                InputEvent::Key { code, pressed } => {
                    if let Some(vk) = vk(code) {
                        let flags = if *pressed { 0 } else { KEYEVENTF_KEYUP };
                        self.send(vec![key(vk, 0, flags)])?;
                        track(&mut self.keys, vk, *pressed);
                    }
                }
                InputEvent::Text { text } => self.send(Self::type_text(self, text)?)?,
            }
        }
        Ok(())
    }

    fn configure(&mut self, options: &InputOptions) -> Result<(), String> {
        if options.smooth == self.glide.is_some() {
            return Ok(());
        }

        self.glide = None;
        if !options.smooth {
            return Ok(());
        }

        let Some(tick) = frame() else {
            return Err("no frame rate to ease against".into());
        };
        let pos = self.pointer()?;
        self.glide = Some(Glide::start(self.x0, self.y0, tick, pos));
        Ok(())
    }

    fn release(&mut self) {
        for &button in &self.buttons {
            let _ = self.send(Self::button(button, false));
        }
        for &vk in &self.keys {
            let _ = self.send(vec![key(vk, 0, KEYEVENTF_KEYUP)]);
        }
        self.buttons.clear();
        self.keys.clear();
    }
}

/// Map a mouse button to its MOUSEEVENTF_* flags and mouseData.
fn button_flags(button: MouseButton, pressed: bool) -> (u32, u32) {
    match button {
        MouseButton::Left => (
            if pressed {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
            0,
        ),
        MouseButton::Middle => (
            if pressed {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
            0,
        ),
        MouseButton::Right => (
            if pressed {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
            0,
        ),
        MouseButton::Back => (
            if pressed {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON1 as u32,
        ),
        MouseButton::Forward => (
            if pressed {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
            XBUTTON2 as u32,
        ),
    }
}

/// Build a MOUSEINPUT at the current cursor position.
fn mouse(flags: u32, data: u32) -> INPUT {
    let mut input = INPUT::default();
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

/// Build a KEYBDINPUT. When `vk` is 0, `scan` is sent as a UTF-16 code unit.
fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut input = INPUT::default();
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

/// Map a DOM KeyboardEvent.code to a Windows virtual-key code.
fn vk(code: &str) -> Option<u16> {
    if let Some(rest) = code.strip_prefix("Key") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_uppercase() {
                return Some(VK_A + (b - b'A') as u16);
            }
        }
    }
    if let Some(rest) = code.strip_prefix("Digit") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_digit() {
                return Some(VK_0 + (b - b'0') as u16);
            }
        }
    }
    if let Some(rest) = code.strip_prefix("Numpad") {
        if rest.len() == 1 {
            let b = rest.as_bytes()[0];
            if b.is_ascii_digit() {
                return Some(VK_NUMPAD0 + (b - b'0') as u16);
            }
        }
    }
    if let Some(rest) = code.strip_prefix('F') {
        if let Ok(n) = rest.parse::<u16>() {
            if (1..=24).contains(&n) {
                return Some(VK_F1 + n - 1);
            }
        }
    }

    Some(match code {
        "Escape" => VK_ESCAPE,
        "Tab" => VK_TAB,
        "CapsLock" => VK_CAPITAL,
        "Backspace" => VK_BACK,
        "Enter" => VK_RETURN,
        "Space" => VK_SPACE,

        "Insert" => VK_INSERT,
        "Delete" => VK_DELETE,
        "Home" => VK_HOME,
        "End" => VK_END,
        "PageUp" => VK_PRIOR,
        "PageDown" => VK_NEXT,

        "ArrowLeft" => VK_LEFT,
        "ArrowUp" => VK_UP,
        "ArrowRight" => VK_RIGHT,
        "ArrowDown" => VK_DOWN,

        "PrintScreen" => VK_SNAPSHOT,
        "ScrollLock" => VK_SCROLL,
        "Pause" => VK_PAUSE,
        "ContextMenu" => VK_APPS,

        "ShiftLeft" => VK_LSHIFT,
        "ShiftRight" => VK_RSHIFT,
        "ControlLeft" => VK_LCONTROL,
        "ControlRight" => VK_RCONTROL,
        "AltLeft" => VK_LMENU,
        "AltRight" => VK_RMENU,
        "MetaLeft" => VK_LWIN,
        "MetaRight" => VK_RWIN,

        "Minus" => VK_OEM_MINUS,
        "Equal" => VK_OEM_PLUS,
        "BracketLeft" => VK_OEM_4,
        "BracketRight" => VK_OEM_6,
        "Backslash" => VK_OEM_5,
        "Semicolon" => VK_OEM_1,
        "Quote" => VK_OEM_7,
        "Backquote" => VK_OEM_3,
        "Comma" => VK_OEM_COMMA,
        "Period" => VK_OEM_PERIOD,
        "Slash" => VK_OEM_2,

        "NumLock" => VK_NUMLOCK,
        "NumpadEnter" => VK_RETURN,
        "NumpadMultiply" => VK_MULTIPLY,
        "NumpadAdd" => VK_ADD,
        "NumpadSubtract" => VK_SUBTRACT,
        "NumpadDecimal" => VK_DECIMAL,
        "NumpadDivide" => VK_DIVIDE,
        "NumpadEqual" => return None,

        _ => return None,
    })
}

/// A character's keysym.  Identical to the X11 backend.
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

fn keymap() -> HashMap<u32, u16> {
    let mut m = HashMap::new();

    for b in b'a'..=b'z' {
        m.insert(b as u32, VK_A + (b - b'a') as u16);
    }
    for b in b'0'..=b'9' {
        m.insert(b as u32, VK_0 + (b - b'0') as u16);
    }

    m.insert(0x0000_0020, VK_SPACE);
    m.insert(0x0000_0060, VK_OEM_3);
    m.insert(0x0000_002d, VK_OEM_MINUS);
    m.insert(0x0000_003d, VK_OEM_PLUS);
    m.insert(0x0000_005b, VK_OEM_4);
    m.insert(0x0000_005d, VK_OEM_6);
    m.insert(0x0000_005c, VK_OEM_5);
    m.insert(0x0000_003b, VK_OEM_1);
    m.insert(0x0000_0027, VK_OEM_7);
    m.insert(0x0000_002c, VK_OEM_COMMA);
    m.insert(0x0000_002e, VK_OEM_PERIOD);
    m.insert(0x0000_002f, VK_OEM_2);

    m
}

struct Pointer {
    pos: (f64, f64),
    vel: (f64, f64),
    target: (f64, f64),
    last_move: (i16, i16),
}

/// A thread easing the pointer towards each new target.
struct Glide {
    state: Arc<Mutex<Pointer>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Glide {
    fn start(x0: i32, y0: i32, tick: Duration, pos: (i16, i16)) -> Self {
        let state = Arc::new(Mutex::new(Pointer {
            pos: (pos.0 as f64, pos.1 as f64),
            vel: (0.0, 0.0),
            target: (pos.0 as f64, pos.1 as f64),
            last_move: pos,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let state = state.clone();
            let stop = stop.clone();
            thread::spawn(move || ease(x0, y0, tick, state, stop))
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

/// Move the pointer towards its target once per frame.
fn ease(x0: i32, y0: i32, tick: Duration, state: Arc<Mutex<Pointer>>, stop: Arc<AtomicBool>) {
    // parameters
    let omega = 60.0;
    let zeta = 0.90;
    let k = omega * omega;
    let c = 2.0 * zeta * omega;

    let pos_threshold = 0.2;
    let vel_threshold = 10.0;

    let mut last = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        sleep(tick);
        let now = Instant::now();
        let dt = (now - last).min(Duration::from_millis(25)).as_secs_f64();
        last = now;

        let mut p = state.lock().unwrap();

        let dx = p.target.0 - p.pos.0;
        let dy = p.target.1 - p.pos.1;

        if dx.abs() < pos_threshold
            && dy.abs() < pos_threshold
            && p.vel.0.abs() < vel_threshold
            && p.vel.1.abs() < vel_threshold
        {
            p.pos = p.target;
            p.vel = (0.0, 0.0);
            let x = p.pos.0 as i16;
            let y = p.pos.1 as i16;
            if (x, y) != p.last_move {
                unsafe { SetCursorPos(x as i32 + x0, y as i32 + y0) };
                p.last_move = (x, y);
            }
            continue;
        }

        let ax = k * dx - c * p.vel.0;
        let ay = k * dy - c * p.vel.1;

        p.vel.0 += ax * dt;
        p.vel.1 += ay * dt;

        p.pos.0 += p.vel.0 * dt;
        p.pos.1 += p.vel.1 * dt;

        let x = p.pos.0 as i16;
        let y = p.pos.1 as i16;

        // Only send an event if the integer coordinate has actually changed.
        if (x, y) != p.last_move {
            unsafe { SetCursorPos(x as i32 + x0, y as i32 + y0) };
            p.last_move = (x, y);
        }
    }
}

/// One frame, from the current display mode.
fn frame() -> Option<Duration> {
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = size_of::<DEVMODEW>() as u16;
    let ok = unsafe { EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok == 0 {
        return None;
    }
    let hz = mode.dmDisplayFrequency as f64;
    (hz > 1.0).then(|| Duration::from_secs_f64(1.0 / hz))
}

/// Track held keys/buttons so they can be released on disconnect.
fn track<T: PartialEq + Copy>(held: &mut Vec<T>, code: T, pressed: bool) {
    if pressed {
        if !held.contains(&code) {
            held.push(code);
        }
    } else {
        held.retain(|&h| h != code);
    }
}
