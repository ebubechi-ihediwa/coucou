// Coucou for Windows — app wiring and the commands the island calls.

mod actions;
mod assistant;
mod claude;
mod executor;
mod files;
mod hooks;
mod http;
mod integrations;
mod island;
mod log;
mod pipe;
#[cfg(windows)]
mod pipe_acl;
mod platform;
mod screen;
mod secrets;
mod settings;
mod tray;
mod voice;

use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::{ManagerExt, MacosLauncher};

use claude::{Chat, ChatContext};
use files::DroppedFile;
use hooks::{HookPreview, HookStatus};
use island::{PollGate, ScreenInfo};
use pipe::Pending;
use settings::Settings;

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub gate: Arc<PollGate>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootInfo {
    settings: Settings,
    screen: ScreenInfo,
    version: String,
    hook_path: String,
    /// False where the OS has no global cursor (Wayland): the page then reports
    /// the cursor from its own mouse events.
    cursor_poll: bool,
}

#[tauri::command]
fn boot(app: AppHandle, shared: State<Shared>) -> BootInfo {
    let mut settings = shared.settings.lock().unwrap().clone();
    // The real state of ~/.claude/settings.json wins over whatever we stored.
    settings.hooks_installed = hooks::status().installed;
    let screen = island::screen_info(&app, &settings.screen);
    BootInfo {
        settings,
        screen,
        version: env!("CARGO_PKG_VERSION").to_string(),
        hook_path: settings::hook_exe_path().to_string_lossy().to_string(),
        cursor_poll: platform::CURSOR_POLL,
    }
}

#[tauri::command]
fn save_settings(app: AppHandle, shared: State<Shared>, settings: Settings) {
    // Whatever the page sent is held to the same rules as a settings file.
    let settings = settings::sanitized(settings);
    let (screen_changed, autostart_changed, shortcut_changed, voice_changed) = {
        let mut current = shared.settings.lock().unwrap();
        let screen_changed = current.screen != settings.screen;
        let autostart_changed = current.autostart != settings.autostart;
        let shortcut_changed =
            current.voice_enabled != settings.voice_enabled || current.voice_shortcut != settings.voice_shortcut;
        let voice_changed = shortcut_changed
            || current.wake_phrase_enabled != settings.wake_phrase_enabled
            || current.wake_phrase != settings.wake_phrase
            || current.speech_provider != settings.speech_provider;
        *current = settings.clone();
        (screen_changed, autostart_changed, shortcut_changed, voice_changed)
    };
    // The log, not stderr: there is no console to read it from.
    settings::save_or_log(&settings);
    if autostart_changed {
        let manager = app.autolaunch();
        let result = if settings.autostart { manager.enable() } else { manager.disable() };
        if let Err(err) = result {
            eprintln!("[coucou] autostart: {err}");
        }
    }
    if screen_changed {
        let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
        island::apply_geometry(&app, &settings.screen, collapsed);
    }
    if voice_changed {
        apply_voice(&app, shortcut_changed);
    }
    // Keep the other window in step (island ⇄ settings window).
    let _ = app.emit("settings-changed", settings);
}

/// Hidden island → shrink the window to the invisible wake strip and park the
/// cursor poll; anything else → full panel and 60 Hz polling.
#[tauri::command]
fn set_collapsed(app: AppHandle, shared: State<Shared>, collapsed: bool) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    shared.gate.collapsed.store(collapsed, Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
    // The wake strip must always take the mouse, and a resize invalidates the flag.
    island::refresh_click_through(&app, &shared.gate);
    shared.gate.set_active(!collapsed);
}

/// The front end pushes the island shape; Rust decides click-through from it.
#[tauri::command]
fn set_island_rect(app: AppHandle, shared: State<Shared>, x: f64, y: f64, width: f64, height: f64) {
    shared.gate.set_rect(island::IslandRect { x, y, w: width, h: height });
    // Without the cursor poll the input region is the click-through: it follows the island.
    if !platform::CURSOR_POLL {
        island::refresh_click_through(&app, &shared.gate);
    }
}

