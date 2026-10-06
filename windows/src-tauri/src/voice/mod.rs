// Push-to-talk voice input.
//
// Voice is an input method, not a second assistant. All this module does is turn a
// held shortcut into the same thing a person could have typed:
//
//     shortcut down → microphone on → shortcut up → microphone off
//         → speech to text → (optional "Hey Coucou" removed) → the text
//
// and hand the text to the island, which submits it through the one chat path that
// typed messages use. Nothing here calls the model, proposes an action or runs one:
// a spoken "delete everything" is exactly as powerful as a typed one, which is to
// say it is untrusted text that the assistant runtime, the policy and the approval
// card judge as always.
//
// The microphone is only ever opened by `begin`, which only a deliberate act reaches
// (the shortcut, or the button), and it is closed by whichever comes first: release,
// the time limit, cancel, an error, shutdown. While idle there is no stream, no
// buffer, no thread and no timer; only the OS delivers the shortcut.

pub mod audio;
pub mod shortcut;
pub mod transcribe;
pub mod wake;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::async_runtime::JoinHandle;

use audio::{encode_wav, judge, Heard, Recording};
use transcribe::{Provider, Transcriber};

pub const COULDNT_CATCH: &str = "I didn't catch that.";
/// The push ended almost at once. The usual cause is pressing the shortcut like a
/// button, so the hint says to hold it; for the microphone button, to speak first.
pub const TAPPED_SHORTCUT: &str = "That was a tap. Hold the shortcut while you speak, then let go.";
pub const TAPPED_BUTTON: &str = "That was too short. Speak first, then press Done.";
pub const STILL_WORKING: &str = "Coucou is still working on your last request.";
const BLOCKED: &str = "Microphone permission is required, or the microphone is muted. \
Check Settings → Privacy → Microphone.";

// ── The microphone ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    NoDevice,
    /// Another program has the microphone to itself.
    InUse,
    Failed,
    /// It was working and then stopped: unplugged, or the driver gave up.
    Interrupted,
    /// Only the Linux stub builds it.
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

impl CaptureError {
    pub fn message(&self) -> &'static str {
        match self {
            CaptureError::NoDevice => "No microphone was detected.",
            CaptureError::InUse => "The microphone is being used by another program.",
            CaptureError::Failed => "I couldn't start the microphone.",
            CaptureError::Interrupted => {
                "The microphone stopped working. Check that it is still connected."
            }
            CaptureError::Unsupported => "Voice isn't available on this system yet.",
        }
    }
}

/// A microphone that records while held open. Whatever the implementation, `stop` and
/// `cancel` must leave the device closed and no audio kept behind.
pub trait Capture: Send + 'static {
    /// Opens the device and starts recording. Fails without leaving anything open.
    fn start(&mut self) -> Result<(), CaptureError>;
    /// Stops, closes the device and hands over everything recorded. Recording never
    /// grows past `audio::MAX_SAMPLES`; a recording cut there says so.
    fn stop(&mut self) -> Result<Recording, CaptureError>;
    /// Stops, closes the device and throws the audio away.
    fn cancel(&mut self);
}

// ── The shortcut ──────────────────────────────────────────────────────────────

/// What the OS reports about the push-to-talk shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyError {
    /// Another program owns that shortcut.
    InUse,
    /// The OS refused for some other reason.
    Failed,
    /// This platform has no push-to-talk yet. Only the Linux stub builds it.
    #[cfg_attr(windows, allow(dead_code))]
    Unsupported,
}

impl HotkeyError {
    pub fn message(&self) -> &'static str {
        match self {
            HotkeyError::InUse => {
                "Another program already uses that shortcut. Pick a different one."
            }
            HotkeyError::Failed => {
                "Windows wouldn't let Coucou register that shortcut. Pick a different one."
            }
            HotkeyError::Unsupported => "Push-to-talk isn't available on this system yet.",
        }
    }
}

// ── What the island is told ───────────────────────────────────────────────────

/// Sent to the page as the `voice` event. The page shows it; it decides nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Event {
    /// Cancelled: nothing was said to anyone.
    Idle,
    /// The microphone is on.
    Listening,
    /// The microphone is off; the recording is being turned into text.
    Transcribing {
        #[serde(rename = "limitReached")]
        limit_reached: bool,
    },
    /// What was said, ready to be submitted exactly like a typed message.
    Transcript {
        text: String,
    },
    /// Nothing to act on, and nothing went wrong ("I didn't catch that").
    Notice {
        message: String,
    },
    Error {
        message: String,
    },
}

