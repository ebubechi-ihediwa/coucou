// What the assistant may ask Coucou to do, and whether it may.
//
// The model proposes; this module decides what a proposal *is*. Everything the
// model sends is untrusted JSON, so an action only exists once `Action::parse` has
// accepted it, and the parser is closed: it knows three kinds, each with exactly the
// fields listed here, and anything else (an unknown kind, a field it does not
// expect, a value of the wrong type or shape) is refused, never repaired.
//
// There is no kind that takes a command line, an executable path or a file path.
// Applications are a fixed list (`AppId`), links are http(s) only and carry no
// credentials, and files are opaque ids that Rust resolves to files inside the
// inbox (see `executor.rs`). The executor parses the action again before it acts.

use std::net::{Ipv4Addr, Ipv6Addr};

use reqwest::Url;
use serde::Serialize;
use serde_json::{json, Map, Value};

/// Longest link accepted. Browsers take much more; nothing legitimate needs it.
pub const MAX_URL_LEN: usize = 2048;
const MAX_FILE_ID_LEN: usize = 32;

// ── Applications ──────────────────────────────────────────────────────────────

/// The applications the assistant may open. A closed list: the ids are the only
/// way to name one, and where each lives is decided in `executor.rs`, not by the
/// model. Each one is a program that sits at a fixed place under the Windows
/// directory. Paint is not here because on current Windows 11 it is a Store app
/// reached through a per-user alias, not a program at a trusted path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppId {
    Notepad,
    Calculator,
    FileExplorer,
}

impl AppId {
    pub const ALL: [AppId; 3] = [AppId::Notepad, AppId::Calculator, AppId::FileExplorer];

    pub fn id(self) -> &'static str {
        match self {
            AppId::Notepad => "notepad",
            AppId::Calculator => "calculator",
            AppId::FileExplorer => "file_explorer",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AppId::Notepad => "Notepad",
            AppId::Calculator => "Calculator",
            AppId::FileExplorer => "File Explorer",
        }
    }

    /// Exact match only: `Notepad`, `notepad.exe` and paths are not ids.
    fn from_id(id: &str) -> Option<AppId> {
        AppId::ALL.into_iter().find(|app| app.id() == id)
    }
}

// ── Links ─────────────────────────────────────────────────────────────────────

/// A link that is safe to hand to the default browser: http or https, a host, no
/// user name or password, no whitespace or control characters. Only the canonical
/// form (`as_str`) is ever passed on, never the text the model wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeUrl(Url);

impl SafeUrl {
    pub fn parse(text: &str) -> Result<SafeUrl, ActionError> {
        if text.len() > MAX_URL_LEN {
            return Err(ActionError::BadUrl("it is too long"));
        }
        if text.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(ActionError::BadUrl(
                "it contains spaces or control characters",
            ));
        }
        // The URL parser is forgiving about web addresses: it reads `https:///x` as
        // `https://x/`, `https:host` as `https://host/` and `\` as `/`. What the model
        // wrote should mean what it says, so only the plain written form goes through.
        let after_scheme = ["https://", "http://"]
            .iter()
            .find(|scheme| {
                text.get(..scheme.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
            })
            .map(|scheme| &text[scheme.len()..]);
        match after_scheme {
            None => {
                return Err(ActionError::BadUrl(
                    "only http and https links can be opened",
                ))
            }
            Some(rest) if rest.starts_with('/') || text.contains('\\') => {
                return Err(ActionError::BadUrl(
                    "it is not written as a plain web address",
                ))
            }
            Some(_) => {}
        }
        let url = Url::parse(text).map_err(|_| ActionError::BadUrl("it is not a valid link"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ActionError::BadUrl(
                "only http and https links can be opened",
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(ActionError::BadUrl("it has no host"));
        }
        // A model-written link must never become a request that carries credentials.
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ActionError::BadUrl(
                "links with a user name or password are not opened",
            ));
        }
        Ok(SafeUrl(url))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn host(&self) -> String {
        self.0.host_str().unwrap_or_default().to_string()
    }

    /// The host is this computer or its own network: opening the link reaches a
    /// service the user runs, not the internet.
    pub fn is_local(&self) -> bool {
        // The parser has already put a numeric host in canonical form (`0x7f.1` is
        // `127.0.0.1`), and writes an IPv6 host in brackets.
        let host = self.0.host_str().unwrap_or_default().to_ascii_lowercase();
        if let Ok(ip) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<Ipv6Addr>()
        {
            return is_local_v6(ip);
        }
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            return is_local_v4(ip);
        }
        !host.contains('.')
            || host == "localhost"
            || [".localhost", ".local", ".internal", ".lan", ".home.arpa"]
                .iter()
                .any(|suffix| host.ends_with(suffix))
    }