#[tauri::command]
fn focus_window(app: AppHandle, focused: bool) {
    let Some(win) = island::window(&app) else { return };
    platform::set_activating(&win, focused);
    if focused {
        let _ = win.set_focus();
    }
}

#[tauri::command]
fn reposition(app: AppHandle, shared: State<Shared>) {
    let pref = shared.settings.lock().unwrap().screen.clone();
    let collapsed = shared.gate.collapsed.load(Ordering::Relaxed);
    island::apply_geometry(&app, &pref, collapsed);
}

#[tauri::command]
fn open_url(url: String) {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return;
    }
    platform::open_url(&url);
}

/// "Open terminal" opens the working folder in VS Code when `code` is on PATH,
/// and falls back to the file manager otherwise.
#[tauri::command]
fn open_in_vscode(path: Option<String>) -> bool {
    // No shell anywhere near this. The path is a project folder chosen by
    // whoever is using Claude Code, and a shell would happily read `&`, `^`, `%`
    // or `$` in a folder name as syntax. Finding the launcher ourselves and
    // handing the path over as a separate argument keeps it a path.
    let path = path.filter(|p| !p.is_empty());
    // It arrives in a hook payload: only an existing folder, given by its full
    // path, goes any further. `code` would read `--something` as an option, and
    // xdg-open would launch a file with whatever handles its type.
    if let Some(p) = path.as_deref() {
        let p = std::path::Path::new(p);
        if !(p.is_absolute() && p.is_dir()) {
            return false;
        }
    }
    if let Some(code) = platform::find_on_path("code") {
        let mut cmd = Command::new(code);
        if let Some(p) = path.as_deref() {
            cmd.arg(p);
        }
        if platform::no_console(&mut cmd).spawn().is_ok() {
            return true;
        }
    }
    if let Some(p) = path.as_deref() {
        platform::reveal_folder(p);
    }
    false
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Tray → Pause. Paused means paused: the pollers stop talking to the network,
/// not just the island stopping showing things.
#[tauri::command]
fn set_paused(paused: bool) {
    integrations::set_paused(paused);
}

// ── Claude Code hooks ─────────────────────────────────────────────────────────

#[tauri::command]
fn hooks_status() -> HookStatus {
    hooks::status()
}

/// Returns the diff the user has to look at before anything is written.
#[tauri::command]
fn hooks_preview(install: bool) -> Result<HookPreview, String> {
    hooks::preview(install)
}

/// Only ever called from an explicit click in the settings window.
#[tauri::command]
fn hooks_apply(
    app: AppHandle,
    shared: State<Shared>,
    install: bool,
    fingerprint: String,
) -> Result<String, String> {
    // The fingerprint comes from the preview the user actually looked at, so a
    // settings.json that changed in between is refused rather than overwritten.
    let backup = hooks::write(install, &fingerprint)?;
    let updated = {
        let mut current = shared.settings.lock().unwrap();
        current.hooks_installed = install;
        settings::save_or_log(&current);
        current.clone()
    };
    let _ = app.emit("settings-changed", updated);
    Ok(backup)
}

#[tauri::command]
fn approval_decision(app: AppHandle, request_id: String, decision: String) {
    pipe::answer(&app, &request_id, &decision);
}

/// The island has the card on screen, so the long wait for a human may begin.
/// Until this arrives the relay only waits a few hundred milliseconds, which is
/// what stops a paused or unresponsive island from freezing Claude Code.
#[tauri::command]
fn approval_ack(app: AppHandle, request_id: String) {
    pipe::acknowledge(&app, &request_id);
}

/// Nobody can act on this request — the island is paused, or another card is
/// already up. Claude Code falls back to asking in the terminal immediately.
#[tauri::command]
fn approval_decline(app: AppHandle, request_id: String) {
    pipe::decline(&app, &request_id);
}

// ── Chat, files and secrets ───────────────────────────────────────────────────

/// The assistant runtime, with the real launcher.
type Assistant = assistant::Runtime<executor::SystemLauncher>;

/// What a chat turn gives the island: the words, and an action waiting for an answer.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    text: String,
    proposal: Option<assistant::ProposalView>,
    /// The person cancelled; there is nothing to show.
    cancelled: bool,
}

