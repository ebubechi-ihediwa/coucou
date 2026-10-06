// The push-to-talk flow end to end, with a fake microphone, a fake speech service
// and a channel standing in for the island. No audio device or network is touched.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::audio::{self, Recording, MAX_SAMPLES, SAMPLE_RATE};
use super::transcribe::{Provider, TranscribeFuture, Transcriber};
use super::*;

// ── Fakes ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Mic {
    open: bool,
    started: usize,
    stopped: usize,
    cancelled: usize,
    /// What is held while recording; handed over or dropped when it ends.
    held: Vec<i16>,
    start_error: Option<CaptureError>,
    stop_result: Option<Result<Recording, CaptureError>>,
}

#[derive(Clone, Default)]
struct FakeMic(Arc<Mutex<Mic>>);

impl FakeMic {
    fn with<R>(&self, f: impl FnOnce(&mut Mic) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }
}

impl Capture for FakeMic {
    fn start(&mut self) -> Result<(), CaptureError> {
        self.with(|m| {
            if let Some(e) = m.start_error.clone() {
                return Err(e);
            }
            m.open = true;
            m.started += 1;
            m.held = vec![1; 100];
            Ok(())
        })
    }
    fn stop(&mut self) -> Result<Recording, CaptureError> {
        self.with(|m| {
            m.open = false;
            m.stopped += 1;
            let held = std::mem::take(&mut m.held);
            m.stop_result.take().unwrap_or_else(|| {
                Ok(Recording {
                    samples: speech(1000).samples.into_iter().chain(held).collect(),
                    hit_limit: false,
                })
            })
        })
    }
    fn cancel(&mut self) {
        self.with(|m| {
            m.open = false;
            m.cancelled += 1;
            m.held.clear();
        })
    }
}

#[derive(Default)]
struct Speech {
    reply: Option<Result<String, String>>,
    hang: bool,
    delay: Duration,
    calls: usize,
    wav_sizes: Vec<usize>,
    providers: Vec<Provider>,
}

#[derive(Clone, Default)]
struct FakeSpeech(Arc<Mutex<Speech>>);

impl FakeSpeech {
    fn says(&self, text: &str) {
        self.0.lock().unwrap().reply = Some(Ok(text.into()));
    }
    fn fails(&self, message: &str) {
        self.0.lock().unwrap().reply = Some(Err(message.into()));
    }
    fn hangs(&self) {
        self.0.lock().unwrap().hang = true;
    }
    fn calls(&self) -> usize {
        self.0.lock().unwrap().calls
    }
}

impl Transcriber for FakeSpeech {
    fn transcribe(&self, provider: Provider, wav: Vec<u8>) -> TranscribeFuture<'_> {
        let inner = self.0.clone();
        Box::pin(async move {
            let (hang, delay, reply) = {
                let mut s = inner.lock().unwrap();
                s.calls += 1;
                s.wav_sizes.push(wav.len());
                s.providers.push(provider);
                (
                    s.hang,
                    s.delay,
                    s.reply.clone().unwrap_or(Ok("hello".into())),
                )
            };
            if hang {
                std::future::pending::<()>().await;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            reply
        })
    }
}

struct ChannelSink(Mutex<Sender<Event>>);

impl Sink for ChannelSink {
    fn publish(&self, event: Event) {
        let _ = self.0.lock().unwrap().send(event);
    }
}

struct Rig {
    voice: Arc<Voice>,
    mic: FakeMic,
    speech: FakeSpeech,
    events: Receiver<Event>,
    /// What the gate says (None = clear to start).
    gate: Arc<Mutex<Option<String>>>,
}

impl Rig {
    fn new() -> Rig {
        Rig::with(Config::new(true, true, wake::DEFAULT_PHRASE.into()))
    }

    fn with(config: Config) -> Rig {
        let (tx, events) = channel();
        let mic = FakeMic::default();
        let speech = FakeSpeech::default();
        let gate: Arc<Mutex<Option<String>>> = Arc::default();
        let seen = gate.clone();
        let voice = Voice::new(
            Box::new(mic.clone()),
            Arc::new(speech.clone()),
            Arc::new(ChannelSink(Mutex::new(tx))),
            Box::new(move || seen.lock().unwrap().clone()),
            config,
        );
        Rig {
            voice,
            mic,
            speech,
            events,
            gate,
        }
    }

