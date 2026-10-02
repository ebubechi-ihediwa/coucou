// Preferences, stored as JSON in settings.json under platform::config_dir().
// No secret ever lands here — API keys live in the OS keychain (see secrets.rs).
//
// The file is `{ "version": N, ...the fields of Settings }`. The rules:
//   * A bad value costs only that value. Every field is read and checked on its
//     own, so one wrong type or an out-of-range number never resets the rest.
//   * Nothing is thrown away. A file that cannot be read as JSON is moved aside
//     (`settings.json.corrupt-<time>`), an older format is copied to
//     `settings.json.v<N>.bak` before it is rewritten, and a file from a *newer*
//     Coucou is copied the same way before we first write over it.
//   * Writes go to a temporary file in the same folder and are renamed over the
//     real one, so a crash leaves the old file or the new one, never half of one.
//   * `Settings::default()` is the only definition of the defaults. The page keeps
//     a copy to render with before `boot` answers; a test fails if the two drift.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// The version this build reads and writes. Files without a `version` are the
/// ones written before versioning existed, and count as version 0.
///
/// 0 → 1: `model` was added to the unversioned format (commit cd50852), so the
/// oldest files lack it; the version field itself is new.
const FORMAT_VERSION: u32 = 1;

const FILE: &str = "settings.json";
/// A settings file is a few hundred bytes. Anything this big is not one.
const MAX_FILE_BYTES: u64 = 1 << 20;

// What the settings window and the island already enforce, now enforced here too.
const VOLUME_RANGE: (f64, f64) = (0.0, 0.2); // slider max, and Sound.setVolume's clamp
const AUTO_CLOSE_RANGE: (f64, f64) = (5.0, 120.0); // the number field's min/max
const ABSENCE_MAX: f64 = 86_400.0; // no UI; a day is already absurd
const MAX_ACTIVE_INTEGRATIONS: usize = 4; // MAX_ACTIVE in the settings window
const MAX_ID_LEN: usize = 64;
const MAX_MODEL_LEN: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sound_enabled: bool,
    pub sound_volume: f64,
    pub auto_close_interval: f64,
    pub absence_interval: f64,
    pub active_integrations: Vec<String>,
    /// "primary" = the main display, "cursor" = whichever display the mouse is on.
    pub screen: String,
    pub autostart: bool,
    pub hooks_installed: bool,
    /// Claude model used by the chat. Changeable in the settings window.
    /// Defaulted explicitly so a settings.json written by an older build still loads.
    #[serde(default = "default_model")]
    pub model: String,
}

fn default_model() -> String {
    crate::claude::DEFAULT_MODEL.to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sound_enabled: true,
            sound_volume: 0.12,
            auto_close_interval: 15.0,
            absence_interval: 180.0,
            active_integrations: vec![
                "integration_resend".into(),
                "integration_n8n".into(),
                "integration_vercel".into(),
                "integration_github".into(),
            ],
            screen: "primary".into(),
            autostart: false,
            hooks_installed: false,
            model: default_model(),
        }
    }
}

pub use crate::platform::{config_dir, local_dir};

pub fn hook_exe_path() -> PathBuf {
    local_dir().join("bin").join(crate::platform::HOOK_EXE)
}

// ── Validation ────────────────────────────────────────────────────────────────

/// Brings every value into what the app accepts and returns the names of the
/// fields it had to change. Numbers are clamped (the intent survives), anything
/// that is not a valid choice goes back to its default.
fn sanitize(s: &mut Settings) -> Vec<&'static str> {
    let d = Settings::default();
    let mut changed = Vec::new();

    let volume = clamp_or(s.sound_volume, VOLUME_RANGE, d.sound_volume);
    if volume != s.sound_volume {
        s.sound_volume = volume;
        changed.push("soundVolume");
    }
    let auto_close = clamp_or(
        s.auto_close_interval,
        AUTO_CLOSE_RANGE,
        d.auto_close_interval,
    );
    if auto_close != s.auto_close_interval {
        s.auto_close_interval = auto_close;
        changed.push("autoCloseInterval");
    }
    // Any positive number of seconds is a usable absence interval.
    let absence = if s.absence_interval > 0.0 {
        clamp_or(
            s.absence_interval,
            (f64::MIN_POSITIVE, ABSENCE_MAX),
            d.absence_interval,
        )
    } else {
        d.absence_interval
    };
    if absence != s.absence_interval {
        s.absence_interval = absence;
        changed.push("absenceInterval");
    }

    if !matches!(s.screen.as_str(), "primary" | "cursor") {
        s.screen = d.screen;
        changed.push("screen");
    }

    // The model id is sent to the API, so it must look like one.
    let model = s.model.trim().to_string();
    if !valid_model(&model) {
        s.model = d.model;
        changed.push("model");
    } else if model != s.model {
        s.model = model;
        changed.push("model");
    }

    // The ids decide which services get polled over the network: only well-formed,
    // distinct ones, and no more than the settings window lets anyone pick.
    let mut ids: Vec<String> = Vec::new();
    for id in &s.active_integrations {
        if valid_id(id) && !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    ids.truncate(MAX_ACTIVE_INTEGRATIONS);
    if ids != s.active_integrations {
        s.active_integrations = ids;
        changed.push("activeIntegrations");
    }
    changed
}