/// Tells the island where the assistant stands, so a card can appear whatever
/// view is open.
fn publish(app: &AppHandle, snapshot: &assistant::Snapshot) {
    let _ = app.emit_to(island::WINDOW_LABEL, "assistant", snapshot);
}

/// One chat turn. The API key and any file bytes stay on the Rust side.
///
/// The model may answer with an action to propose. It is judged here, in Rust, and
/// comes back as a proposal for the person to approve: nothing runs from this call
/// unless the policy has explicitly let a low-risk action through.
#[tauri::command]
async fn chat_send(
    app: AppHandle,
    shared: State<'_, Shared>,
    chat: State<'_, Chat>,
    assistant: State<'_, Assistant>,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let model = shared.settings.lock().unwrap().model.clone();
    let (token, superseded) = assistant
        .begin_turn()
        .map_err(|_| "Coucou is still working on your last request.".to_string())?;
    assistant::settle(&chat, superseded);
    publish(&app, &assistant.snapshot());

    // The request runs as a task of its own so that cancelling can end it. If it
    // is ended, its sender goes with it and the wait below returns an error.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task_app = app.clone();
    let task = tauri::async_runtime::spawn(async move {
        let chat = task_app.state::<Chat>();
        let assistant = task_app.state::<Assistant>();
        let reply = claude::send(&chat, &model, query, context, assistant.files()).await;
        let _ = tx.send(reply);
    });
    assistant.attach_abort(token, move || task.abort());

    let cancelled = || ChatReply { text: String::new(), proposal: None, cancelled: true };
    let Ok(reply) = rx.await else { return Ok(cancelled()) };
    let reply = match reply {
        Ok(reply) => reply,
        Err(message) => {
            assistant.fail_turn(token, message.clone());
            publish(&app, &assistant.snapshot());
            return Err(message);
        }
    };

    let Ok(settled) = assistant.on_reply(token, reply.text, reply.proposal) else {
        // Cancelled while the answer was on its way: it is dropped, not shown.
        return Ok(cancelled());
    };
    let (text, proposal) = match settled {
        assistant::Settled::Reply { text } => (text, None),
        assistant::Settled::Proposed { text, proposal } => (text, Some(proposal)),
        assistant::Settled::Refused { text, reason, result } => {
            assistant::settle(&chat, Some(result));
            log::line(format!("assistant: refused a proposed action ({reason})"));
            // The model's words may promise something; say that nothing happened.
            let said = if text.is_empty() {
                reason
            } else {
                format!("{text}\n\nCoucou didn't do that: {reason}")
            };
            (said, None)
        }
        assistant::Settled::AutoApproved { text, proposal, approved } => {
            // Only a policy that allows low-risk actions without asking gets here.
            let outcome = run_approved(&assistant, &approved);
            let (snapshot, result) = assistant.finish(proposal.id, outcome);
            assistant::settle(&chat, result);
            publish(&app, &snapshot);
            return Ok(ChatReply { text, proposal: None, cancelled: false });
        }
    };
    publish(&app, &assistant.snapshot());
    Ok(ChatReply { text, proposal, cancelled: false })
}

/// Carries out an approved action and logs how it ended, never what it targeted.
/// The executor only starts a process or checks one file, which takes milliseconds,
/// so it runs right here rather than on a thread of its own.
fn run_approved(assistant: &Assistant, approved: &assistant::Approved) -> executor::Outcome {
    let outcome = assistant.execute(approved);
    log::line(format!(
        "assistant: action {} {}",
        approved.id,
        match &outcome {
            executor::Outcome::Done(_) => "done",
            executor::Outcome::Failed(_) => "failed",
            executor::Outcome::Cancelled => "cancelled before it started",
        }
    ));
    outcome
}