    fn next(&self) -> Event {
        self.events
            .recv_timeout(Duration::from_secs(3))
            .expect("an event")
    }

    /// Nothing more arrives (for a while).
    fn quiet(&self) {
        if let Ok(event) = self.events.recv_timeout(Duration::from_millis(250)) {
            panic!("unexpected event: {event:?}");
        }
    }

    /// Hold, then release, and wait for the end of the push.
    fn push(&self) -> Event {
        self.voice.begin().unwrap();
        assert_eq!(self.next(), Event::Listening);
        self.voice.end();
        assert_eq!(
            self.next(),
            Event::Transcribing {
                limit_reached: false
            }
        );
        self.next()
    }
}

/// `ms` of loud signal.
fn speech(ms: u32) -> Recording {
    Recording {
        samples: (0..SAMPLE_RATE * ms / 1000)
            .map(|i| if i % 2 == 0 { 9000 } else { -9000 })
            .collect(),
        hit_limit: false,
    }
}

// ── The happy path ────────────────────────────────────────────────────────────

#[test]
fn hold_speak_release_gives_the_text_without_the_wake_phrase() {
    let rig = Rig::new();
    rig.speech.says("Hey Coucou, open Notepad.");

    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    assert_eq!(rig.voice.phase(), Phase::Listening);
    assert!(
        rig.mic.with(|m| m.open),
        "the microphone is on while the key is held"
    );

    rig.voice.end();
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: false
        }
    );
    // The microphone is already off while the text is being worked out.
    assert!(!rig.mic.with(|m| m.open));

    assert_eq!(
        rig.next(),
        Event::Transcript {
            text: "Open Notepad.".into()
        }
    );
    assert_eq!(rig.voice.phase(), Phase::Idle);
    rig.quiet();
    assert_eq!(rig.mic.with(|m| (m.started, m.stopped)), (1, 1));
    assert_eq!(rig.speech.calls(), 1);
}

#[test]
fn events_arrive_in_order_even_when_the_answer_is_instant() {
    // A speech service that answers at once must not get its text in ahead of
    // "Transcribing": the page would be left on the transcribing card.
    let rig = Rig::new();
    rig.speech.says("one");
    for _ in 0..300 {
        assert_eq!(rig.push(), Event::Transcript { text: "one".into() });
    }
}

#[test]
fn with_the_phrase_switched_off_the_text_arrives_whole() {
    let rig = Rig::with(Config::new(true, false, wake::DEFAULT_PHRASE.into()));
    rig.speech.says("  Hey Coucou, open Notepad.  ");
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "Hey Coucou, open Notepad.".into()
        }
    );
}

#[test]
fn a_mention_of_the_name_inside_a_sentence_is_never_stripped() {
    let rig = Rig::new();
    rig.speech.says("I was talking about Coucou yesterday.");
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "I was talking about Coucou yesterday.".into()
        }
    );
}

#[test]
fn a_second_push_works_after_the_first() {
    let rig = Rig::new();
    rig.speech.says("one");
    assert_eq!(rig.push(), Event::Transcript { text: "one".into() });
    rig.speech.says("two");
    assert_eq!(rig.push(), Event::Transcript { text: "two".into() });
    assert_eq!(rig.mic.with(|m| m.started), 2);
}

// ── Not started ───────────────────────────────────────────────────────────────

#[test]
fn with_voice_off_nothing_starts_and_nothing_is_said() {
    let rig = Rig::with(Config::new(false, true, wake::DEFAULT_PHRASE.into()));
    assert_eq!(rig.voice.begin(), Err(Refused::Disabled));
    assert_eq!(rig.mic.with(|m| m.started), 0);
    rig.quiet();
}