pub trait Sink: Send + Sync + 'static {
    fn publish(&self, event: Event);
}

// ── Settings the controller needs ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub wake_phrase_enabled: bool,
    pub wake_phrase: String,
    /// Which speech service turns the recording into text.
    pub provider: Provider,
    pub max_recording: Duration,
}

impl Config {
    pub fn new(enabled: bool, wake_phrase_enabled: bool, wake_phrase: String) -> Config {
        Config {
            enabled,
            wake_phrase_enabled,
            wake_phrase,
            provider: transcribe::DEFAULT_PROVIDER,
            max_recording: Duration::from_secs(u64::from(audio::MAX_RECORDING_SECS)),
        }
    }
}

impl Config {
    pub fn with_provider(mut self, provider: Provider) -> Config {
        self.provider = provider;
        self
    }
}

/// The text a transcript becomes before it is submitted. With the phrase off it is
/// the transcript, trimmed, and nothing else.
pub fn normalize(transcript: &str, config: &Config) -> String {
    if config.wake_phrase_enabled {
        wake::strip_leading_phrase(transcript, &config.wake_phrase)
    } else {
        transcript.trim().to_string()
    }
}

// ── The controller ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Listening,
    Transcribing,
}

/// Why `begin` did not start recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Voice is switched off in settings.
    Disabled,
    /// Something rules it out for now (the assistant is working, no speech key is
    /// saved): the person is told, and the microphone is never opened.
    Blocked,
    /// Already recording or transcribing: a second push is ignored, not stacked.
    Already,
    /// The microphone would not start; the person is told why.
    Capture,
}

struct State {
    phase: Phase,
    /// Counts pushes. Anything that arrives for an older one (a timer, a late
    /// answer) is ignored, so a cancelled push can never deliver text.
    session: u64,
    timer: Option<JoinHandle<()>>,
    request: Option<JoinHandle<()>>,
}