/// The person's answer to a proposal. Approving runs the action; denying withdraws
/// it. Either way the result is returned and published.
#[tauri::command]
async fn assistant_decide(
    app: AppHandle,
    chat: State<'_, Chat>,
    assistant: State<'_, Assistant>,
    proposal_id: u64,
    approve: bool,
) -> Result<assistant::Snapshot, String> {
    if !approve {
        let result = assistant.deny(proposal_id).map_err(|e| e.message().to_string())?;
        assistant::settle(&chat, result);
        let snapshot = assistant.snapshot();
        publish(&app, &snapshot);
        return Ok(snapshot);
    }
    let approved = assistant.approve(proposal_id).map_err(|e| e.message().to_string())?;
    publish(&app, &assistant.snapshot());
    let outcome = run_approved(&assistant, &approved);
    let (snapshot, result) = assistant.finish(proposal_id, outcome);
    assistant::settle(&chat, result);
    publish(&app, &snapshot);
    Ok(snapshot)
}

/// Cancels whatever the assistant is doing: the request, a waiting proposal, or an
/// action that has not started yet.
#[tauri::command]
fn assistant_cancel(app: AppHandle, chat: State<Chat>, assistant: State<Assistant>) -> assistant::Snapshot {
    let report = assistant.cancel();
    assistant::settle(&chat, report.result);
    publish(&app, &report.snapshot);
    report.snapshot
}

#[tauri::command]
fn assistant_state(assistant: State<Assistant>) -> assistant::Snapshot {
    assistant.snapshot()
}

#[tauri::command]
fn chat_reset(app: AppHandle, chat: State<Chat>, assistant: State<Assistant>) {
    assistant.reset();
    chat.reset();
    publish(&app, &assistant.snapshot());
}

/// Copies a dropped file into the inbox and reports its name back. Only a path the
/// OS handed us in a drop is accepted (see `files::Grants`).
#[tauri::command]
fn ingest_file(grants: State<files::Grants>, path: String) -> Result<DroppedFile, String> {
    files::ingest(&grants, &path)
}

/// The island may only ask whether a key exists — never read it.
#[tauri::command]
fn secret_present(key: String) -> bool {
    secrets::present(&key)
}

#[tauri::command]
fn secret_set(key: String, value: String) -> Result<(), String> {
    secrets::set(&key, &value)
}

#[tauri::command]
fn secret_clear(key: String) -> Result<(), String> {
    secrets::clear(&key)
}

/// Opens the configured n8n instance — the URL lives in the Credential Manager.
#[tauri::command]
fn open_n8n() {
    if let Some(url) = secrets::get("n8n-url") {
        open_url(url);
    }
}

/// Refresh buttons in the integration cards.
#[tauri::command]
async fn refresh_integration(app: AppHandle, id: String) {
    integrations::poll_once(app, &id).await;
}

/// Lets the island write to the same log as the Rust side.
#[tauri::command]
fn log_line(message: String) {
    log::line(format!("ui  {message}"));
}

// ── Voice ─────────────────────────────────────────────────────────────────────

/// Everything push-to-talk needs from the app, in one place: the controller, the
/// registered shortcut, and where the registration stands.
pub struct VoiceHub {
    voice: Arc<voice::Voice>,
    hotkey: Mutex<Option<platform::voice::Hotkey>>,
    shortcut: Mutex<ShortcutStatus>,
    /// The shortcut thread hands its events to one consumer, so a press and the
    /// release after it are always handled in that order.
    events: std::sync::mpsc::Sender<voice::HotkeyEvent>,
}

#[derive(Clone, PartialEq)]
enum ShortcutStatus {
    /// Voice is switched off: no shortcut is registered.
    Off,
    Ready,
    Failed(String),
}

/// What the page is told about voice, for the settings window and the mic button.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceStatus {
    /// False where there is no push-to-talk yet (Linux).
    supported: bool,
    enabled: bool,
    shortcut: String,
    /// "off", "ready" or "failed".
    shortcut_state: &'static str,
    /// Why the shortcut could not be registered.
    problem: Option<String>,
    /// Which speech service is chosen ("openai" or "groq") and what to call it.
    provider: &'static str,
    provider_label: &'static str,
    /// Whether that service's key is saved (never its value).
    has_key: bool,
    phase: voice::Phase,
}

/// Carries what the controller says to the island.
struct IslandSink {
    app: AppHandle,
}