    /// A query or fragment can carry data to the site (a search, a tracking id, or
    /// something lifted from the conversation).
    pub fn carries_data(&self) -> bool {
        self.0.query().is_some() || self.0.fragment().is_some()
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || (a == 100 && (64..=127).contains(&b)) // carrier-grade NAT
}

fn is_local_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_local_v4(v4);
    }
    ip.is_loopback() || ip.is_unspecified() || ip.is_unique_local() || ip.is_unicast_link_local()
}

// ── Files ─────────────────────────────────────────────────────────────────────

/// An opaque handle Coucou gave the model for a file the user attached. It is not
/// a path and is resolved only by `executor::Files`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileId(String);

impl FileId {
    pub fn parse(text: &str) -> Result<FileId, ActionError> {
        let ok = !text.is_empty()
            && text.len() <= MAX_FILE_ID_LEN
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if ok {
            Ok(FileId(text.to_string()))
        } else {
            Err(ActionError::BadFileId)
        }
    }

    /// Ids are made by Coucou, never by the model.
    pub fn issued(n: usize) -> FileId {
        FileId(format!("f{n}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ── The action ────────────────────────────────────────────────────────────────

// The variants mirror the action types the model names (`open_app`, `open_url`, `open_file`).
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    OpenApp(AppId),
    OpenUrl(SafeUrl),
    OpenFile(FileId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    NotAnObject,
    MissingType,
    UnknownType(String),
    MissingField(&'static str),
    WrongType(&'static str),
    UnexpectedField(String),
    UnknownApp,
    BadUrl(&'static str),
    BadFileId,
    /// An action that parsed once but does not parse to itself again.
    Inconsistent,
}

impl ActionError {
    /// For the island and the log. It never repeats what the model wrote beyond a
    /// few harmless characters of a name.
    pub fn message(&self) -> String {
        match self {
            ActionError::NotAnObject => "The proposed action was not understood.".into(),
            ActionError::MissingType => "The proposed action had no type.".into(),
            ActionError::UnknownType(kind) => format!("Coucou can't do \"{kind}\"."),
            ActionError::MissingField(field) => {
                format!("The proposed action is missing \"{field}\".")
            }
            ActionError::WrongType(field) => {
                format!("The proposed action has an invalid \"{field}\".")
            }
            ActionError::UnexpectedField(field) => {
                format!("The proposed action has an unexpected \"{field}\".")
            }
            ActionError::UnknownApp => "That application is not one Coucou can open.".into(),
            ActionError::BadUrl(why) => format!("That link can't be opened: {why}."),
            ActionError::BadFileId => "That file is not available.".into(),
            ActionError::Inconsistent => "The proposed action was not valid.".into(),
        }
    }
}

/// A few harmless characters of something the model wrote, for an error message.
fn shorten(text: &str) -> String {
    text.chars()
        .take(24)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '?'
            }
        })
        .collect()
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), ActionError> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(ActionError::UnexpectedField(shorten(key))),
        None => Ok(()),
    }
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, ActionError> {
    object
        .get(name)
        .ok_or(ActionError::MissingField(name))?
        .as_str()
        .ok_or(ActionError::WrongType(name))
}

impl Action {
    /// The one way an action comes into being from the model's output.
    pub fn parse(value: &Value) -> Result<Action, ActionError> {
        let object = value.as_object().ok_or(ActionError::NotAnObject)?;
        let kind = object
            .get("type")
            .ok_or(ActionError::MissingType)?
            .as_str()
            .ok_or(ActionError::WrongType("type"))?;
        match kind {
            "open_app" => {
                only_keys(object, &["type", "app"])?;
                AppId::from_id(string_field(object, "app")?)
                    .map(Action::OpenApp)
                    .ok_or(ActionError::UnknownApp)
            }
            "open_url" => {
                only_keys(object, &["type", "url"])?;
                SafeUrl::parse(string_field(object, "url")?).map(Action::OpenUrl)
            }
            "open_file" => {
                only_keys(object, &["type", "fileId"])?;
                FileId::parse(string_field(object, "fileId")?).map(Action::OpenFile)
            }
            other => Err(ActionError::UnknownType(shorten(other))),
        }
    }

    /// The canonical form: what `parse` accepts, nothing else.
    pub fn to_json(&self) -> Value {
        match self {
            Action::OpenApp(app) => json!({ "type": "open_app", "app": app.id() }),
            Action::OpenUrl(url) => json!({ "type": "open_url", "url": url.as_str() }),
            Action::OpenFile(id) => json!({ "type": "open_file", "fileId": id.as_str() }),
        }
    }

    /// Checks the action again, from scratch: it must parse back to itself. The
    /// executor calls this before acting, so an `Action` that was built or changed
    /// anywhere along the way is still checked by the code that matters.
    pub fn validate(&self) -> Result<(), ActionError> {
        match Action::parse(&self.to_json()) {
            Ok(again) if again == *self => Ok(()),
            Ok(_) => Err(ActionError::Inconsistent),
            Err(err) => Err(err),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Action::OpenApp(_) => "open_app",
            Action::OpenUrl(_) => "open_url",
            Action::OpenFile(_) => "open_file",
        }
    }

    pub fn risk(&self) -> Risk {
        match self {
            Action::OpenApp(_) | Action::OpenFile(_) => Risk::Low,
            // A link to somewhere on the user's own network can trigger things there,
            // and a query string can carry data out; neither is "just open a page".
            Action::OpenUrl(url) if url.is_local() || url.carries_data() => Risk::Medium,
            Action::OpenUrl(_) => Risk::Low,
        }
    }

    /// What to show the person. `file_name` resolves a file id to its display name.
    pub fn view(&self, file_name: &dyn Fn(&FileId) -> Option<String>) -> ActionView {
        let (title, target) = match self {
            Action::OpenApp(app) => (format!("Open {}", app.label()), None),
            Action::OpenUrl(url) => (
                format!("Open {} in your browser", url.host()),
                Some(url.as_str().to_string()),
            ),
            Action::OpenFile(id) => {
                let name = file_name(id).unwrap_or_else(|| "an attached file".into());
                (format!("Open {name}"), Some(name))
            }
        };
        ActionView {
            kind: self.kind(),
            title,
            target,
            risk: self.risk(),
        }
    }
}

/// What the island displays for a proposal: the action, said plainly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActionView {
    pub kind: &'static str,
    pub title: String,
    pub target: Option<String>,
    pub risk: Risk,
}

// ── Policy ────────────────────────────────────────────────────────────────────

/// How much harm an action can do if it is not what the person wanted. Only the
/// first two are produced by the three actions that exist; the ladder is here so
/// that the kinds that need real care later (deleting or changing a file, sending a
/// message, running a command, buying, changing system settings) have somewhere to
/// stand that is never "just run it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Opens something and changes nothing.
    Low,
    /// Reaches out, or could be steered toward somewhere unexpected.
    Medium,
    /// Changes or sends something, or can't be undone.
    #[allow(dead_code)] // no action is this risky yet; see the note above
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyConfig {
    /// Run low-risk actions without asking. Off: the first slice asks every time, so
    /// the whole proposal → approval → result loop is what gets exercised. It can
    /// never apply above `Risk::Low`.
    pub auto_approve_low_risk: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Run it without asking.
    Allow,
    /// Show it and run it only if the person approves.
    Ask(Risk),
    /// Do not run it, and say why.
    Refuse(String),
}

/// The decision for an action that has already been parsed. Invalid is refused
/// here too, so a caller cannot reach `Allow` or `Ask` with a bad action.
pub fn decide(action: &Action, config: &PolicyConfig) -> Verdict {
    if let Err(err) = action.validate() {
        return Verdict::Refuse(err.message());
    }
    match action.risk() {
        Risk::Low if config.auto_approve_low_risk => Verdict::Allow,
        risk => Verdict::Ask(risk),
    }
}

/// Raw model output to a decision. Anything that does not parse is refused: the
/// policy fails closed.
pub fn assess(raw: &Value, config: &PolicyConfig) -> Result<(Action, Verdict), Verdict> {
    match Action::parse(raw) {
        Ok(action) => {
            let verdict = decide(&action, config);
            Ok((action, verdict))
        }
        Err(err) => Err(Verdict::Refuse(err.message())),
    }
}

// ── The tool the model is given ───────────────────────────────────────────────

pub const TOOL_NAME: &str = "propose_action";

/// The client tool offered to the model. The model's only way to ask for something
/// is to call this; what it sends back is parsed by `Action::parse` like any other
/// untrusted input, whatever this schema says.
pub fn tool_definition() -> Value {
    let apps: Vec<&str> = AppId::ALL.iter().map(|app| app.id()).collect();
    json!({
        "name": TOOL_NAME,
        "description": format!(
            "Propose one action for Coucou to perform on the user's computer. The user is shown \
             the action and must approve it before anything happens, so propose it only when the \
             user clearly asked for it, and do not say it has been done until you are told it \
             was. Kinds: open_app (open one of these applications: {}), open_url (open an http or \
             https link in the default browser), open_file (open a file the user attached, by \
             the fileId given with it). Nothing else can be done.",
            apps.join(", ")
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "type": { "type": "string", "enum": ["open_app", "open_url", "open_file"] },
                "app": { "type": "string", "enum": apps },
                "url": { "type": "string", "description": "An http or https link, without a user name or password." },
                "fileId": { "type": "string", "description": "The id of an attached file." }
            },
            "required": ["type"],
            "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: Value) -> Result<Action, ActionError> {
        Action::parse(&value)
    }

    // ── The schema ───────────────────────────────────────────────────────────

    #[test]
    fn the_three_actions_parse_and_round_trip() {
        for (raw, expected) in [
            (
                json!({ "type": "open_app", "app": "notepad" }),
                Action::OpenApp(AppId::Notepad),
            ),
            (
                json!({ "type": "open_app", "app": "file_explorer" }),
                Action::OpenApp(AppId::FileExplorer),
            ),
            (
                json!({ "type": "open_url", "url": "https://example.com/docs?page=2" }),
                Action::OpenUrl(SafeUrl::parse("https://example.com/docs?page=2").unwrap()),
            ),
            (
                json!({ "type": "open_file", "fileId": "f1" }),
                Action::OpenFile(FileId::parse("f1").unwrap()),
            ),
        ] {
            let action = parse(raw.clone()).unwrap();
            assert_eq!(action, expected);
            assert_eq!(
                action.to_json(),
                raw,
                "the canonical form is what was accepted"
            );
            assert_eq!(action.validate(), Ok(()));
        }
    }

    #[test]
    fn something_that_is_not_an_object_with_a_string_type_is_refused() {
        for raw in [
            json!(null),
            json!("open_app"),
            json!(["open_app"]),
            json!(42),
            json!(true),
        ] {
            assert_eq!(parse(raw), Err(ActionError::NotAnObject));
        }
        assert_eq!(parse(json!({})), Err(ActionError::MissingType));
        assert_eq!(
            parse(json!({ "app": "notepad" })),
            Err(ActionError::MissingType)
        );
        for raw in [
            json!({ "type": 1 }),
            json!({ "type": null }),
            json!({ "type": ["open_app"] }),
            json!({ "type": { "name": "open_app" } }),
        ] {
            assert_eq!(parse(raw), Err(ActionError::WrongType("type")));
        }
    }

    #[test]
    fn unknown_action_types_are_refused() {
        for kind in [
            "run_command",
            "shell",
            "exec",
            "execute",
            "powershell",
            "cmd",
            "delete_file",
            "write_file",
            "send_email",
            "click",
            "type_text",
            "screenshot",
            "download",
            "install",
            // Near misses are not the real thing: the match is exact.
            "OPEN_APP",
            "Open_App",
            "open_app ",
            " open_app",
            "open-app",
            "openapp",
            "open_app\0",
            "",
        ] {
            let err = parse(json!({ "type": kind, "app": "notepad" })).unwrap_err();
            assert!(
                matches!(err, ActionError::UnknownType(_)),
                "{kind:?} -> {err:?}"
            );
        }
    }

    #[test]
    fn unexpected_parameters_are_refused_not_ignored() {
        // The classic ways to smuggle something through a tolerant parser.
        for raw in [
            json!({ "type": "open_app", "app": "notepad", "args": "C:\\secret.txt" }),
            json!({ "type": "open_app", "app": "notepad", "command": "calc.exe" }),
            json!({ "type": "open_app", "app": "notepad", "path": "C:\\Windows\\System32\\cmd.exe" }),
            json!({ "type": "open_app", "app": "notepad", "cwd": "C:\\" }),
            json!({ "type": "open_url", "url": "https://example.com", "headers": { "Authorization": "x" } }),
            json!({ "type": "open_url", "url": "https://example.com", "app": "notepad" }),
            json!({ "type": "open_file", "fileId": "f1", "path": "C:\\Windows\\win.ini" }),
            json!({ "type": "open_file", "fileId": "f1", "app": "cmd" }),
            // A field of another action is unexpected here, even when the right one is there.
            json!({ "type": "open_app", "app": "notepad", "url": "https://example.com" }),
        ] {
            assert!(
                matches!(parse(raw.clone()), Err(ActionError::UnexpectedField(_))),
                "{raw}"
            );
        }
    }

    #[test]
    fn missing_and_wrongly_typed_parameters_are_refused() {
        assert_eq!(
            parse(json!({ "type": "open_app" })),
            Err(ActionError::MissingField("app"))
        );
        assert_eq!(
            parse(json!({ "type": "open_url" })),
            Err(ActionError::MissingField("url"))
        );
        assert_eq!(
            parse(json!({ "type": "open_file" })),
            Err(ActionError::MissingField("fileId"))
        );
        for bad in [
            json!(5),
            json!(null),
            json!(["notepad"]),
            json!({ "id": "notepad" }),
            json!(true),
        ] {
            assert_eq!(
                parse(json!({ "type": "open_app", "app": bad })),
                Err(ActionError::WrongType("app"))
            );
            assert_eq!(
                parse(json!({ "type": "open_url", "url": bad })),
                Err(ActionError::WrongType("url"))
            );
            assert_eq!(
                parse(json!({ "type": "open_file", "fileId": bad })),
                Err(ActionError::WrongType("fileId"))
            );
        }
    }

    // ── Applications ─────────────────────────────────────────────────────────

    #[test]
    fn only_the_listed_applications_can_be_named_and_only_by_id() {
        for app in AppId::ALL {
            assert_eq!(
                parse(json!({ "type": "open_app", "app": app.id() })),
                Ok(Action::OpenApp(app))
            );
        }
        for bad in [
            "cmd",
            "powershell",
            "pwsh",
            "wt",
            "regedit",
            "taskmgr",
            "mshta",
            "wscript",
            "explorer",
            "notepad.exe",
            "Notepad",
            "NOTEPAD",
            " notepad",
            "notepad ",
            "notepad\n",
            "C:\\Windows\\System32\\notepad.exe",
            "C:\\Windows\\System32\\cmd.exe",
            "\\\\server\\share\\x.exe",
            "..\\..\\notepad",
            "../notepad",
            "notepad && calc",
            "notepad; calc",
            "notepad|calc",
            "notepad\" /c calc",
            "%SystemRoot%\\notepad.exe",
            "$(calc)",
            "`calc`",
            "",
        ] {
            assert_eq!(
                parse(json!({ "type": "open_app", "app": bad })),
                Err(ActionError::UnknownApp),
                "{bad:?}"
            );
        }
    }

    // ── Links ────────────────────────────────────────────────────────────────

    #[test]
    fn ordinary_web_links_are_accepted_in_their_canonical_form() {
        for (given, canonical) in [
            ("https://example.com", "https://example.com/"),
            (
                "http://example.com/a/b?c=d#e",
                "http://example.com/a/b?c=d#e",
            ),
            ("HTTPS://EXAMPLE.COM/Path", "https://example.com/Path"),
            ("https://example.com:8443/x", "https://example.com:8443/x"),
            (
                "https://sub.example.co.uk/%E2%9C%93",
                "https://sub.example.co.uk/%E2%9C%93",
            ),
        ] {
            assert_eq!(
                SafeUrl::parse(given).unwrap().as_str(),
                canonical,
                "{given}"
            );
        }
    }

    #[test]
    fn links_that_are_not_plain_web_links_are_refused() {
        for bad in [
            // Other schemes: scripts, local files, data, other protocol handlers.
            "javascript:alert(1)",
            "file:///C:/Windows/System32/cmd.exe",
            "file://server/share/x.exe",
            "data:text/html,<script>1</script>",
            "ftp://example.com/x",
            "ws://example.com",
            "mailto:a@b.c",
            "ms-msdt:/id PCWDiagnostic",
            "ms-settings:privacy",
            "calculator:",
            "search-ms:query=x",
            "vscode://file/C:/x",
            "\\\\server\\share\\x.exe",
            "C:\\Windows\\System32\\cmd.exe",
            // Not links at all.
            "example.com",
            "www.example.com",
            "//example.com",
            "https://",
            "http://",
            "",
            "not a url",
            // Forms the URL parser would quietly rewrite into a different address.
            "https:///path",
            "https:////example.com",
            "https:/example.com",
            "https:example.com",
            "http:example.com",
            "https:\\\\example.com",
            "https://example.com\\@evil.com",
            "https://example.com/a\\b",
            "https:\\/example.com",
            // Credentials in the link.
            "https://user:pw@example.com",
            "https://user@example.com",
            "https://:pw@example.com",
            "http://admin:admin@192.168.1.1/",
            // Whitespace and control characters, including ones that end a command line.
            " https://example.com",
            "https://example.com ",
            "https://exa mple.com",
            "https://example.com/a b",
            "https://example.com\n",
            "https://example.com\r\nHost: evil",
            "https://example.com\0",
            "https://example.com/\u{7}",
            "https://example.com/\t",
        ] {
            assert!(
                matches!(SafeUrl::parse(bad), Err(ActionError::BadUrl(_))),
                "{bad:?} was accepted as {:?}",
                SafeUrl::parse(bad).map(|u| u.as_str().to_string())
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(matches!(SafeUrl::parse(&long), Err(ActionError::BadUrl(_))));
        assert!(SafeUrl::parse(&format!(
            "https://example.com/{}",
            "a".repeat(MAX_URL_LEN - 21)
        ))
        .is_ok());
    }

    #[test]
    fn command_characters_in_a_link_stay_inside_the_link() {
        // `&`, `|`, `;` are legal in a path or query; what reaches the launcher is
        // the canonical link, which has no space or quote to end an argument with.
        let url = SafeUrl::parse("https://example.com/a&calc.exe|b;c?x=1&y=%22quoted%22").unwrap();
        assert!(url.as_str().starts_with("https://example.com/"));
        assert!(!url.as_str().contains(char::is_whitespace));
        assert!(!url.as_str().contains('"'));
        // The ones that could end an argument are encoded by the parser.
        assert_eq!(
            SafeUrl::parse("https://example.com/a'b<c>d")
                .unwrap()
                .as_str(),
            "https://example.com/a'b%3Cc%3Ed"
        );
        assert_eq!(
            SafeUrl::parse("https://example.com/\"&calc")
                .unwrap()
                .as_str(),
            "https://example.com/%22&calc"
        );
    }

    #[test]
    fn links_to_this_computer_or_its_network_are_recognised() {
        for local in [
            "http://localhost:3000/",
            "http://LOCALHOST/",
            "http://app.localhost/",
            "http://127.0.0.1/",
            "http://127.1.2.3:8080/",
            "http://10.0.0.5/",
            "http://192.168.1.1/admin",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://100.64.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://router/",
            "http://nas.local/",
            "http://printer.lan/",
            "http://host.internal/",
            "http://box.home.arpa/",
        ] {
            assert!(SafeUrl::parse(local).unwrap().is_local(), "{local}");
        }
        for public in [
            "https://example.com/",
            "http://93.184.216.34/",
            "http://[2001:db8::1]/",
            "https://sub.example.co.uk/",
            "http://172.32.0.1/",
            "http://100.63.0.1/",
        ] {
            assert!(!SafeUrl::parse(public).unwrap().is_local(), "{public}");
        }
    }

    // ── Files ────────────────────────────────────────────────────────────────

    #[test]
    fn file_ids_are_opaque_handles_never_paths() {
        for ok in ["f1", "f23", "file-1", "a_b", &"a".repeat(MAX_FILE_ID_LEN)] {
            assert!(FileId::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "F1",
            "f 1",
            "f1\n",
            "../f1",
            "..",
            ".",
            "f1/../x",
            "f1\\x",
            "C:\\Windows\\win.ini",
            "C:/x",
            "/etc/passwd",
            "\\\\server\\share",
            "f1.txt",
            "f1%2e%2e",
            "f1;calc",
            "é",
            &"a".repeat(MAX_FILE_ID_LEN + 1),
        ] {
            assert_eq!(FileId::parse(bad), Err(ActionError::BadFileId), "{bad:?}");
        }
        assert_eq!(FileId::issued(7).as_str(), "f7");
    }

    // ── Risk and policy ──────────────────────────────────────────────────────

    #[test]
    fn risk_follows_what_the_action_can_reach() {
        let risk = |raw: Value| parse(raw).unwrap().risk();
        assert_eq!(
            risk(json!({ "type": "open_app", "app": "notepad" })),
            Risk::Low
        );
        assert_eq!(
            risk(json!({ "type": "open_file", "fileId": "f1" })),
            Risk::Low
        );
        assert_eq!(
            risk(json!({ "type": "open_url", "url": "https://example.com/" })),
            Risk::Low
        );
        // Somewhere on the user's own network, or a link that can carry data out.
        assert_eq!(
            risk(json!({ "type": "open_url", "url": "http://192.168.1.1/admin" })),
            Risk::Medium
        );
        assert_eq!(
            risk(json!({ "type": "open_url", "url": "http://localhost:8080/" })),
            Risk::Medium
        );
        assert_eq!(
            risk(json!({ "type": "open_url", "url": "https://example.com/?q=something" })),
            Risk::Medium
        );
        assert_eq!(
            risk(json!({ "type": "open_url", "url": "https://example.com/#token" })),
            Risk::Medium
        );
        assert!(Risk::Low < Risk::Medium && Risk::Medium < Risk::High);
    }

    #[test]
    fn the_default_policy_asks_for_everything() {
        let config = PolicyConfig::default();
        assert!(!config.auto_approve_low_risk);
        for raw in [
            json!({ "type": "open_app", "app": "notepad" }),
            json!({ "type": "open_url", "url": "https://example.com/" }),
            json!({ "type": "open_file", "fileId": "f1" }),
            json!({ "type": "open_url", "url": "http://localhost/" }),
        ] {
            let (_, verdict) = assess(&raw, &config).unwrap();
            assert!(matches!(verdict, Verdict::Ask(_)), "{raw}: {verdict:?}");
        }
    }

    #[test]
    fn auto_approval_reaches_only_low_risk_actions() {
        let config = PolicyConfig {
            auto_approve_low_risk: true,
        };
        let verdict = |raw: Value| assess(&raw, &config).unwrap().1;
        assert_eq!(
            verdict(json!({ "type": "open_app", "app": "calculator" })),
            Verdict::Allow
        );
        assert_eq!(
            verdict(json!({ "type": "open_url", "url": "https://example.com/" })),
            Verdict::Allow
        );
        // Never above Low, whatever the setting.
        assert_eq!(
            verdict(json!({ "type": "open_url", "url": "http://localhost/" })),
            Verdict::Ask(Risk::Medium)
        );
        assert_eq!(
            verdict(json!({ "type": "open_url", "url": "https://example.com/?q=1" })),
            Verdict::Ask(Risk::Medium)
        );
    }

    #[test]
    fn anything_that_does_not_parse_is_refused_whatever_the_policy_says() {
        for config in [
            PolicyConfig::default(),
            PolicyConfig {
                auto_approve_low_risk: true,
            },
        ] {
            for raw in [
                json!({ "type": "run_command", "command": "calc" }),
                json!({ "type": "open_app", "app": "cmd" }),
                json!({ "type": "open_url", "url": "javascript:alert(1)" }),
                json!({ "type": "open_file", "fileId": "../../x" }),
                json!({ "type": "open_app", "app": "notepad", "args": "x" }),
                json!(null),
            ] {
                match assess(&raw, &config) {
                    Err(Verdict::Refuse(reason)) => assert!(!reason.is_empty()),
                    other => panic!("{raw} was not refused: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn the_error_messages_say_what_is_wrong_without_repeating_what_was_sent() {
        let long_name = "x".repeat(500);
        let err = parse(json!({ "type": long_name })).unwrap_err();
        assert!(err.message().chars().count() < 80, "{}", err.message());
        // Control characters and shell syntax in a name are not echoed.
        let err = parse(json!({ "type": "calc\n&& del *" })).unwrap_err();
        let message = err.message();
        assert!(
            !message.contains('\n') && !message.contains('&') && !message.contains('*'),
            "{message}"
        );
        let err =
            parse(json!({ "type": "open_app", "app": "notepad", "k\u{1b}ey": 1 })).unwrap_err();
        assert!(!err.message().contains('\u{1b}'));
    }

    #[test]
    fn an_action_that_was_changed_after_parsing_is_caught_by_validate() {
        // The only way to get an invalid `Action` is to go around `parse`; the
        // executor's second look catches it anyway.
        let forged = Action::OpenFile(FileId("../../secret".into()));
        assert_eq!(forged.validate(), Err(ActionError::BadFileId));
        assert!(matches!(
            decide(
                &forged,
                &PolicyConfig {
                    auto_approve_low_risk: true
                }
            ),
            Verdict::Refuse(_)
        ));
        let forged = Action::OpenUrl(SafeUrl(Url::parse("file:///C:/Windows/win.ini").unwrap()));
        assert!(forged.validate().is_err());
        let forged = Action::OpenUrl(SafeUrl(Url::parse("https://user:pw@example.com/").unwrap()));
        assert!(forged.validate().is_err());
    }

    // ── What the person sees ─────────────────────────────────────────────────

    #[test]
    fn the_proposal_says_plainly_what_will_happen() {
        let names = |id: &FileId| (id.as_str() == "f1").then(|| "report.pdf".to_string());
        let view = |raw: Value| parse(raw).unwrap().view(&names);

        let v = view(json!({ "type": "open_app", "app": "notepad" }));
        assert_eq!(
            (v.title.as_str(), v.target.as_deref()),
            ("Open Notepad", None)
        );

        let v = view(json!({ "type": "open_url", "url": "https://example.com/a?b=1" }));
        assert_eq!(v.title, "Open example.com in your browser");
        assert_eq!(
            v.target.as_deref(),
            Some("https://example.com/a?b=1"),
            "the whole link is shown"
        );
        assert_eq!(v.risk, Risk::Medium);

        let v = view(json!({ "type": "open_file", "fileId": "f1" }));
        assert_eq!(
            (v.title.as_str(), v.target.as_deref()),
            ("Open report.pdf", Some("report.pdf"))
        );
        let v = view(json!({ "type": "open_file", "fileId": "f9" }));
        assert_eq!(v.title, "Open an attached file");
        assert_eq!(serde_json::to_value(&v).unwrap()["kind"], "open_file");
        assert_eq!(serde_json::to_value(&v).unwrap()["risk"], "low");
    }

    // ── The tool ─────────────────────────────────────────────────────────────

    #[test]
    fn the_tool_offered_to_the_model_matches_what_the_parser_accepts() {
        let tool = tool_definition();
        assert_eq!(tool["name"], TOOL_NAME);
        let schema = &tool["input_schema"];
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["required"], json!(["type"]));
        let kinds: Vec<&str> = schema["properties"]["type"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["open_app", "open_url", "open_file"]);
        let apps: Vec<&str> = schema["properties"]["app"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(apps, AppId::ALL.map(AppId::id), "one list, two uses");
        // Each kind the schema names parses, and the description lists the real apps.
        for app in AppId::ALL {
            assert!(tool["description"].as_str().unwrap().contains(app.id()));
        }
        assert!(parse(json!({ "type": "open_app", "app": apps[0] })).is_ok());
    }
}
