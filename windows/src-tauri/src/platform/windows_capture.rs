// Windows: one picture of the display the person is working on.
//
// GDI, once, into memory. There is no capture session, no Desktop Duplication stream,
// no timer: a call copies the pixels of one monitor and returns, and every handle it
// opened is closed before it does (a guard releases each one, on every path out).
//
// "The display the person is working on" is the monitor of the foreground window, which
// is where the island's own chat sits when they type and the app they were in when they
// spoke to it. With no foreground window it is the monitor under the cursor, and failing
// that the primary one. Only that one monitor is captured, never the whole desktop.
//
// What GDI cannot see stays unseen: windows that ask Windows to keep them out of
// captures (some video players, password managers, DRM content) come out black.

use std::ffi::c_void;

use ::windows::Win32::Foundation::{E_ACCESSDENIED, POINT};
use ::windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY, SRCCOPY,
};
use ::windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};

use crate::screen::{RawFrame, ScreenError, LIMITS};

/// Runs a closure when dropped: how each handle is given back, whatever happens.
struct Release<F: FnMut()>(F);

impl<F: FnMut()> Drop for Release<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}

/// The rectangle (left, top, width, height) of the monitor the person is working on.
unsafe fn active_monitor() -> Result<(i32, i32, i32, i32), ScreenError> {
    unsafe {
        let window = GetForegroundWindow();
        let monitor = if window.0.is_null() {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return Err(ScreenError::Unavailable);
        }
        let r = info.rcMonitor;
        Ok((r.left, r.top, r.right - r.left, r.bottom - r.top))
    }
}

/// Copies the active monitor's pixels into memory.
pub fn grab() -> Result<RawFrame, ScreenError> {
    unsafe {
        let (left, top, width, height) = active_monitor()?;
        if width <= 0 || height <= 0 {
            return Err(ScreenError::Unavailable);
        }
        // Refused before anything is allocated.
        if width as u64 * height as u64 > LIMITS.max_source_pixels {
            return Err(ScreenError::TooLarge);
        }

        let screen = GetDC(None);
        if screen.is_invalid() {
            return Err(ScreenError::Unavailable);
        }
        let _screen = Release(|| {
            ReleaseDC(None, screen);
        });
        let memory: HDC = CreateCompatibleDC(Some(screen));
        if memory.is_invalid() {
            return Err(ScreenError::CaptureFailed);
        }
        let _memory = Release(|| {
            let _ = DeleteDC(memory);
        });

        // A top-down 32-bit bitmap whose pixels we can read straight from memory.
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(memory), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|_| ScreenError::CaptureFailed)?;
        let _bitmap = Release(|| {
            let _ = DeleteObject(bitmap.into());
        });
        if bits.is_null() {
            return Err(ScreenError::CaptureFailed);
        }

        let previous = SelectObject(memory, bitmap.into());
        // CAPTUREBLT includes layered windows (the island, tooltips), as they look.
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            left,
            top,
            SRCCOPY | CAPTUREBLT,
        );
        SelectObject(memory, previous);
        // Windows refuses to copy from the secure desktop (a UAC prompt, the lock screen):
        // that is "not allowed", not a fault.
        copied.map_err(|e| match e.code() == E_ACCESSDENIED {
            true => ScreenError::PermissionDenied,
            false => ScreenError::CaptureFailed,
        })?;

        let bytes = width as usize * height as usize * 4;
        let bgra = std::slice::from_raw_parts(bits as *const u8, bytes).to_vec();
        Ok(RawFrame {
            width: width as u32,
            height: height as u32,
            bgra,
        })
    }
}