impl voice::Sink for IslandSink {
    fn publish(&self, event: voice::Event) {
        // What was said is the person's own and stays out of the log: only that
        // something happened, and how long it was.
        log::line(match &event {
            voice::Event::Idle => "voice: cancelled".to_string(),
            voice::Event::Listening => "voice: listening".to_string(),
            voice::Event::Transcribing { limit_reached } => {
                format!("voice: transcribing (time limit reached: {limit_reached})")
            }
            voice::Event::Transcript { text } => format!("voice: transcript of {} characters", text.chars().count()),
            voice::Event::Notice { message } | voice::Event::Error { message } => format!("voice: {message}"),
        });
        let _ = self.app.emit_to(island::WINDOW_LABEL, "voice", &event);
    }
}

impl VoiceHub {
    fn new(app: &AppHandle, settings: &Settings) -> VoiceHub {
        let (events, incoming) = std::sync::mpsc::channel();
        let gate_app = app.clone();
        let voice = voice::Voice::new(
            Box::new(platform::voice::Microphone::default()),
            Arc::new(voice::transcribe::Speech),
            Arc::new(IslandSink { app: app.clone() }),
            Box::new(move || voice_gate(&gate_app)),
            voice_config(settings),
        );
        // One thread, asleep until the OS reports the shortcut. Not a poll.
        let worker = voice.clone();
        let spawned = std::thread::Builder::new().name("coucou-voice".into()).spawn(move || {
            // A release ends the push its own press started, and no other: tapping the
            // shortcut while a push begun with the button is running must not stop that
            // one, and a late release must not stop a newer push.
            let mut started: Option<u64> = None;
            for event in incoming {
                match event {
                    voice::HotkeyEvent::Pressed => started = worker.begin().ok(),
                    voice::HotkeyEvent::Released => {
                        if let Some(session) = started.take() {
                            worker.end_session(session);
                        }
                    }
                }
            }
        });
        if spawned.is_err() {
            log::line("voice: could not start its thread");
        }
        VoiceHub {
            voice,
            hotkey: Mutex::new(None),
            shortcut: Mutex::new(ShortcutStatus::Off),
            events,
        }
    }
}

/// The speech service the settings name. Settings validation keeps it one of the two the
/// app knows, so the fallback is only for a value that never went through it.
fn voice_provider(settings: &Settings) -> voice::transcribe::Provider {
    voice::transcribe::Provider::from_id(&settings.speech_provider)
        .unwrap_or(voice::transcribe::DEFAULT_PROVIDER)
}

fn voice_config(settings: &Settings) -> voice::Config {
    voice::Config::new(settings.voice_enabled, settings.wake_phrase_enabled, settings.wake_phrase.clone())
        .with_provider(voice_provider(settings))
}

/// A push may not start while the assistant is busy (a typed message is refused the
/// same way), while Coucou is paused, or with no key for the speech service. Said
/// before the microphone is touched.
fn voice_gate(app: &AppHandle) -> Option<String> {
    use assistant::Phase;
    if matches!(app.state::<Assistant>().snapshot().phase, Phase::Thinking | Phase::Executing) {
        return Some(voice::STILL_WORKING.to_string());
    }
    if integrations::PAUSED.load(Ordering::Relaxed) {
        return Some("Coucou is paused.".to_string());
    }
    let provider = voice_provider(&app.state::<Shared>().settings.lock().unwrap());
    if !secrets::present(provider.key_name()) {
        return Some(provider.no_key_message());
    }
    None
}

/// Makes the controller and the registered shortcut match the settings. Turning voice
/// off unregisters the shortcut, which is what makes the microphone unreachable.
fn apply_voice(app: &AppHandle, reregister: bool) {
    let hub = app.state::<VoiceHub>();
    let settings = app.state::<Shared>().settings.lock().unwrap().clone();
    hub.voice.configure(voice_config(&settings));
    if !reregister {
        return;
    }
    // Dropped outside the lock: ending the shortcut's thread must not wait on it.
    let old = hub.hotkey.lock().unwrap().take();
    drop(old);

    let status = if !settings.voice_enabled {
        ShortcutStatus::Off
    } else {
        match voice::shortcut::Shortcut::parse(&settings.voice_shortcut) {
            Err(why) => ShortcutStatus::Failed(why.message().to_string()),
            Ok(shortcut) => {
                let events = hub.events.clone();
                let on_event: Arc<dyn Fn(voice::HotkeyEvent) + Send + Sync> = Arc::new(move |event| {
                    let _ = events.send(event);
                });
                match platform::voice::Hotkey::register(&shortcut, on_event) {
                    Ok(hotkey) => {
                        *hub.hotkey.lock().unwrap() = Some(hotkey);
                        ShortcutStatus::Ready
                    }
                    Err(why) => ShortcutStatus::Failed(why.message().to_string()),
                }
            }
        }
    };
    log::line(match &status {
        ShortcutStatus::Off => "voice: off, no shortcut registered".to_string(),
        ShortcutStatus::Ready => format!("voice: shortcut {} registered", settings.voice_shortcut),
        ShortcutStatus::Failed(why) => format!("voice: shortcut {} not registered: {why}", settings.voice_shortcut),
    });
    *hub.shortcut.lock().unwrap() = status;
    let _ = app.emit("voice-status", voice_status(app));
}