/// `value` clamped into `range`, or `fallback` if it is not a number at all.
fn clamp_or(value: f64, range: (f64, f64), fallback: f64) -> f64 {
    if value.is_finite() {
        value.clamp(range.0, range.1)
    } else {
        fallback
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn valid_model(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= MAX_MODEL_LEN
        && model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// For settings that arrive from the page (`save_settings`): same rules as a file.
pub fn sanitized(mut settings: Settings) -> Settings {
    let changed = sanitize(&mut settings);
    if !changed.is_empty() {
        crate::log::line(format!("settings: corrected {}", changed.join(", ")));
    }
    settings
}

// ── Reading ───────────────────────────────────────────────────────────────────

/// What came out of the file, and what happened on the way (for the log).
#[derive(Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub notes: Vec<String>,
}

fn settings_path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// Serialises the file operations: the island and the settings window can both save.
static IO: Mutex<()> = Mutex::new(());

fn io_lock() -> std::sync::MutexGuard<'static, ()> {
    IO.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn load() -> Settings {
    let loaded = load_from(&config_dir());
    for note in &loaded.notes {
        crate::log::line(format!("settings: {note}"));
    }
    loaded.settings
}

/// Never fails and never panics: whatever the file holds, there are settings.
fn load_from(dir: &Path) -> Loaded {
    let _io = io_lock();
    let path = settings_path(dir);
    let mut notes = Vec::new();
    let defaults = |notes| Loaded {
        settings: Settings::default(),
        notes,
    };

    let meta = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        // A fresh install: nothing to read and nothing to write yet.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return defaults(notes),
        Err(e) => {
            notes.push(format!(
                "cannot read {FILE} ({:?}); using the defaults, file left as it is",
                e.kind()
            ));
            return defaults(notes);
        }
    };
    if meta.len() > MAX_FILE_BYTES {
        notes.push(format!(
            "{FILE} is too large to be settings; moved aside, using the defaults"
        ));
        quarantine(&path, &mut notes);
        return defaults(notes);
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            notes.push(format!(
                "cannot read {FILE} ({:?}); using the defaults, file left as it is",
                e.kind()
            ));
            return defaults(notes);
        }
    };
    // Notepad and friends put a BOM on UTF-8 files.
    let text = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    if text.iter().all(u8::is_ascii_whitespace) {
        // What an interrupted write of the old, non-atomic kind leaves behind.
        // There is nothing in it to keep.
        notes.push(format!("{FILE} is empty; using the defaults"));
        return defaults(notes);
    }

    let Ok(Value::Object(mut object)) = serde_json::from_slice::<Value>(text) else {
        notes.push(format!(
            "{FILE} is not valid settings JSON; moved aside, using the defaults"
        ));
        quarantine(&path, &mut notes);
        return defaults(notes);
    };

    let version = match object.get("version") {
        None => 0,
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) => n,
            None => {
                notes.push("version is not a number; treated as an unversioned file".into());
                0
            }
        },
    };

    if version > FORMAT_VERSION {
        // Written by a newer Coucou. Use what we understand, change nothing here;
        // `save_to` copies the file aside before it is first written over.
        notes.push(format!("{FILE} is version {version}, newer than this build ({FORMAT_VERSION}); read, not rewritten"));
        let settings = from_object(&object, &mut notes);
        return Loaded { settings, notes };
    }

    let migrating = version < FORMAT_VERSION;
    if migrating {
        migrate(&mut object, version);
    }
    let settings = from_object(&object, &mut notes);

    if migrating {
        notes.push(format!(
            "migrated {FILE} from version {version} to {FORMAT_VERSION}"
        ));
        // The original is copied first; if that or the write fails the file on disk
        // is exactly what it was, and the migrated settings still serve this run.
        match backup(&path, &format!("v{version}.bak")) {
            Ok(()) => {
                if let Err(e) = write_file(dir, &settings) {
                    notes.push(format!(
                        "could not write the migrated file ({:?}); the original is untouched",
                        e.kind()
                    ));
                }
            }
            Err(e) => notes.push(format!(
                "could not back up {FILE} ({:?}); not rewriting it",
                e.kind()
            )),
        }
    }
    Loaded { settings, notes }
}

/// Moves each version of the format forward, one step at a time. Deterministic and
/// idempotent: it only adds what is missing, so running it again changes nothing.
fn migrate(object: &mut Map<String, Value>, from: u32) {
    let mut version = from;
    while version < FORMAT_VERSION {
        match version {
            0 => migrate_0_to_1(object),
            _ => break,
        }
        version += 1;
    }
    object.insert("version".into(), FORMAT_VERSION.into());
}