#[test]
fn a_second_press_while_recording_is_ignored_not_stacked() {
    let rig = Rig::new();
    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    assert_eq!(rig.voice.begin(), Err(Refused::Already));
    assert_eq!(
        rig.mic.with(|m| m.started),
        1,
        "one microphone, one recording"
    );
    rig.quiet();
    // And while transcribing.
    rig.speech.hangs();
    rig.voice.end();
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: false
        }
    );
    assert_eq!(rig.voice.begin(), Err(Refused::Already));
    assert_eq!(rig.mic.with(|m| m.started), 1);
}

#[test]
fn while_the_assistant_is_busy_a_push_is_refused_with_a_reason() {
    let rig = Rig::new();
    *rig.gate.lock().unwrap() = Some(STILL_WORKING.into());
    assert_eq!(rig.voice.begin(), Err(Refused::Blocked));
    assert_eq!(
        rig.next(),
        Event::Notice {
            message: STILL_WORKING.into()
        }
    );
    assert_eq!(
        rig.mic.with(|m| m.started),
        0,
        "the microphone was never opened"
    );
    // Once it is free again, the same shortcut works.
    *rig.gate.lock().unwrap() = None;
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "hello".into()
        }
    );
}

#[test]
fn the_chosen_speech_service_is_the_one_asked_and_a_change_applies_to_the_next_push() {
    let rig = Rig::with(
        Config::new(true, true, wake::DEFAULT_PHRASE.into()).with_provider(Provider::Groq),
    );
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "hello".into()
        }
    );
    rig.voice.configure(
        Config::new(true, true, wake::DEFAULT_PHRASE.into()).with_provider(Provider::OpenAi),
    );
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "hello".into()
        }
    );
    let asked = rig.speech.0.lock().unwrap().providers.clone();
    assert_eq!(asked, vec![Provider::Groq, Provider::OpenAi]);
}

#[test]
fn the_default_speech_service_is_openai() {
    assert_eq!(
        Config::new(true, true, "x".into()).provider,
        Provider::OpenAi
    );
}

#[test]
fn without_a_speech_key_the_microphone_is_never_opened_and_the_person_is_told() {
    let rig = Rig::new();
    *rig.gate.lock().unwrap() = Some(Provider::OpenAi.no_key_message());
    assert_eq!(rig.voice.begin(), Err(Refused::Blocked));
    assert_eq!(
        rig.next(),
        Event::Notice {
            message: "Add your OpenAI key in Settings to use voice.".into()
        }
    );
    assert_eq!(
        rig.mic.with(|m| m.started),
        0,
        "nothing was recorded that could not be transcribed"
    );
    assert_eq!(rig.speech.calls(), 0);
}

#[test]
fn a_release_with_no_push_in_progress_does_nothing() {
    let rig = Rig::new();
    rig.voice.end();
    rig.quiet();
    assert_eq!(rig.mic.with(|m| (m.started, m.stopped)), (0, 0));
}

// ── Microphone problems ───────────────────────────────────────────────────────

#[test]
fn a_microphone_that_will_not_start_says_why_and_the_next_try_works() {
    for (error, message) in [
        (CaptureError::NoDevice, "No microphone was detected."),
        (
            CaptureError::InUse,
            "The microphone is being used by another program.",
        ),
        (CaptureError::Failed, "I couldn't start the microphone."),
        (
            CaptureError::Unsupported,
            "Voice isn't available on this system yet.",
        ),
    ] {
        let rig = Rig::new();
        rig.mic.with(|m| m.start_error = Some(error));
        assert_eq!(rig.voice.begin(), Err(Refused::Capture));
        assert_eq!(
            rig.next(),
            Event::Error {
                message: message.into()
            }
        );
        assert_eq!(rig.voice.phase(), Phase::Idle);
        assert!(!rig.mic.with(|m| m.open));

        rig.mic.with(|m| m.start_error = None);
        assert_eq!(
            rig.push(),
            Event::Transcript {
                text: "hello".into()
            }
        );
    }
}