fn voice_status(app: &AppHandle) -> VoiceStatus {
    let settings = app.state::<Shared>().settings.lock().unwrap().clone();
    let provider = voice_provider(&settings);
    // A page can ask before setup has made the hub; it then simply sees "off".
    let hub = app.try_state::<VoiceHub>();
    let (shortcut_state, problem) = match hub.as_ref().map(|h| h.shortcut.lock().unwrap().clone()) {
        Some(ShortcutStatus::Ready) => ("ready", None),
        Some(ShortcutStatus::Failed(why)) => ("failed", Some(why)),
        Some(ShortcutStatus::Off) | None => ("off", None),
    };
    VoiceStatus {
        supported: platform::voice::SUPPORTED,
        enabled: settings.voice_enabled,
        shortcut: settings.voice_shortcut,
        shortcut_state,
        problem,
        provider: provider.id(),
        provider_label: provider.label(),
        has_key: secrets::present(provider.key_name()),
        phase: hub.map_or(voice::Phase::Idle, |h| h.voice.phase()),
    }
}

#[tauri::command]
fn voice_state(app: AppHandle) -> VoiceStatus {
    voice_status(&app)
}

/// The microphone button. Starts a push exactly like the shortcut does; the second
/// press of the button (`voice_stop`) is the release.
#[tauri::command]
fn voice_start(hub: State<VoiceHub>) -> Result<(), String> {
    match hub.voice.begin() {
        Err(voice::Refused::Disabled) => Err("Voice is off. Turn it on in Settings.".into()),
        // Anything else has already been said to the island.
        _ => Ok(()),
    }
}

/// For the settings window: the shortcut in its canonical spelling, or the reason it
/// cannot be one. Nothing is registered here.
#[tauri::command]
fn voice_check_shortcut(text: String) -> Result<String, String> {
    voice::shortcut::Shortcut::parse(&text)
        .map(|shortcut| shortcut.to_string())
        .map_err(|why| why.message().to_string())
}

#[tauri::command]
fn voice_check_phrase(text: String) -> Result<String, String> {
    let phrase = text.trim();
    if voice::wake::valid_phrase(phrase) {
        Ok(phrase.to_string())
    } else {
        Err("Use letters and spaces, up to 40 characters.".to_string())
    }
}

#[tauri::command]
fn voice_stop(hub: State<VoiceHub>) {
    hub.voice.end();
}

#[tauri::command]
fn voice_cancel(hub: State<VoiceHub>) {
    hub.voice.cancel();
}

// ── Settings window ───────────────────────────────────────────────────────────

/// WebView2 allows exactly one browser environment per app, and its options are
/// fixed by whichever webview is created first. Every window must therefore ask
/// for the *same* arguments as the island (see `additionalBrowserArgs` in
/// tauri.conf.json) — a mismatch makes the second window come up blank, with no
/// error anywhere.
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --autoplay-policy=no-user-gesture-required";

/// In a dev build the pages are served by Vite, so the second window needs the
/// absolute dev URL; a bundled build resolves it inside the app bundle.
fn settings_page_url(app: &AppHandle) -> WebviewUrl {
    #[cfg(dev)]
    if let Some(mut base) = app.config().build.dev_url.clone() {
        base.set_path("/settings.html");
        return WebviewUrl::External(base);
    }
    let _ = app;
    WebviewUrl::App("settings.html".into())
}

