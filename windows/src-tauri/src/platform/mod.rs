// Everything that differs between operating systems, behind one set of names.
//
// The rest of the app calls `platform::…` and never touches Win32 or a Linux
// API directly. Each OS file exposes the same functions; the compiler picks one.

use std::path::PathBuf;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use self::linux::*;

// Screen awareness: one picture of the display the person is working on.
#[cfg(windows)]
#[path = "windows_capture.rs"]
pub mod capture;
#[cfg(target_os = "linux")]
#[path = "linux_capture.rs"]
pub mod capture;

// Push-to-talk: the global shortcut and the microphone.
#[cfg(windows)]
#[path = "windows_voice.rs"]
pub mod voice;
#[cfg(target_os = "linux")]
#[path = "linux_voice.rs"]
pub mod voice;

/// Wall-clock time in the user's time zone, for log lines and backup names.
pub struct LocalTime {
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// The user's home directory, where `.claude/settings.json` lives.
pub fn home_dir() -> PathBuf {
    std::env::var_os(HOME_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