#[test]
fn a_microphone_that_dies_mid_push_is_reported_and_closed() {
    let rig = Rig::new();
    rig.mic
        .with(|m| m.stop_result = Some(Err(CaptureError::Interrupted)));
    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    rig.voice.end();
    assert_eq!(
        rig.next(),
        Event::Error {
            message: "The microphone stopped working. Check that it is still connected.".into()
        }
    );
    assert_eq!(rig.voice.phase(), Phase::Idle);
    assert_eq!(rig.speech.calls(), 0, "nothing was sent");
    assert!(!rig.mic.with(|m| m.open));
}

#[test]
fn a_blocked_or_muted_microphone_is_told_apart_from_a_quiet_room() {
    let rig = Rig::new();
    rig.mic.with(|m| {
        m.stop_result = Some(Ok(Recording {
            samples: vec![0; 16_000],
            hit_limit: false,
        }))
    });
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice.end();
    let Event::Error { message } = rig.next() else {
        panic!("expected an error")
    };
    assert!(
        message.contains("Microphone permission is required"),
        "{message}"
    );
    assert_eq!(rig.speech.calls(), 0);
}

#[test]
fn silence_and_taps_are_not_uploaded_and_each_says_what_to_do() {
    // (what was recorded, whether the shortcut was let go (else the button's Done), what is said)
    let quiet = vec![30i16; 16_000];
    let tap = vec![9000i16; 1_000];
    let cases = [
        (quiet.clone(), true, COULDNT_CATCH),
        (quiet, false, COULDNT_CATCH),
        (tap.clone(), true, TAPPED_SHORTCUT),
        (tap, false, TAPPED_BUTTON),
        (vec![], true, TAPPED_SHORTCUT),
        (vec![], false, TAPPED_BUTTON),
    ];
    for (samples, shortcut, said) in cases {
        let rig = Rig::new();
        rig.mic.with(|m| {
            m.stop_result = Some(Ok(Recording {
                samples,
                hit_limit: false,
            }))
        });
        let session = rig.voice.begin().unwrap();
        rig.next();
        if shortcut {
            rig.voice.end_session(session);
        } else {
            rig.voice.end();
        }
        assert_eq!(
            rig.next(),
            Event::Notice {
                message: said.into()
            }
        );
        assert_eq!(rig.voice.phase(), Phase::Idle);
        assert_eq!(
            rig.speech.calls(),
            0,
            "nothing was sent for a recording with no speech in it"
        );
        rig.quiet();
    }
}

#[test]
fn the_tap_hints_say_what_to_do_and_are_not_the_quiet_room_message() {
    assert!(TAPPED_SHORTCUT.contains("Hold the shortcut") && TAPPED_SHORTCUT.contains("let go"));
    assert!(TAPPED_BUTTON.contains("Done"));
    assert_ne!(TAPPED_SHORTCUT, COULDNT_CATCH);
    assert_ne!(TAPPED_BUTTON, COULDNT_CATCH);
}

// ── The time and size limits ──────────────────────────────────────────────────

#[test]
fn a_push_that_is_never_released_is_stopped_at_the_limit_and_the_person_is_told() {
    let mut config = Config::new(true, true, wake::DEFAULT_PHRASE.into());
    config.max_recording = Duration::from_millis(80);
    let rig = Rig::with(config);
    rig.speech.says("Hey Coucou, what time is it?");

    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    // No release. The limit does it.
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: true
        }
    );
    assert!(
        !rig.mic.with(|m| m.open),
        "the microphone is closed at the limit"
    );
    assert_eq!(
        rig.next(),
        Event::Transcript {
            text: "What time is it?".into()
        }
    );
    assert_eq!(rig.mic.with(|m| m.stopped), 1);

    // A late release of that same key press changes nothing.
    rig.voice.end();
    rig.quiet();
}

#[test]
fn the_default_limit_is_forty_five_seconds() {
    assert_eq!(
        Config::new(true, true, "x".into()).max_recording,
        Duration::from_secs(45)
    );
}

#[test]
fn a_recording_the_device_cut_at_its_own_cap_says_so() {
    let rig = Rig::new();
    rig.mic.with(|m| {
        m.stop_result = Some(Ok(Recording {
            samples: speech(1000).samples,
            hit_limit: true,
        }))
    });
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice.end();
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: true
        }
    );
}