pub struct Voice {
    capture: Mutex<Box<dyn Capture>>,
    state: Mutex<State>,
    config: Mutex<Config>,
    transcriber: Arc<dyn Transcriber>,
    sink: Arc<dyn Sink>,
    /// Why a push cannot start right now, if it cannot: the assistant is thinking or
    /// executing (the same refusal a typed message gets), or there is no key for the
    /// speech service. Checked before the microphone is touched.
    gate: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Voice {
    pub fn new(
        capture: Box<dyn Capture>,
        transcriber: Arc<dyn Transcriber>,
        sink: Arc<dyn Sink>,
        gate: Box<dyn Fn() -> Option<String> + Send + Sync>,
        config: Config,
    ) -> Arc<Voice> {
        Arc::new(Voice {
            capture: Mutex::new(capture),
            state: Mutex::new(State {
                phase: Phase::Idle,
                session: 0,
                timer: None,
                request: None,
            }),
            config: Mutex::new(config),
            transcriber,
            sink,
            gate,
        })
    }

    pub fn phase(&self) -> Phase {
        self.state.lock().unwrap().phase
    }

    /// New settings. Turning voice off ends anything in progress.
    pub fn configure(self: &Arc<Self>, config: Config) {
        let enabled = config.enabled;
        *self.config.lock().unwrap() = config;
        if !enabled {
            self.cancel();
        }
    }

    /// Starts a push: the microphone opens. Only the shortcut or the button get here.
    pub fn begin(self: &Arc<Self>) -> Result<u64, Refused> {
        let max = {
            let config = self.config.lock().unwrap();
            if !config.enabled {
                return Err(Refused::Disabled);
            }
            config.max_recording
        };
        if let Some(message) = (self.gate)() {
            self.sink.publish(Event::Notice { message });
            return Err(Refused::Blocked);
        }
        // Every change of phase is announced while the state is still held, so the
        // page hears things in the order they happened (a very fast answer must not
        // overtake "Transcribing").
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Idle {
            return Err(Refused::Already);
        }
        if let Err(err) = self.capture.lock().unwrap().start() {
            self.sink.publish(Event::Error {
                message: err.message().into(),
            });
            return Err(Refused::Capture);
        }
        state.phase = Phase::Listening;
        state.session += 1;
        let session = state.session;
        // The last line of defence against a stuck key or a lost release: whatever
        // happens, the microphone is closed when this fires.
        let this = Arc::clone(self);
        state.timer = Some(tauri::async_runtime::spawn(async move {
            tokio::time::sleep(max).await;
            this.finish_recording(Some(session), true);
        }));
        self.sink.publish(Event::Listening);
        Ok(session)
    }

    /// The push ended (the button pressed again).
    pub fn end(self: &Arc<Self>) {
        self.finish_recording(None, false);
    }

    /// The shortcut was released: ends the push that press started, if it is still
    /// the one running.
    pub fn end_session(self: &Arc<Self>, session: u64) {
        self.finish_recording(Some(session), false);
    }

    /// Listening → transcribing. `session` is `Some` for the timer, which must not
    /// end a push that is not its own.
    fn finish_recording(self: &Arc<Self>, session: Option<u64>, limit_reached: bool) {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Listening || session.is_some_and(|s| s != state.session) {
            return;
        }
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        // The microphone is closed here, before anything else happens.
        let stopped = self.capture.lock().unwrap().stop();
        let mut recording = match stopped {
            Ok(recording) => recording,
            Err(err) => {
                state.phase = Phase::Idle;
                self.sink.publish(Event::Error {
                    message: err.message().into(),
                });
                return;
            }
        };
        recording.hit_limit |= limit_reached;
        let limit_reached = recording.hit_limit;

        let problem = match judge(&recording) {
            Heard::Speech => None,
            // `session` is only given when the shortcut was let go (or the timer fired);
            // the microphone button's second press has none.
            Heard::TooShort => Some(Event::Notice {
                message: if session.is_some() {
                    TAPPED_SHORTCUT
                } else {
                    TAPPED_BUTTON
                }
                .into(),
            }),
            Heard::Silence => Some(Event::Notice {
                message: COULDNT_CATCH.into(),
            }),
            Heard::Blocked => Some(Event::Error {
                message: BLOCKED.into(),
            }),
        };
        if let Some(event) = problem {
            state.phase = Phase::Idle;
            self.sink.publish(event);
            return;
        }

        state.phase = Phase::Transcribing;
        let id = state.session;
        let wav = encode_wav(&recording.samples);
        drop(recording);
        // Said before the request exists, and with the state held: the answer cannot
        // get in ahead of it.
        self.sink.publish(Event::Transcribing { limit_reached });
        let this = Arc::clone(self);
        let transcriber = Arc::clone(&self.transcriber);
        let provider = self.config.lock().unwrap().provider;
        state.request = Some(tauri::async_runtime::spawn(async move {
            let result = transcriber.transcribe(provider, wav).await;
            this.deliver(id, result);
        }));
    }

    /// The speech service answered. A cancelled push has a newer session by now, so
    /// its answer goes nowhere.
    fn deliver(&self, session: u64, result: Result<String, String>) {
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::Transcribing || state.session != session {
            return;
        }
        state.phase = Phase::Idle;
        state.request = None;
        let event = match result {
            Err(message) => Event::Error { message },
            Ok(transcript) => {
                let text = normalize(&transcript, &self.config.lock().unwrap());
                if text.is_empty() {
                    Event::Notice {
                        message: COULDNT_CATCH.into(),
                    }
                } else {
                    Event::Transcript { text }
                }
            }
        };
        self.sink.publish(event);
    }

    /// Stops whatever voice is doing: the recording (nothing is kept) or the request
    /// to the speech service. Returns whether there was anything to stop.
    pub fn cancel(self: &Arc<Self>) -> bool {
        self.stop_everything(true)
    }

    /// The app is closing: the microphone is closed, silently.
    pub fn shutdown(self: &Arc<Self>) {
        self.stop_everything(false);
    }

    fn stop_everything(&self, announce: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        let was = state.phase != Phase::Idle;
        state.session += 1;
        state.phase = Phase::Idle;
        if let Some(timer) = state.timer.take() {
            timer.abort();
        }
        if let Some(request) = state.request.take() {
            request.abort();
        }
        // Always, not only when listening: closing a device that is closed is harmless,
        // and leaving one open is the one thing that must never happen.
        self.capture.lock().unwrap().cancel();
        if was && announce {
            self.sink.publish(Event::Idle);
        }
        was
    }
}

#[cfg(test)]
mod tests;
