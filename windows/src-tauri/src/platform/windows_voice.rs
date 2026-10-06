// Windows: the push-to-talk shortcut and the microphone.
//
// Neither polls. The OS wakes a thread when something happens, and the rest of the
// time those threads (and the microphone) simply are not there.
//
//   * The shortcut is a `RegisterHotKey`, which costs nothing while idle and tells
//     the program nothing about any other key. `RegisterHotKey` says when it is
//     pressed but never when it is let go, so for the length of one push (and no
//     longer) a low-level keyboard hook is installed. It is asked one question,
//     "was one of the keys of the shortcut released?", it never stores or forwards a
//     key code, and it always passes every key on to the next program.
//   * The microphone is `waveIn`: it hands over 16 kHz mono 16-bit audio whatever the
//     device really does, in 100 ms pieces, and signals an event for each, so the
//     recording thread sleeps between them. It opens on `start` and is closed, with
//     its buffers freed, before `stop` or `cancel` returns.

use std::cell::RefCell;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ::windows::Win32::Foundation::{
    CloseHandle, ERROR_HOTKEY_ALREADY_REGISTERED, HANDLE, LPARAM, LRESULT, WPARAM,
};
use ::windows::Win32::Media::Audio::{
    waveInAddBuffer, waveInClose, waveInGetNumDevs, waveInOpen, waveInPrepareHeader, waveInReset,
    waveInStart, waveInStop, waveInUnprepareHeader, CALLBACK_EVENT, HWAVEIN, WAVEFORMATEX, WAVEHDR,
    WAVE_FORMAT_PCM, WAVE_MAPPER, WHDR_DONE,
};
use ::windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThreadId, SetEvent, WaitForMultipleObjects,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_APP,
    WM_HOTKEY, WM_KEYUP, WM_QUIT, WM_SYSKEYUP,
};

use crate::voice::audio::{self, Recording};
use crate::voice::shortcut::Shortcut;
use crate::voice::{Capture, CaptureError, HotkeyError, HotkeyEvent};

pub const SUPPORTED: bool = true;

// ── The shortcut ──────────────────────────────────────────────────────────────

const HOTKEY_ID: i32 = 0x436F; // "Co"
/// Posted to the shortcut's own thread.
const WM_RELEASED: u32 = WM_APP + 1;
const WM_STOP_WATCHING: u32 = WM_APP + 2;