/// Unversioned files exist in two shapes: the first Windows builds (eight fields)
/// and those after `model` was added (nine). Bringing the first to the second is
/// the whole migration; every other value is carried over as it is.
fn migrate_0_to_1(object: &mut Map<String, Value>) {
    object
        .entry("model")
        .or_insert_with(|| default_model().into());
}

/// Reads each field on its own, so a wrong type costs that field and no other.
fn from_object(object: &Map<String, Value>, notes: &mut Vec<String>) -> Settings {
    let d = Settings::default();
    let mut s = Settings {
        sound_enabled: field(
            object,
            "soundEnabled",
            notes,
            Value::as_bool,
            d.sound_enabled,
        ),
        sound_volume: field(object, "soundVolume", notes, Value::as_f64, d.sound_volume),
        auto_close_interval: field(
            object,
            "autoCloseInterval",
            notes,
            Value::as_f64,
            d.auto_close_interval,
        ),
        absence_interval: field(
            object,
            "absenceInterval",
            notes,
            Value::as_f64,
            d.absence_interval,
        ),
        active_integrations: field(
            object,
            "activeIntegrations",
            notes,
            text_list,
            d.active_integrations,
        ),
        screen: field(
            object,
            "screen",
            notes,
            |v| v.as_str().map(String::from),
            d.screen,
        ),
        autostart: field(object, "autostart", notes, Value::as_bool, d.autostart),
        hooks_installed: field(
            object,
            "hooksInstalled",
            notes,
            Value::as_bool,
            d.hooks_installed,
        ),
        model: field(
            object,
            "model",
            notes,
            |v| v.as_str().map(String::from),
            d.model,
        ),
    };
    for name in sanitize(&mut s) {
        notes.push(format!("{name} was invalid or out of range; corrected"));
    }
    s
}

fn field<T>(
    object: &Map<String, Value>,
    key: &str,
    notes: &mut Vec<String>,
    parse: impl Fn(&Value) -> Option<T>,
    default: T,
) -> T {
    match object.get(key) {
        None => default,
        Some(value) => parse(value).unwrap_or_else(|| {
            notes.push(format!("{key} has the wrong type; using its default"));
            default
        }),
    }
}

/// An array of strings. Entries that are not strings are skipped, the rest kept.
fn text_list(value: &Value) -> Option<Vec<String>> {
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
    )
}

// ── Writing ───────────────────────────────────────────────────────────────────

/// Saves the settings, after the same validation a file gets. Failing leaves the
/// previous file in place.
pub fn save(settings: &Settings) -> io::Result<()> {
    save_to(&config_dir(), settings)
}

/// `save`, for callers that have nowhere to show an error: it goes to the log.
pub fn save_or_log(settings: &Settings) {
    if let Err(err) = save(settings) {
        crate::log::line(format!("settings: could not save ({:?})", err.kind()));
    }
}

fn save_to(dir: &Path, settings: &Settings) -> io::Result<()> {
    let _io = io_lock();
    crate::platform::ensure_private_dir(dir)?;
    let path = settings_path(dir);

    // A newer Coucou's file is copied aside before this one overwrites it, and if
    // that copy cannot be made the save does not happen.
    if let Some(newer) = version_on_disk(&path).filter(|v| *v > FORMAT_VERSION) {
        backup(&path, &format!("v{newer}.bak"))?;
    }

    let mut settings = settings.clone();
    sanitize(&mut settings);
    write_file(dir, &settings)
}

fn version_on_disk(path: &Path) -> Option<u32> {
    let bytes = std::fs::read(path).ok()?;
    let text = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    let object = serde_json::from_slice::<Value>(text).ok()?;
    u32::try_from(object.get("version")?.as_u64()?).ok()
}

fn encode(settings: &Settings) -> io::Result<Vec<u8>> {
    let invalid = |e| io::Error::new(io::ErrorKind::InvalidData, e);
    let Value::Object(fields) = serde_json::to_value(settings).map_err(invalid)? else {
        return Err(invalid(serde_json::Error::io(io::Error::other(
            "settings are not an object",
        ))));
    };
    let mut object = Map::new();
    object.insert("version".into(), FORMAT_VERSION.into());
    object.extend(fields);
    serde_json::to_vec_pretty(&Value::Object(object)).map_err(invalid)
}