/// The settings window is created hidden at launch and only ever shown and
/// hidden afterwards. A WebView2 window created later — on the main thread or
/// not — silently comes up blank in this app, so the window that works is the
/// one that exists before the island's webview does.
fn create_settings_window(app: &AppHandle) {
    let url = settings_page_url(app);
    match WebviewWindowBuilder::new(app, "settings", url)
        .additional_browser_args(BROWSER_ARGS)
        .title("Settings — Coucou")
        .inner_size(560.0, 680.0)
        .min_inner_size(460.0, 480.0)
        .resizable(true)
        .visible(false)
        .center()
        .build()
    {
        Ok(win) => {
            // Closing it must only hide it, or it could never be reopened.
            let hidden = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = hidden.hide();
                }
            });
        }
        Err(err) => log::line(format!("settings window failed: {err}")),
    }
}

pub fn show_settings_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("settings") else {
        log::line("settings window missing");
        return;
    };
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

#[tauri::command]
fn open_settings_window(app: AppHandle) {
    show_settings_window(&app);
}

pub fn run() {
    platform::prepare_environment();
    let loaded = settings::load();
    let gate = Arc::new(PollGate::new());

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            let _ = app.emit_to(island::WINDOW_LABEL, "tray", "open".to_string());
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .manage(Shared {
            settings: Mutex::new(loaded.clone()),
            gate: gate.clone(),
        })
        .manage(Pending::default())
        .manage(Chat::default())
        .manage(Assistant::new(
            executor::SystemLauncher,
            executor::Files::inbox(),
            actions::PolicyConfig::default(),
        ))
        .manage(files::Grants::default())
        // The OS drop event is the only source of a path `ingest_file` will accept.
        // It is recorded here, in Rust, before the page is able to react to it.
        .on_webview_event(|webview, event| {
            if webview.label() != island::WINDOW_LABEL {
                return;
            }
            if let tauri::WebviewEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                webview.state::<files::Grants>().grant(paths);
            }
        })
        .invoke_handler(tauri::generate_handler![
            boot,
            save_settings,
            set_collapsed,
            set_island_rect,
            focus_window,
            reposition,
            open_url,
            open_in_vscode,
            quit_app,
            hooks_status,
            hooks_preview,
            hooks_apply,
            approval_decision,
            approval_ack,
            approval_decline,
            log_line,
            chat_send,
            chat_reset,
            assistant_decide,
            assistant_cancel,
            assistant_state,
            ingest_file,
            secret_present,
            secret_set,
            secret_clear,
            refresh_integration,
            open_n8n,
            open_settings_window,
            set_paused,
            voice_state,
            voice_start,
            voice_stop,
            voice_cancel,
            voice_check_shortcut,
            voice_check_phrase,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            app.manage(VoiceHub::new(&handle, &loaded));
            tray::build(&handle)?;
            // Before the island: see create_settings_window.
            create_settings_window(&handle);

            if let Some(win) = island::window(&handle) {
                platform::make_non_activating(&win);
                island::apply_geometry(&handle, &loaded.screen, false);
                let _ = win.show();
            }
            gate.collapsed.store(false, Ordering::Relaxed);
            // Nothing drawn yet, so nothing takes the mouse until the page
            // reports the island's shape.
            if !platform::CURSOR_POLL {
                island::refresh_click_through(&handle, &gate);
            }
            gate.set_active(true);
            island::spawn_cursor_poll(handle.clone(), gate.clone());

            log::line(format!("--- Coucou {} started ---", env!("CARGO_PKG_VERSION")));
            hooks::ensure_hook_exe(&handle);
            pipe::start(handle.clone());
            integrations::start(handle.clone());
            apply_voice(&handle, true);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building Coucou")
        .run(|app, event| {
            // Whatever is happening, the microphone is closed before the process goes.
            if let tauri::RunEvent::Exit = event {
                let hub = app.state::<VoiceHub>();
                hub.voice.shutdown();
                let hotkey = hub.hotkey.lock().unwrap().take();
                drop(hotkey);
            }
        });
}