#[test]
fn an_oversized_recording_is_cut_before_it_is_uploaded() {
    let rig = Rig::new();
    let loud = vec![9000i16; MAX_SAMPLES + 123_456];
    rig.mic.with(|m| {
        m.stop_result = Some(Ok(Recording {
            samples: loud,
            hit_limit: false,
        }))
    });
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "hello".into()
        }
    );
    let sizes = rig.speech.0.lock().unwrap().wav_sizes.clone();
    assert_eq!(
        sizes,
        vec![audio::MAX_WAV_BYTES],
        "never more than the limit goes upstream"
    );
}

// ── Cancelling ────────────────────────────────────────────────────────────────

#[test]
fn cancelling_while_recording_closes_the_microphone_and_submits_nothing() {
    let rig = Rig::new();
    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    assert!(rig.voice.cancel());
    assert_eq!(rig.next(), Event::Idle);
    assert!(!rig.mic.with(|m| m.open));
    assert!(
        rig.mic.with(|m| m.held.is_empty()),
        "the audio was thrown away"
    );
    assert_eq!(rig.voice.phase(), Phase::Idle);
    // Letting go of the key afterwards is harmless.
    rig.voice.end();
    rig.quiet();
    assert_eq!(rig.speech.calls(), 0);
    // And voice still works.
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "hello".into()
        }
    );
}

#[test]
fn cancelling_while_transcribing_drops_the_request_and_its_answer() {
    let rig = Rig::new();
    rig.speech.hangs();
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice.end();
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: false
        }
    );
    assert!(rig.voice.cancel());
    assert_eq!(rig.next(), Event::Idle);
    rig.quiet();
    assert_eq!(rig.voice.phase(), Phase::Idle);

    // A new push is not confused by the old one.
    rig.speech.0.lock().unwrap().hang = false;
    rig.speech.says("again");
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "again".into()
        }
    );
}

#[test]
fn an_answer_that_arrives_after_a_cancel_is_never_delivered() {
    let rig = Rig::new();
    rig.speech.hangs();
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice.end();
    rig.next();
    let old = rig.voice.state.lock().unwrap().session;
    rig.voice.cancel();
    rig.next();
    // The answer of the cancelled push turns up anyway.
    rig.voice.deliver(old, Ok("open calculator".into()));
    rig.quiet();
    assert_eq!(rig.voice.phase(), Phase::Idle);
}

#[test]
fn cancelling_when_nothing_is_happening_says_nothing() {
    let rig = Rig::new();
    assert!(!rig.voice.cancel());
    rig.quiet();
}

#[test]
fn switching_voice_off_ends_a_push_in_progress() {
    let rig = Rig::new();
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice
        .configure(Config::new(false, true, wake::DEFAULT_PHRASE.into()));
    assert_eq!(rig.next(), Event::Idle);
    assert!(!rig.mic.with(|m| m.open));
    assert_eq!(rig.voice.begin(), Err(Refused::Disabled));
}

#[test]
fn closing_the_app_closes_the_microphone_even_mid_push() {
    let rig = Rig::new();
    rig.voice.begin().unwrap();
    rig.next();
    rig.voice.shutdown();
    assert!(!rig.mic.with(|m| m.open));
    assert_eq!(rig.voice.phase(), Phase::Idle);
    rig.quiet();
}

#[test]
fn a_timer_left_over_from_an_old_push_cannot_end_a_new_one() {
    let mut config = Config::new(true, true, wake::DEFAULT_PHRASE.into());
    config.max_recording = Duration::from_millis(300);
    let rig = Rig::with(config);
    rig.voice.begin().unwrap();
    rig.next();
    let old = rig.voice.state.lock().unwrap().session;
    rig.voice.cancel();
    rig.next();
    rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    // The first push's timer firing now is stale and is ignored.
    rig.voice.finish_recording(Some(old), true);
    assert_eq!(rig.voice.phase(), Phase::Listening);
    assert_eq!(rig.mic.with(|m| m.stopped), 0);
}