thread_local! {
    /// The keys whose release ends the push. Read by the hook, on the same thread.
    static WATCHED: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// A registered shortcut. Dropping it unregisters the shortcut and ends its thread.
pub struct Hotkey {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl Hotkey {
    /// Registers `shortcut` for the whole desktop. `on_event` runs on the shortcut's
    /// own thread, so it must only hand the event on.
    pub fn register(
        shortcut: &Shortcut,
        on_event: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ) -> Result<Hotkey, HotkeyError> {
        let shortcut = *shortcut;
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("coucou-hotkey".into())
            .spawn(move || shortcut_thread(shortcut, on_event, ready_tx))
            .map_err(|_| HotkeyError::Failed)?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(Hotkey {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err(HotkeyError::Failed),
        }
    }

    /// The push ended some other way (cancel, time limit): stop watching for the
    /// release, so no keyboard hook outlives the recording.
    pub fn stop_watching(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_STOP_WATCHING, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for Hotkey {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn shortcut_thread(
    shortcut: Shortcut,
    on_event: Arc<dyn Fn(HotkeyEvent) + Send + Sync>,
    ready: mpsc::Sender<Result<u32, HotkeyError>>,
) {
    unsafe {
        // Make sure this thread has a message queue before anyone posts to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_APP, WM_APP, PM_NOREMOVE);
        let thread_id = GetCurrentThreadId();

        let flags = HOT_KEY_MODIFIERS(shortcut.modifier_flags()) | MOD_NOREPEAT;
        if let Err(error) = RegisterHotKey(None, HOTKEY_ID, flags, shortcut.virtual_key()) {
            let why = if error.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult() {
                HotkeyError::InUse
            } else {
                HotkeyError::Failed
            };
            let _ = ready.send(Err(why));
            return;
        }
        let _ = ready.send(Ok(thread_id));

        let mut hook: Option<HHOOK> = None;
        let unhook = |hook: &mut Option<HHOOK>| {
            if let Some(h) = hook.take() {
                let _ = UnhookWindowsHookEx(h);
            }
            WATCHED.with(|w| w.borrow_mut().clear());
        };

        // 0 is WM_QUIT; -1 is an error. Either ends the thread.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            match msg.message {
                WM_HOTKEY if msg.wParam.0 as i32 == HOTKEY_ID => {
                    if hook.is_none() {
                        WATCHED.with(|w| *w.borrow_mut() = shortcut.release_keys());
                        hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(release_hook), None, 0).ok();
                        // The key may have come up before the hook was in place; one
                        // look, not a loop.
                        let still_down =
                            GetAsyncKeyState(shortcut.virtual_key() as i32) as u16 & 0x8000 != 0;
                        if hook.is_none() || !still_down {
                            let _ =
                                PostThreadMessageW(thread_id, WM_RELEASED, WPARAM(0), LPARAM(0));
                        }
                    }
                    on_event(HotkeyEvent::Pressed);
                }
                WM_RELEASED => {
                    if hook.is_some() {
                        unhook(&mut hook);
                        on_event(HotkeyEvent::Released);
                    }
                }
                WM_STOP_WATCHING => unhook(&mut hook),
                _ => {}
            }
        }
        unhook(&mut hook);
        let _ = UnregisterHotKey(None, HOTKEY_ID);
    }
}

/// Runs for every key while a push is in progress. Only asks whether the key that
/// came up is one of the shortcut's; passes every key on untouched.
unsafe extern "system" fn release_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && matches!(wparam.0 as u32, WM_KEYUP | WM_SYSKEYUP) {
        let key = unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode };
        if WATCHED.with(|w| w.borrow().contains(&key)) {
            unsafe {
                let _ = PostThreadMessageW(GetCurrentThreadId(), WM_RELEASED, WPARAM(0), LPARAM(0));
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ── The microphone ────────────────────────────────────────────────────────────

/// How much each buffer holds: short, so the end of a push loses almost nothing.
const BUFFER_MS: u32 = 100;
const BUFFERS: usize = 4;
const BUFFER_SAMPLES: usize = (audio::SAMPLE_RATE * BUFFER_MS / 1000) as usize;
/// A microphone that delivers nothing for this long has stopped (unplugged, or the
/// driver gave up). This is the only wait with a timeout, and only while recording.
const SILENT_DEVICE: Duration = Duration::from_secs(2);

const MMSYSERR_ALLOCATED: u32 = 4;

#[derive(Default)]
pub struct Microphone {
    running: Option<Running>,
}

struct Running {
    /// Raw handle of the event that tells the recording thread to finish.
    stop: isize,
    thread: JoinHandle<Result<Recording, CaptureError>>,
}

impl Microphone {
    /// Ends the recording thread and returns what it recorded. The microphone is
    /// closed when this returns.
    fn finish(&mut self) -> Result<Recording, CaptureError> {
        let Some(running) = self.running.take() else {
            return Err(CaptureError::Failed);
        };
        unsafe {
            let stop = HANDLE(running.stop as *mut core::ffi::c_void);
            let _ = SetEvent(stop);
            let result = running.thread.join().unwrap_or(Err(CaptureError::Failed));
            let _ = CloseHandle(stop);
            result
        }
    }
}

impl Capture for Microphone {
    fn start(&mut self) -> Result<(), CaptureError> {
        if self.running.is_some() {
            return Err(CaptureError::Failed);
        }
        unsafe {
            // A manual-reset event: once the stop is signalled it stays signalled.
            let stop = CreateEventW(None, true, false, None).map_err(|_| CaptureError::Failed)?;
            let raw = stop.0 as isize;
            let (ready_tx, ready_rx) = mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("coucou-mic".into())
                .spawn(move || record(raw, ready_tx))
                .map_err(|_| {
                    let _ = CloseHandle(stop);
                    CaptureError::Failed
                })?;
            match ready_rx.recv() {
                Ok(Ok(())) => {
                    self.running = Some(Running { stop: raw, thread });
                    Ok(())
                }
                Ok(Err(error)) => {
                    let _ = thread.join();
                    let _ = CloseHandle(stop);
                    Err(error)
                }
                Err(_) => {
                    let _ = thread.join();
                    let _ = CloseHandle(stop);
                    Err(CaptureError::Failed)
                }
            }
        }
    }

    fn stop(&mut self) -> Result<Recording, CaptureError> {
        self.finish()
    }

    fn cancel(&mut self) {
        // Whatever was recorded is dropped right here.
        let _ = self.finish();
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The recording thread: opens the device, records until `stop` is signalled or the
/// limit is reached, closes the device and returns the audio.
fn record(
    stop_raw: isize,
    ready: mpsc::Sender<Result<(), CaptureError>>,
) -> Result<Recording, CaptureError> {
    unsafe {
        let fail = |error: CaptureError| {
            let _ = ready.send(Err(error.clone()));
            Err(error)
        };
        if waveInGetNumDevs() == 0 {
            return fail(CaptureError::NoDevice);
        }
        let Ok(data_event) = CreateEventW(None, false, false, None) else {
            return fail(CaptureError::Failed);
        };
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 1,
            nSamplesPerSec: audio::SAMPLE_RATE,
            nAvgBytesPerSec: audio::SAMPLE_RATE * 2,
            nBlockAlign: 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut device = HWAVEIN::default();
        let opened = waveInOpen(
            Some(&mut device),
            WAVE_MAPPER,
            &format,
            Some(data_event.0 as usize),
            None,
            CALLBACK_EVENT,
        );
        if opened != 0 {
            let _ = CloseHandle(data_event);
            return fail(match opened {
                MMSYSERR_ALLOCATED => CaptureError::InUse,
                // No driver, or no such device.
                2 | 6 => CaptureError::NoDevice,
                _ => CaptureError::Failed,
            });
        }

        // The audio lands in these; neither may move while the device holds them.
        let mut storage: Vec<Vec<i16>> = (0..BUFFERS).map(|_| vec![0i16; BUFFER_SAMPLES]).collect();
        let mut headers: Vec<Box<WAVEHDR>> = Vec::with_capacity(BUFFERS);
        let header_size = size_of::<WAVEHDR>() as u32;
        let mut queued = true;
        for buffer in storage.iter_mut() {
            let mut header = Box::new(WAVEHDR {
                lpData: ::windows::core::PSTR(buffer.as_mut_ptr().cast()),
                dwBufferLength: (BUFFER_SAMPLES * 2) as u32,
                ..Default::default()
            });
            queued &= waveInPrepareHeader(device, &mut *header, header_size) == 0
                && waveInAddBuffer(device, &mut *header, header_size) == 0;
            headers.push(header);
        }
        let release = |headers: &mut Vec<Box<WAVEHDR>>| {
            let _ = waveInReset(device);
            for header in headers.iter_mut() {
                let _ = waveInUnprepareHeader(device, &mut **header, header_size);
            }
            let _ = waveInClose(device);
            let _ = CloseHandle(data_event);
        };
        if !queued || waveInStart(device) != 0 {
            release(&mut headers);
            return fail(CaptureError::Failed);
        }
        let _ = ready.send(Ok(()));

        let stop = HANDLE(stop_raw as *mut core::ffi::c_void);
        let mut samples: Vec<i16> = Vec::with_capacity(audio::SAMPLE_RATE as usize * 4);
        let mut hit_limit = false;
        let mut interrupted = false;
        let take = |headers: &mut Vec<Box<WAVEHDR>>, samples: &mut Vec<i16>, requeue: bool| {
            for header in headers.iter_mut() {
                if header.dwFlags & WHDR_DONE == 0 {
                    continue;
                }
                let count = header.dwBytesRecorded as usize / 2;
                let data = std::slice::from_raw_parts(header.lpData.0 as *const i16, count);
                let room = audio::MAX_SAMPLES.saturating_sub(samples.len());
                samples.extend_from_slice(&data[..count.min(room)]);
                header.dwBytesRecorded = 0;
                if requeue {
                    let _ = waveInAddBuffer(device, &mut **header, header_size);
                }
            }
        };
        let handles = [stop, data_event];
        loop {
            let woke = WaitForMultipleObjects(&handles, false, SILENT_DEVICE.as_millis() as u32);
            match woke.0 {
                // The stop event: the person let go, or something cancelled.
                0 => break,
                // A buffer filled.
                1 => {
                    take(&mut headers, &mut samples, true);
                    if samples.len() >= audio::MAX_SAMPLES {
                        hit_limit = true;
                        break;
                    }
                }
                // Nothing for two seconds: the device has gone.
                _ => {
                    interrupted = true;
                    break;
                }
            }
        }

        let _ = waveInStop(device);
        let _ = waveInReset(device); // hands back the buffer that was part filled
        take(&mut headers, &mut samples, false);
        release(&mut headers);
        drop(storage);

        if interrupted {
            return Err(CaptureError::Interrupted);
        }
        Ok(Recording { samples, hit_limit })
    }
}