/// Writes next to the destination and renames over it: the destination holds the
/// old content or the new, never a mixture, on a crash or a full disk. (Rename
/// replaces atomically on Linux filesystems and, in practice, on NTFS; neither
/// platform's documentation in std promises it, and the data is not fsynced to the
/// directory, so a power cut can still lose the *latest* save — not the old file.)
fn write_file(dir: &Path, settings: &Settings) -> io::Result<()> {
    let bytes = encode(settings)?;
    let tmp = dir.join(format!("{FILE}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, settings_path(dir)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copies the settings file to `settings.json.<suffix>`, keeping the first copy
/// ever made: an existing backup is the older, truer original and is left alone.
fn backup(path: &Path, suffix: &str) -> io::Result<()> {
    let target = path.with_file_name(format!("{FILE}.{suffix}"));
    let mut source = File::open(path)?;
    match File::options().write(true).create_new(true).open(&target) {
        Ok(mut out) => {
            if let Err(e) = io::copy(&mut source, &mut out).and_then(|_| out.sync_all()) {
                let _ = std::fs::remove_file(&target);
                return Err(e);
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && target.is_file() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Moves an unreadable file out of the way, keeping every byte of it.
fn quarantine(path: &Path, notes: &mut Vec<String>) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for n in 0..100 {
        let name = if n == 0 {
            format!("{FILE}.corrupt-{stamp}")
        } else {
            format!("{FILE}.corrupt-{stamp}-{n}")
        };
        let target = path.with_file_name(name);
        if target.exists() {
            continue;
        }
        match std::fs::rename(path, &target) {
            Ok(()) => return,
            Err(e) => {
                notes.push(format!(
                    "could not move {FILE} aside ({:?}); it is left as it is",
                    e.kind()
                ));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A scratch settings folder under the temp dir, removed on drop. Tests never
    /// touch the real settings.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "coucou-settings-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Dir(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join(FILE)
        }
        fn put(&self, text: &str) {
            std::fs::write(self.file(), text).unwrap();
        }
        fn read(&self) -> String {
            std::fs::read_to_string(self.file()).unwrap()
        }
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            names
        }
        fn with_prefix(&self, prefix: &str) -> Vec<String> {
            self.names()
                .into_iter()
                .filter(|n| n.starts_with(prefix))
                .collect()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A user who changed everything away from the defaults.
    fn custom() -> Settings {
        Settings {
            sound_enabled: false,
            sound_volume: 0.05,
            auto_close_interval: 30.0,
            absence_interval: 600.0,
            active_integrations: vec!["integration_stripe".into(), "integration_calcom".into()],
            screen: "cursor".into(),
            autostart: true,
            hooks_installed: true,
            model: "claude-sonnet-5".into(),
        }
    }

    /// What that file means today: everything it says, plus the model it never had.
    fn first_build() -> Settings {
        Settings {
            model: default_model(),
            ..custom()
        }
    }

    /// The first Windows builds' file (commit c34451a): eight fields, no model, no version.
    const V0_FIRST_BUILD: &str = r#"{
  "soundEnabled": false,
  "soundVolume": 0.05,
  "autoCloseInterval": 30.0,
  "absenceInterval": 600.0,
  "activeIntegrations": ["integration_stripe", "integration_calcom"],
  "screen": "cursor",
  "autostart": true,
  "hooksInstalled": true
}"#;

    // ── Fresh install, valid files ───────────────────────────────────────────

    #[test]
    fn a_fresh_install_gets_the_defaults_and_writes_nothing() {
        let dir = Dir::new("fresh");
        let loaded = load_from(&dir.0);
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert!(
            dir.names().is_empty(),
            "loading must not create files: {:?}",
            dir.names()
        );
        // And a folder that does not exist yet is the same.
        assert_eq!(
            load_from(&dir.0.join("not-yet")).settings,
            Settings::default()
        );
    }

    #[test]
    fn the_defaults_are_themselves_valid() {
        let mut d = Settings::default();
        assert!(sanitize(&mut d).is_empty());
        assert_eq!(d, Settings::default());
    }

    #[test]
    fn a_complete_valid_file_loads_exactly_and_is_not_rewritten() {
        let dir = Dir::new("valid");
        save_to(&dir.0, &custom()).unwrap();
        let before = dir.read();
        assert!(before.contains("\"version\": 1"), "{before}");

        let loaded = load_from(&dir.0);
        assert_eq!(loaded.settings, custom());
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
        assert_eq!(dir.read(), before, "a current file needs no rewrite");
        assert_eq!(dir.names(), vec![FILE.to_string()], "and no backup either");
    }

    #[test]
    fn a_missing_field_gets_its_default_and_the_others_survive() {
        let dir = Dir::new("missing");
        dir.put(r#"{ "version": 1, "soundEnabled": false, "screen": "cursor" }"#);
        let loaded = load_from(&dir.0);
        let d = Settings::default();
        assert!(!loaded.settings.sound_enabled);
        assert_eq!(loaded.settings.screen, "cursor");
        assert_eq!(loaded.settings.sound_volume, d.sound_volume);
        assert_eq!(loaded.settings.active_integrations, d.active_integrations);
        assert_eq!(loaded.settings.model, d.model);
        // Defaults for missing fields are not an error, and nothing is rewritten.
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    }

    // ── Invalid values ───────────────────────────────────────────────────────

    #[test]
    fn one_bad_field_costs_only_that_field() {
        let dir = Dir::new("badfield");
        dir.put(
            r#"{ "version": 1, "soundEnabled": false, "soundVolume": "loud",
                 "autoCloseInterval": 45, "screen": 7, "autostart": true,
                 "model": "claude-haiku-4-5", "activeIntegrations": "everything" }"#,
        );
        let s = load_from(&dir.0).settings;
        let d = Settings::default();
        // The valid ones are kept...
        assert!(!s.sound_enabled);
        assert!(s.autostart);
        assert_eq!(s.auto_close_interval, 45.0);
        assert_eq!(s.model, "claude-haiku-4-5");
        // ...the wrong-typed ones fall back individually.
        assert_eq!(s.sound_volume, d.sound_volume);
        assert_eq!(s.screen, d.screen);
        assert_eq!(s.active_integrations, d.active_integrations);
    }

    #[test]
    fn values_out_of_range_are_clamped_and_invalid_choices_reset() {
        let mut s = Settings {
            sound_volume: 9.0,
            auto_close_interval: 1.0,
            absence_interval: -5.0,
            screen: "the moon".into(),
            model: "claude; rm -rf /".into(),
            ..Settings::default()
        };
        let changed = sanitize(&mut s);
        assert_eq!(
            s.sound_volume, VOLUME_RANGE.1,
            "too loud is clamped, not reset"
        );
        assert_eq!(s.auto_close_interval, AUTO_CLOSE_RANGE.0);
        assert_eq!(s.absence_interval, Settings::default().absence_interval);
        assert_eq!(s.screen, "primary");
        assert_eq!(
            s.model,
            default_model(),
            "an id that is not a model id is never sent"
        );
        for name in [
            "soundVolume",
            "autoCloseInterval",
            "absenceInterval",
            "screen",
            "model",
        ] {
            assert!(changed.contains(&name), "{name} not reported: {changed:?}");
        }

        let mut s = Settings {
            sound_volume: f64::NAN,
            auto_close_interval: f64::INFINITY,
            ..Settings::default()
        };
        sanitize(&mut s);
        assert_eq!((s.sound_volume, s.auto_close_interval), (0.12, 15.0));
        let mut s = Settings {
            absence_interval: 1e12,
            ..Settings::default()
        };
        sanitize(&mut s);
        assert_eq!(s.absence_interval, ABSENCE_MAX);
    }

    #[test]
    fn model_ids_must_look_like_model_ids() {
        for ok in [
            "claude-opus-5",
            "claude-haiku-4-5",
            "claude-3-5-sonnet-20241022",
            "my.model_v2:latest",
        ] {
            assert!(valid_model(ok), "{ok}");
        }
        for bad in [
            "",
            " ",
            "a b",
            "x;y",
            "x\ny",
            "a/b",
            "é",
            &"m".repeat(MAX_MODEL_LEN + 1),
        ] {
            assert!(!valid_model(bad), "{bad:?}");
        }
        // Stray whitespace around a good id is trimmed, not discarded.
        let mut s = Settings {
            model: "  claude-opus-5\n".into(),
            ..Settings::default()
        };
        sanitize(&mut s);
        assert_eq!(s.model, "claude-opus-5");
    }

    #[test]
    fn integration_ids_are_filtered_deduplicated_and_capped() {
        let mut s = Settings {
            active_integrations: [
                "integration_github",
                "integration_github",
                "Not Valid",
                "",
                "integration_n8n",
                "x-y",
                "integration_vercel",
                "integration_stripe",
                "integration_resend",
            ]
            .map(String::from)
            .to_vec(),
            ..Settings::default()
        };
        sanitize(&mut s);
        assert_eq!(
            s.active_integrations,
            [
                "integration_github",
                "integration_n8n",
                "integration_vercel",
                "integration_stripe"
            ]
        );
        // A long id is not an id.
        assert!(!valid_id(&"a".repeat(MAX_ID_LEN + 1)));
        // Entries that are not text are skipped by the reader, the text ones kept.
        let object: Map<String, Value> = serde_json::from_str(
            r#"{ "activeIntegrations": ["integration_n8n", 4, null, "integration_github"] }"#,
        )
        .unwrap();
        let s = from_object(&object, &mut Vec::new());
        assert_eq!(
            s.active_integrations,
            ["integration_n8n", "integration_github"]
        );
    }

    #[test]
    fn what_the_page_sends_is_checked_like_a_file() {
        let sent = Settings {
            sound_volume: 50.0,
            screen: "elsewhere".into(),
            ..custom()
        };
        let s = sanitized(sent);
        assert_eq!(s.sound_volume, VOLUME_RANGE.1);
        assert_eq!(s.screen, "primary");
        // Everything valid in it is untouched.
        assert_eq!(s.model, custom().model);
        assert_eq!(s.active_integrations, custom().active_integrations);
        // And it is checked on its way to disk too, not only at the command.
        let dir = Dir::new("page");
        save_to(
            &dir.0,
            &Settings {
                auto_close_interval: 9999.0,
                ..custom()
            },
        )
        .unwrap();
        assert_eq!(
            load_from(&dir.0).settings.auto_close_interval,
            AUTO_CLOSE_RANGE.1
        );
    }

    // ── Unknown fields, newer versions ───────────────────────────────────────

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = Dir::new("unknown");
        dir.put(r#"{ "version": 1, "soundEnabled": false, "futureThing": { "a": 1 }, "theme": "dark" }"#);
        let loaded = load_from(&dir.0);
        assert!(!loaded.settings.sound_enabled);
        assert!(loaded.notes.is_empty(), "{:?}", loaded.notes);
    }

    #[test]
    fn a_file_from_a_newer_version_is_read_not_rewritten_and_backed_up_before_any_save() {
        let dir = Dir::new("future");
        let newer =
            r#"{ "version": 7, "soundEnabled": false, "screen": "cursor", "holograms": true }"#;
        dir.put(newer);

        let loaded = load_from(&dir.0);
        assert!(!loaded.settings.sound_enabled);
        assert_eq!(loaded.settings.screen, "cursor");
        assert!(
            loaded.notes.iter().any(|n| n.contains("newer")),
            "{:?}",
            loaded.notes
        );
        assert_eq!(dir.read(), newer, "loading a newer file must not touch it");

        // The user changes something: the newer file is copied first.
        save_to(
            &dir.0,
            &Settings {
                autostart: true,
                ..loaded.settings
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.v7.bak")).unwrap(),
            newer
        );
        assert!(dir.read().contains("\"version\": 1"));
        // A second save does not replace the backup with our own output.
        save_to(&dir.0, &Settings::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.v7.bak")).unwrap(),
            newer
        );
    }

    // ── Migrations ───────────────────────────────────────────────────────────

    #[test]
    fn the_first_builds_file_migrates_with_every_preference_kept() {
        let dir = Dir::new("v0a");
        dir.put(V0_FIRST_BUILD);
        let loaded = load_from(&dir.0);
        assert_eq!(
            loaded.settings,
            first_build(),
            "all eight values carried over, model filled in"
        );
        assert!(
            loaded.notes.iter().any(|n| n.contains("migrated")),
            "{:?}",
            loaded.notes
        );

        // The file is now current, and the original is kept byte for byte.
        let rewritten: Value = serde_json::from_str(&dir.read()).unwrap();
        assert_eq!(rewritten["version"], 1);
        assert_eq!(rewritten["model"], default_model());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.v0.bak")).unwrap(),
            V0_FIRST_BUILD
        );
    }

    #[test]
    fn the_unversioned_file_with_a_model_migrates_too() {
        // The shape after `model` was added (cd50852), still without a version.
        let dir = Dir::new("v0b");
        let text = V0_FIRST_BUILD.replace(
            "\"hooksInstalled\": true",
            "\"hooksInstalled\": true, \"model\": \"claude-sonnet-5\"",
        );
        dir.put(&text);
        assert_eq!(load_from(&dir.0).settings, custom());
        assert_eq!(
            std::fs::read_to_string(dir.0.join("settings.json.v0.bak")).unwrap(),
            text
        );
        assert!(dir.read().contains("\"version\": 1"));
    }

    #[test]
    fn migrating_again_changes_nothing() {
        let dir = Dir::new("again");
        dir.put(V0_FIRST_BUILD);
        let first = load_from(&dir.0).settings;
        let after_first = dir.read();
        let names = dir.names();

        // Every later start reads the migrated file as current: same settings, no
        // rewrite, no second backup, no migration note.
        for _ in 0..3 {
            let again = load_from(&dir.0);
            assert_eq!(again.settings, first);
            assert!(again.notes.is_empty(), "{:?}", again.notes);
            assert_eq!(dir.read(), after_first);
            assert_eq!(dir.names(), names);
        }

        // The migration function itself is a fixed point.
        let mut object: Map<String, Value> = serde_json::from_str(V0_FIRST_BUILD).unwrap();
        migrate(&mut object, 0);
        let once = object.clone();
        migrate(&mut object, 0);
        assert_eq!(object, once);
        migrate(&mut object, FORMAT_VERSION);
        assert_eq!(object, once);
    }

    #[test]
    fn a_migration_that_cannot_back_up_leaves_the_original_untouched() {
        let dir = Dir::new("nobackup");
        dir.put(V0_FIRST_BUILD);
        // Something that is not a file is already where the backup would go.
        std::fs::create_dir(dir.0.join("settings.json.v0.bak")).unwrap();
        let loaded = load_from(&dir.0);
        assert_eq!(
            loaded.settings,
            first_build(),
            "the migrated settings still serve this run"
        );
        assert!(
            loaded.notes.iter().any(|n| n.contains("not rewriting")),
            "{:?}",
            loaded.notes
        );
        assert_eq!(dir.read(), V0_FIRST_BUILD);
    }

    #[test]
    fn a_migration_that_cannot_write_leaves_the_original_untouched() {
        let dir = Dir::new("nowrite");
        dir.put(V0_FIRST_BUILD);
        let backup_exists_already = dir.0.join("settings.json.v0.bak");
        std::fs::write(&backup_exists_already, "older original").unwrap();
        // Block the temp file's name with a directory so the write must fail.
        std::fs::create_dir(dir.0.join(format!("{FILE}.tmp-{}", std::process::id()))).unwrap();
        let loaded = load_from(&dir.0);
        assert_eq!(loaded.settings, first_build());
        assert!(
            loaded
                .notes
                .iter()
                .any(|n| n.contains("original is untouched")),
            "{:?}",
            loaded.notes
        );
        assert_eq!(dir.read(), V0_FIRST_BUILD);
        assert_eq!(
            std::fs::read_to_string(&backup_exists_already).unwrap(),
            "older original",
            "the first backup stays"
        );
    }

    #[test]
    fn a_bad_version_field_is_treated_as_unversioned() {
        let dir = Dir::new("badversion");
        dir.put(r#"{ "version": "two", "soundEnabled": false }"#);
        let loaded = load_from(&dir.0);
        assert!(!loaded.settings.sound_enabled);
        assert!(
            dir.read().contains("\"version\": 1"),
            "stamped with the real version"
        );
        assert!(dir.0.join("settings.json.v0.bak").exists());
    }

    // ── Malformed files ──────────────────────────────────────────────────────

    #[test]
    fn malformed_json_is_moved_aside_never_deleted() {
        for (tag, junk) in [
            ("trunc", r#"{ "version": 1, "soundEnabled": fal"#),
            ("garbage", "this is not json"),
            ("array", "[1, 2, 3]"),
            ("null", "null"),
            ("number", "42"),
        ] {
            let dir = Dir::new(tag);
            dir.put(junk);
            let loaded = load_from(&dir.0);
            assert_eq!(loaded.settings, Settings::default(), "{tag}");
            assert!(
                loaded.notes.iter().any(|n| n.contains("moved aside")),
                "{tag}: {:?}",
                loaded.notes
            );
            let kept = dir.with_prefix("settings.json.corrupt-");
            assert_eq!(kept.len(), 1, "{tag}: {:?}", dir.names());
            assert_eq!(
                std::fs::read_to_string(dir.0.join(&kept[0])).unwrap(),
                junk,
                "{tag}: bytes preserved"
            );
            assert!(!dir.file().exists(), "{tag}: the next save starts clean");
        }
    }

    #[test]
    fn repeated_corruption_keeps_every_bad_copy() {
        let dir = Dir::new("twice");
        for junk in ["{ first", "{ second"] {
            dir.put(junk);
            load_from(&dir.0);
        }
        assert_eq!(
            dir.with_prefix("settings.json.corrupt-").len(),
            2,
            "{:?}",
            dir.names()
        );
    }

    #[test]
    fn empty_oversized_and_bom_files_are_handled() {
        let dir = Dir::new("empty");
        dir.put("");
        assert_eq!(load_from(&dir.0).settings, Settings::default());
        dir.put("  \r\n ");
        assert_eq!(load_from(&dir.0).settings, Settings::default());
        assert!(
            dir.with_prefix("settings.json.corrupt-").is_empty(),
            "an empty file has nothing worth keeping"
        );

        // A Notepad-style UTF-8 BOM in front of otherwise valid settings.
        dir.put("\u{FEFF}{ \"version\": 1, \"soundEnabled\": false }");
        assert!(!load_from(&dir.0).settings.sound_enabled);

        let big = Dir::new("big");
        big.put(&format!(
            "{{ \"version\": 1, \"pad\": \"{}\" }}",
            "x".repeat(MAX_FILE_BYTES as usize)
        ));
        assert_eq!(load_from(&big.0).settings, Settings::default());
        assert_eq!(big.with_prefix("settings.json.corrupt-").len(), 1);
    }

    #[test]
    fn leftovers_of_an_interrupted_write_do_no_harm() {
        let dir = Dir::new("interrupted");
        save_to(&dir.0, &custom()).unwrap();
        // A crash between "write the temp file" and "rename it": the real file is
        // whole, a half-written temp file sits beside it.
        std::fs::write(
            dir.0.join(format!("{FILE}.tmp-{}", std::process::id())),
            "{ \"version\": 1, \"sou",
        )
        .unwrap();
        assert_eq!(load_from(&dir.0).settings, custom());
        // The next save simply reuses the temp name and leaves none behind.
        save_to(
            &dir.0,
            &Settings {
                autostart: false,
                ..custom()
            },
        )
        .unwrap();
        assert!(
            dir.with_prefix("settings.json.tmp").is_empty(),
            "{:?}",
            dir.names()
        );
        assert!(!load_from(&dir.0).settings.autostart);
    }

    // ── Persistence failures ─────────────────────────────────────────────────

    #[test]
    fn saving_where_the_folder_cannot_exist_fails_cleanly() {
        let dir = Dir::new("blocked");
        // A file where the settings folder should be.
        let blocker = dir.0.join("Coucou");
        std::fs::write(&blocker, "I am a file").unwrap();
        assert!(save_to(&blocker, &custom()).is_err());
        assert_eq!(std::fs::read_to_string(&blocker).unwrap(), "I am a file");
        // Reading from there is still just "no settings".
        assert_eq!(load_from(&blocker).settings, Settings::default());
    }

    #[test]
    fn a_failed_write_leaves_the_previous_file_and_no_temp_file() {
        let dir = Dir::new("failwrite");
        save_to(&dir.0, &custom()).unwrap();
        let good = dir.read();

        // The temp name is taken by a directory: the write cannot even start.
        let tmp = dir.0.join(format!("{FILE}.tmp-{}", std::process::id()));
        std::fs::create_dir(&tmp).unwrap();
        assert!(save_to(&dir.0, &Settings::default()).is_err());
        assert_eq!(
            dir.read(),
            good,
            "the previous settings survive a failed save"
        );
        std::fs::remove_dir(&tmp).unwrap();

        // The rename is what fails: a directory sits where settings.json would go.
        let other = Dir::new("failrename");
        std::fs::create_dir(other.file()).unwrap();
        assert!(save_to(&other.0, &custom()).is_err());
        assert!(other.file().is_dir());
        assert!(
            other.with_prefix("settings.json.tmp").is_empty(),
            "no temp left behind: {:?}",
            other.names()
        );
    }

    #[test]
    fn a_successful_save_leaves_exactly_one_file() {
        let dir = Dir::new("clean");
        for i in 0..5 {
            save_to(
                &dir.0,
                &Settings {
                    auto_close_interval: 10.0 + i as f64,
                    ..custom()
                },
            )
            .unwrap();
        }
        assert_eq!(dir.names(), vec![FILE.to_string()]);
        assert_eq!(load_from(&dir.0).settings.auto_close_interval, 14.0);
    }

    #[test]
    fn concurrent_saves_never_interleave() {
        let dir = Dir::new("threads");
        let path = dir.0.clone();
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        save_to(
                            &path,
                            &Settings {
                                auto_close_interval: 5.0 + i as f64,
                                ..custom()
                            },
                        )
                        .unwrap();
                        let seen = load_from(&path).settings;
                        assert_eq!(
                            seen.model,
                            custom().model,
                            "a torn file would have reset it"
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(dir.names(), vec![FILE.to_string()]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_settings_folder_is_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new("private");
        let fresh = dir.0.join("config");
        save_to(&fresh, &custom()).unwrap();
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    // ── Preservation, and the one definition of the defaults ─────────────────

    #[test]
    fn changing_one_preference_keeps_all_the_others() {
        let dir = Dir::new("keep");
        save_to(&dir.0, &custom()).unwrap();
        let mut s = load_from(&dir.0).settings;
        s.sound_enabled = true;
        save_to(&dir.0, &s).unwrap();
        assert_eq!(
            load_from(&dir.0).settings,
            Settings {
                sound_enabled: true,
                ..custom()
            }
        );
    }

    /// The page needs defaults to render with before `boot` answers. Rust owns them;
    /// this fails the moment the page's copy says something different.
    #[test]
    fn the_pages_copy_of_the_defaults_matches_the_rust_ones() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../src/core/state.ts"))
                .expect("src/core/state.ts");
        let start = source
            .find("DEFAULT_SETTINGS: Settings = {")
            .expect("DEFAULT_SETTINGS in state.ts");
        let body = &source[start..];
        let body =
            &body[body.find('{').unwrap() + 1..body.find("\n};").expect("end of DEFAULT_SETTINGS")];

        // `key: value,` entries, where a value is a literal or a (multi-line) array.
        let mut object = Map::new();
        let (mut depth, mut entry, mut entries) = (0i32, String::new(), Vec::new());
        for c in body.chars() {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
            if c == ',' && depth == 0 {
                entries.push(std::mem::take(&mut entry));
            } else {
                entry.push(c);
            }
        }
        entries.push(entry);
        for entry in entries.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            let (key, value) = entry.split_once(':').expect("key: value");
            // TypeScript allows a trailing comma in an array; JSON does not.
            let value = value.trim();
            let value = match value.strip_suffix(']') {
                Some(inner) => format!("{}]", inner.trim_end().trim_end_matches(',')),
                None => value.to_string(),
            };
            let value: Value =
                serde_json::from_str(&value).unwrap_or_else(|e| panic!("{key}: {e}"));
            object.insert(key.trim().to_string(), value);
        }
        // `15` in TypeScript and `15.0` from serde are the same number.
        fn as_floats(value: Value) -> Value {
            match value {
                Value::Number(n) => serde_json::Number::from_f64(n.as_f64().unwrap())
                    .map_or(Value::Null, Value::Number),
                other => other,
            }
        }
        let ts: Map<String, Value> = object.into_iter().map(|(k, v)| (k, as_floats(v))).collect();
        let Value::Object(rust) = serde_json::to_value(Settings::default()).unwrap() else {
            unreachable!()
        };
        let rust: Map<String, Value> = rust.into_iter().map(|(k, v)| (k, as_floats(v))).collect();
        assert_eq!(
            ts, rust,
            "state.ts DEFAULT_SETTINGS drifted from Settings::default()"
        );
    }
}