#[test]
fn the_release_of_an_old_press_cannot_end_a_newer_push() {
    let rig = Rig::new();
    let first = rig.voice.begin().unwrap();
    rig.next();
    rig.voice.cancel();
    rig.next();
    let second = rig.voice.begin().unwrap();
    assert_eq!(rig.next(), Event::Listening);
    assert_ne!(first, second);

    // The first press is let go late: nothing happens to the second push.
    rig.voice.end_session(first);
    assert_eq!(rig.voice.phase(), Phase::Listening);
    assert_eq!(rig.mic.with(|m| m.stopped), 0);
    rig.quiet();

    // Its own release does end it.
    rig.voice.end_session(second);
    assert_eq!(
        rig.next(),
        Event::Transcribing {
            limit_reached: false
        }
    );
}

// ── What comes back ───────────────────────────────────────────────────────────

#[test]
fn a_failed_transcription_is_reported_and_leaves_voice_ready() {
    let rig = Rig::new();
    rig.speech
        .fails("I couldn't transcribe that. Check your connection.");
    assert_eq!(
        rig.push(),
        Event::Error {
            message: "I couldn't transcribe that. Check your connection.".into()
        }
    );
    assert_eq!(rig.voice.phase(), Phase::Idle);
    rig.speech.says("fine now");
    assert_eq!(
        rig.push(),
        Event::Transcript {
            text: "fine now".into()
        }
    );
}

#[test]
fn an_empty_or_phrase_only_transcript_is_not_a_request() {
    for said in ["", "   ", "\n", "Hey Coucou.", "hey coucou, ..."] {
        let rig = Rig::new();
        rig.speech.says(said);
        assert_eq!(
            rig.push(),
            Event::Notice {
                message: COULDNT_CATCH.into()
            },
            "{said:?}"
        );
    }
}

#[test]
fn the_text_is_normalised_in_one_place() {
    let on = Config::new(true, true, "Hey Coucou".into());
    let off = Config::new(true, false, "Hey Coucou".into());
    assert_eq!(normalize("Hey Coucou! Open Notepad.", &on), "Open Notepad.");
    assert_eq!(
        normalize("Hey Coucou! Open Notepad.", &off),
        "Hey Coucou! Open Notepad."
    );
    assert_eq!(normalize("  Open Notepad.  ", &on), "Open Notepad.");
    assert_eq!(normalize("  Open Notepad.  ", &off), "Open Notepad.");
}

#[test]
fn no_message_the_island_can_show_gives_away_a_path_a_key_or_a_stack_trace() {
    for e in [
        CaptureError::NoDevice,
        CaptureError::InUse,
        CaptureError::Failed,
        CaptureError::Interrupted,
        CaptureError::Unsupported,
    ] {
        let m = e.message();
        assert!(
            !m.contains('\\')
                && !m.contains("http")
                && !m.contains("panicked")
                && !m.contains("0x"),
            "{m}"
        );
    }
    assert!(!BLOCKED.contains('\\') && !COULDNT_CATCH.contains('\\'));
}

#[test]
fn events_reach_the_page_in_the_documented_shape() {
    let json = |e: &Event| serde_json::to_value(e).unwrap();
    assert_eq!(
        json(&Event::Listening),
        serde_json::json!({ "phase": "listening" })
    );
    assert_eq!(json(&Event::Idle), serde_json::json!({ "phase": "idle" }));
    assert_eq!(
        json(&Event::Transcribing {
            limit_reached: true
        }),
        serde_json::json!({ "phase": "transcribing", "limitReached": true })
    );
    assert_eq!(
        json(&Event::Transcript {
            text: "Open Notepad.".into()
        }),
        serde_json::json!({ "phase": "transcript", "text": "Open Notepad." })
    );
    assert_eq!(
        json(&Event::Notice {
            message: "m".into()
        }),
        serde_json::json!({ "phase": "notice", "message": "m" })
    );
    assert_eq!(
        json(&Event::Error {
            message: "m".into()
        }),
        serde_json::json!({ "phase": "error", "message": "m" })
    );
}
