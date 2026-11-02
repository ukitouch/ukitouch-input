//! TODO: Currently only x11 implemented.

use serde::Deserialize;

/// A point on the source screen.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Point {
    pub x: i16,
    pub y: i16,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

/// A synthesized input event.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InputEvent {
    /// Move the pointer to an absolute position on the root screen.
    Move { x: i16, y: i16 },
    /// Press or release a pointer button, at `at` when given, else current position.
    Button {
        at: Option<Point>,
        button: MouseButton,
        pressed: bool,
    },
    /// Wheel notches, positive towards the right/bottom.
    Scroll { at: Option<Point>, dx: i32, dy: i32 },
    /// Press or release a physical key, named by its `KeyboardEvent.code`.
    Key { code: String, pressed: bool },
    /// Type a string.
    Text { text: String },
}

/// Input options from the host.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InputOptions {
    /// Apply spring smooth to the pointer's motion.
    pub smooth: bool,
}

/// The whole backend of input, handles all input events into clicks.
pub trait InputBackend: Send {
    /// Handle a batch of events.
    fn handle(&mut self, events: &[InputEvent]) -> Result<(), String>;

    /// Apply options.
    fn configure(&mut self, _options: &InputOptions) -> Result<(), String> {
        Ok(())
    }

    /// Release everything this backend is holding down.
    fn release(&mut self);
}

/// Build the input backend for this platform.
pub fn create() -> Result<Box<dyn InputBackend>, String> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        return x11::X11Input::new().map(|b| Box::new(b) as Box<dyn InputBackend>);
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    {
        Err(format!("no input backend for {}", std::env::consts::OS))
    }
}

#[cfg(all(unix, not(target_os = "android")))]
pub mod x11;
