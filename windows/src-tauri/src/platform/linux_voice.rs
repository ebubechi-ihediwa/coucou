// Linux: push-to-talk is not built yet. Say so, plainly, instead of pretending: the
// shortcut cannot be registered, the microphone cannot be opened, and the rest of the
// app is untouched.

use std::sync::Arc;

use crate::voice::audio::Recording;
use crate::voice::shortcut::Shortcut;
use crate::voice::{Capture, CaptureError, HotkeyError, HotkeyEvent};

pub const SUPPORTED: bool = false;

pub struct Hotkey;

impl Hotkey {
    pub fn register(
        _shortcut: &Shortcut,
        _on_event: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Hotkey, HotkeyError> {
        Err(HotkeyError::Unsupported)
    }
}

#[derive(Default)]
pub struct Microphone;

impl Capture for Microphone {
    fn start(&mut self) -> Result<(), CaptureError> {
        Err(CaptureError::Unsupported)
    }
    fn stop(&mut self) -> Result<Recording, CaptureError> {
        Err(CaptureError::Unsupported)
    }
    fn cancel(&mut self) {}
}
