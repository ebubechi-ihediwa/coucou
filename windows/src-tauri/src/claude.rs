// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks.
//
// Everything happens here rather than in the island: the API key never leaves
// the Credential Manager, and file bytes never cross the IPC boundary.

use std::sync::Mutex;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::actions;
use crate::assistant::{RawProposal, ToolResult};
use crate::executor::Files;
use crate::{files, http, log, secrets};

const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Server-side fallback: on a policy decline the API retries the same request on
/// a fallback model inside the same call, so the island never shows a dead end.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 4096;
/// Text and code files are inlined; anything larger is skipped, as on macOS.
const MAX_INLINE_TEXT: u64 = 200_000;

pub const DEFAULT_MODEL: &str = "claude-opus-5";

const SYSTEM_PROMPT: &str = "You are Mochi, a personal AI assistant living at the top of the user's screen. \
You have web search access and can help with absolutely anything — research, coding, finding places, recommendations, tasks, questions. \
Respond in the user's language. Be thorough and complete — use as much detail as the task requires. \
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks. \
You can also act on the user's computer, within narrow limits: when the user clearly asks you to open an application, a web page or a file they attached, call the propose_action tool. \
Coucou shows the user exactly what you propose and does it only if they approve, then tells you how it went. Never say something has been done before you are told so. \
If a request needs anything the tool cannot do, say so in plain words instead.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history, including tool_use / tool_result blocks.
    messages: Mutex<Vec<Value>>,
    /// Answers owed to the model's tool uses, carried by the next user message.
    /// The API refuses a conversation in which a tool use has no answer.
    tool_results: Mutex<Vec<ToolResult>>,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
        self.tool_results.lock().unwrap().clear();
    }

    pub fn add_tool_result(&self, result: ToolResult) {
        self.tool_results.lock().unwrap().push(result);
    }

    pub(crate) fn take_tool_results(&self) -> Vec<ToolResult> {
        std::mem::take(&mut *self.tool_results.lock().unwrap())
    }

    /// Puts back what a failed request had taken, ahead of anything added since.
    fn restore_tool_results(&self, mut taken: Vec<ToolResult>) {
        let mut held = self.tool_results.lock().unwrap();
        taken.append(&mut held);
        *held = taken;
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    pub(crate) fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.messages.lock().unwrap().pop();
    }

    pub(crate) fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

/// What a chat turn produced: the model's words, and the action it asked for, if
/// any. The action is still untrusted; the runtime decides what becomes of it.
pub struct ModelReply {
    pub text: String,
    pub proposal: Option<RawProposal>,
}

/// One chat turn. Returns the assistant's text and any proposed action, or a
/// message the island shows in the note view.
pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
    files: &Files,
) -> Result<ModelReply, String> {
    // Answers owed from the last turn go first. Unless the turn completes they are
    // put back, so the conversation stays one the API will accept; see `TurnGuard`.
    let owed = chat.take_tool_results();
    let mut guard = TurnGuard { chat, owed: owed.clone(), pushed: false, done: false };
    send_turn(chat, model, query, context, files, &owed, &mut guard).await
}

/// Keeps the conversation consistent however a turn ends. If the turn fails, or its
/// future is simply dropped because the person cancelled mid-request, the user
/// message it added is removed and the answers it took are given back; otherwise
/// the next request would carry a message the model never answered, or lose a
/// tool result the API insists on.
struct TurnGuard<'a> {
    chat: &'a Chat,
    owed: Vec<ToolResult>,
    pushed: bool,
    done: bool,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if self.pushed {
            self.chat.pop();
        }
        self.chat.restore_tool_results(std::mem::take(&mut self.owed));
    }
}

async fn send_turn(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
    files: &Files,
    owed: &[ToolResult],
    guard: &mut TurnGuard<'_>,
) -> Result<ModelReply, String> {
    let key = secrets::get("anthropic-api-key")
        .ok_or_else(|| "API key missing. Open settings.".to_string())?;

    let mut content: Vec<Value> = tool_result_blocks(owed);

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                // An Err returns before anything is pushed: the history stays as it was.
                if let Some(block) = file_block(path)? {
                    content.push(block);
                }
                // The model gets an opaque id for the file, never its path.
                let note = match files.register(std::path::Path::new(path)) {
                    Ok((id, _)) => format!(
                        "File: {name} (fileId: {}; Coucou can open it for the user with open_file)",
                        id.as_str()
                    ),
                    Err(_) => format!("File: {name}"),
                };
                content.push(json!({ "type": "text", "text": note }));
            }
            Some(ChatContext::Window { app_name, title, url }) => {
                let mut text = format!("Context — App: {app_name}, Window: {title}");
                if let Some(url) = url {
                    text.push_str(&format!(", URL: {url}"));
                }
                content.push(json!({ "type": "text", "text": text }));
            }
            None => {}
        }
    }
    content.push(json!({ "type": "text", "text": query }));

    chat.push(json!({ "role": "user", "content": content }));
    guard.pushed = true;

    let body = request_body(model, chat.snapshot());

    // From here an early return (or a dropped future) undoes the user message, via
    // the guard, so the history stays consistent with what the model saw.
    let response = call(&key, &body).await?;

    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        let why = response
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .unwrap_or("Claude declined this one.");
        return Err(why.to_string());
    }

    let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
        return Err("Unexpected API response.".into());
    };

    // Store the whole content — tool_use / tool_result blocks included — so the
    // next turn has the right context. The exchange is kept from here on.
    chat.push(json!({ "role": "assistant", "content": blocks.clone() }));
    guard.done = true;

    let Interpreted { text, proposal, unanswered } = interpret(&blocks);
    // Any tool use but the one proposal still owes the model an answer.
    for result in unanswered {
        chat.add_tool_result(result);
    }

    if text.is_empty() && proposal.is_none() {
        return Err("No response text.".into());
    }
    Ok(ModelReply { text, proposal })
}

/// The basic web search. The newer `web_search_20260209` filters results by running
/// code, which the fallback model does not support, so with `"fallbacks": "default"`
/// the API refuses the whole request before any model answers.
const WEB_SEARCH_TOOL: &str = "web_search_20250305";

/// The request for one turn: the conversation, the web search the model has always
/// had, and the single tool through which it may ask Coucou to act.
pub(crate) fn request_body(model: &str, messages: Vec<Value>) -> Value {
    json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": SYSTEM_PROMPT,
        "tools": [
            { "type": WEB_SEARCH_TOOL, "name": "web_search", "max_uses": 5 },
            actions::tool_definition(),
        ],
        "fallbacks": "default",
        "messages": messages,
    })
}

pub(crate) fn tool_result_blocks(results: &[ToolResult]) -> Vec<Value> {
    results
        .iter()
        .map(|r| json!({ "type": "tool_result", "tool_use_id": r.tool_use_id, "content": r.text }))
        .collect()
}

pub(crate) struct Interpreted {
    pub text: String,
    pub proposal: Option<RawProposal>,
    /// Tool uses that will not be acted on, with the answer to give for each.
    pub unanswered: Vec<ToolResult>,
}

/// Reads a response's content blocks. The first `propose_action` call is the
/// proposal. A second one, or a call to any other client tool, is not acted on and
/// is answered at once, so that the conversation stays well formed. What the model
/// put in a call is not looked at here at all; that is `actions::Action::parse`'s job.
pub(crate) fn interpret(blocks: &[Value]) -> Interpreted {
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    let mut proposal = None;
    let mut unanswered = Vec::new();
    for block in blocks.iter().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")) {
        // A call with no id cannot be answered; the API would not have produced one.
        let Some(id) = block.get("id").and_then(Value::as_str) else { continue };
        let is_ours = block.get("name").and_then(Value::as_str) == Some(actions::TOOL_NAME);
        if is_ours && proposal.is_none() {
            proposal = Some(RawProposal { tool_use_id: id.to_string(), input: block.get("input").cloned().unwrap_or(Value::Null) });
        } else {
            let why = if is_ours {
                "Only one action can be proposed at a time, so this one was not performed."
            } else {
                "That tool is not available, so nothing was done."
            };
            unanswered.push(ToolResult { tool_use_id: id.to_string(), text: why.into() });
        }
    }
    Interpreted { text, proposal, unanswered }
}

async fn call(key: &str, body: &Value) -> Result<Value, String> {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("?");
    let failed = |e: http::ApiError| {
        let shown = chat_error(&e);
        log::line(failure_line(model, &shown));
        shown
    };
    let client = http::chat_client().map_err(failed)?;
    call_at(&client, ENDPOINT, key, body, &http::CHAT_RETRY)
        .await
        .map_err(failed)
}

/// What the log keeps of a failed request: the model and the same words the island
/// shows. The API's own explanation (an unsupported tool, an unknown model) is what
/// says why a request was rejected, and the island cuts it short. The text carries
/// neither the key nor the response body: `http` removes the key from the API's
/// message before it is kept, and `chat_error` never includes the body or the URL.
fn failure_line(model: &str, shown: &str) -> String {
    format!("chat request failed (model {model}): {shown}")
}

/// One message request. A message changes nothing on the server, so the
/// transient failures (rate limit, overload, no connection) are asked again a
/// couple of times; a bad key, a bad request or a timeout are not.
async fn call_at(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    body: &Value,
    retry: &http::Retry,
) -> Result<Value, http::ApiError> {
    let key = http::secret(key)?;
    let bytes = http::send_retrying(
        || {
            client
                .post(endpoint)
                .header("x-api-key", key.clone())
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", FALLBACK_BETA)
                .json(body)
        },
        http::CHAT_MAX_BODY,
        retry,
    )
    .await?;
    serde_json::from_slice(&bytes).map_err(|_| http::ApiError::Malformed)
}

/// What the note view says when a chat request fails: which kind of failure it
/// was and what to do about it, never the key, the URL or the raw body.
fn chat_error(err: &http::ApiError) -> String {
    use http::ApiError::*;
    match err {
        Status { code: 401, .. } => "API key rejected (401). Check it in Settings.".into(),
        Status { code: 403, message, .. } => match message {
            Some(m) => format!("Claude refused this request (403): {m}"),
            None => "Claude refused this request (403). The key may lack access to this model.".into(),
        },
        Status { code: 429, retry_after, .. } => match retry_after {
            Some(wait) => format!("Claude is rate limiting requests (429). Try again in {} s.", wait.as_secs().max(1)),
            None => "Claude is rate limiting requests (429). Try again in a minute.".into(),
        },
        Status { code, .. } if *code >= 500 => {
            format!("Claude is overloaded or unavailable ({code}). Try again in a moment.")
        }
        // The API's own words are what explain a bad request or an unknown model.
        Status { code, message: Some(m), .. } => format!("Claude API {code}: {m}"),
        Status { code, .. } => format!("Claude API {code}"),
        Timeout => "Claude took too long to answer. Try again.".into(),
        Connect => "No connection. Check your network and try again.".into(),
        Transport => "The connection to Claude broke. Try again.".into(),
        TooLarge => "Claude's reply was too large to read.".into(),
        Malformed => "Bad API response.".into(),
    }
}

/// PDF → document block, image → image block, text/code → inline text.
/// Mirrors readFileAsBlock() in ClaudeService.swift.
///
/// `path` comes from the page, so it is only ever read through `files`, which
/// serves nothing from outside the inbox and nothing above a size limit. What the
/// user must hear about is an `Err`; a file that is simply gone, unreadable or too
/// long to inline is left out, as before, and the question is still asked.
fn file_block(path: &str) -> Result<Option<Value>, String> {
    file_block_in(path, &files::inbox_dir())
}

fn file_block_in(path: &str, inbox: &std::path::Path) -> Result<Option<Value>, String> {
    use files::ReadError;

    let read = |limit| files::read_confined(inbox, std::path::Path::new(path), limit);
    let skipped = |why: ReadError| match why {
        ReadError::Outside | ReadError::TooLarge(_) => Err(why.message()),
        // No path in the log: it came from the page.
        _ => {
            log::line(format!("chat file skipped: {why:?}"));
            Ok(None)
        }
    };

    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let media_type = match ext.as_str() {
        "pdf" => Some(("document", "application/pdf")),
        "jpg" | "jpeg" => Some(("image", "image/jpeg")),
        "png" => Some(("image", "image/png")),
        "gif" => Some(("image", "image/gif")),
        "webp" => Some(("image", "image/webp")),
        _ => None,
    };

    if let Some((block_type, media)) = media_type {
        let bytes = match read(files::MAX_ATTACHMENT) {
            Ok(bytes) => bytes,
            Err(why) => return skipped(why),
        };
        return Ok(Some(json!({
            "type": block_type,
            "source": { "type": "base64", "media_type": media, "data": base64(&bytes) },
        })));
    }

    // Too long to inline is skipped quietly (as on macOS), not an error.
    let bytes = match read(MAX_INLINE_TEXT) {
        Ok(bytes) => bytes,
        Err(files::ReadError::TooLarge(_)) => return Ok(None),
        Err(why) => return skipped(why),
    };
    let Ok(text) = String::from_utf8(bytes) else { return Ok(None) };
    Ok(Some(json!({ "type": "text", "text": format!("File contents:\n{text}") })))
}

/// Small standalone base64 encoder — not worth another dependency.
/// Also used for Stripe's basic auth.
pub(crate) fn base64_for(bytes: &[u8]) -> String {
    base64(bytes)
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, file_block_in, files, MAX_INLINE_TEXT};
    use std::path::{Path, PathBuf};

    /// An inbox stand-in and an outside folder, under the temp dir, removed on drop.
    struct Dirs(PathBuf);

    impl Dirs {
        fn new() -> Dirs {
            // One folder per test: they run in parallel.
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!("coucou-claude-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(root.join("inbox")).unwrap();
            std::fs::create_dir_all(root.join("outside")).unwrap();
            Dirs(root)
        }
        fn inbox(&self) -> PathBuf {
            self.0.join("inbox")
        }
        fn block(&self, path: &Path) -> Result<Option<serde_json::Value>, String> {
            file_block_in(path.to_str().unwrap(), &self.inbox())
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── The model's side of an action ────────────────────────────────────────

    use super::{interpret, request_body, tool_result_blocks, Chat, TurnGuard};
    use crate::assistant::ToolResult;

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input })
    }

    #[test]
    fn the_request_offers_the_web_search_and_exactly_one_tool_for_acting() {
        let body = request_body("claude-opus-5", vec![]);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "web_search");
        // Not the code-running variant: the fallback model rejects it (a live 400).
        assert_eq!(tools[0]["type"], "web_search_20250305");
        assert_eq!(body["fallbacks"], "default");
        assert!(tools.iter().all(|t| t.get("allowed_callers").is_none()));
        assert_eq!(tools[1]["name"], crate::actions::TOOL_NAME);
        // The schema forbids anything the parser would refuse.
        assert_eq!(tools[1]["input_schema"]["additionalProperties"], false);
        // The model is told what the tool is for, and not to claim it was done.
        let system = body["system"].as_str().unwrap();
        assert!(system.contains("propose_action") && system.contains("Never say something has been done"));
    }

    #[test]
    fn a_tool_call_in_a_response_becomes_a_proposal_and_text_stays_text() {
        let blocks = vec![
            serde_json::json!({ "type": "text", "text": "Opening it." }),
            tool_use("toolu_1", "propose_action", serde_json::json!({ "type": "open_app", "app": "notepad" })),
        ];
        let found = interpret(&blocks);
        assert_eq!(found.text, "Opening it.");
        let proposal = found.proposal.unwrap();
        assert_eq!(proposal.tool_use_id, "toolu_1");
        assert_eq!(proposal.input, serde_json::json!({ "type": "open_app", "app": "notepad" }));
        assert!(found.unanswered.is_empty());
        // No tool call, no proposal.
        let plain = interpret(&[serde_json::json!({ "type": "text", "text": "Hello." })]);
        assert!(plain.proposal.is_none() && plain.unanswered.is_empty());
    }

    #[test]
    fn extra_and_unknown_tool_calls_are_not_acted_on_but_are_answered() {
        let blocks = vec![
            tool_use("a", "propose_action", serde_json::json!({ "type": "open_app", "app": "notepad" })),
            tool_use("b", "propose_action", serde_json::json!({ "type": "open_app", "app": "calculator" })),
            tool_use("c", "run_shell", serde_json::json!({ "command": "calc" })),
        ];
        let found = interpret(&blocks);
        assert_eq!(found.proposal.unwrap().tool_use_id, "a", "only the first is proposed");
        let ids: Vec<&str> = found.unanswered.iter().map(|r| r.tool_use_id.as_str()).collect();
        assert_eq!(ids, ["b", "c"], "every other call still gets an answer");
        assert!(found.unanswered.iter().all(|r| r.text.contains("not") || r.text.contains("nothing")));
    }

    #[test]
    fn a_tool_call_without_an_id_or_with_junk_input_cannot_become_a_valid_action() {
        // No id: cannot be answered, so it is ignored.
        let found = interpret(&[serde_json::json!({ "type": "tool_use", "name": "propose_action", "input": {} })]);
        assert!(found.proposal.is_none());
        // Junk input is passed on as is; the parser refuses it later.
        let found = interpret(&[serde_json::json!({ "type": "tool_use", "id": "x", "name": "propose_action" })]);
        assert_eq!(found.proposal.unwrap().input, serde_json::Value::Null);
    }

    #[test]
    fn answers_owed_to_the_model_come_first_in_the_next_message() {
        let owed = vec![
            ToolResult { tool_use_id: "toolu_1".into(), text: "The user approved and Coucou did it: Opened Notepad.".into() },
            ToolResult { tool_use_id: "toolu_2".into(), text: "nothing was done".into() },
        ];
        let blocks = tool_result_blocks(&owed);
        assert_eq!(blocks[0], serde_json::json!({ "type": "tool_result", "tool_use_id": "toolu_1", "content": "The user approved and Coucou did it: Opened Notepad." }));
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b["type"] == "tool_result"));
    }

    #[test]
    fn owed_answers_are_taken_once_and_given_back_ahead_of_newer_ones() {
        let chat = Chat::default();
        let r = |id: &str| ToolResult { tool_use_id: id.into(), text: id.into() };
        chat.add_tool_result(r("old"));
        let taken = chat.take_tool_results();
        assert_eq!(taken.len(), 1);
        assert!(chat.take_tool_results().is_empty(), "taken, not copied");
        chat.add_tool_result(r("new"));
        chat.restore_tool_results(taken);
        let ids: Vec<String> = chat.take_tool_results().into_iter().map(|r| r.tool_use_id).collect();
        assert_eq!(ids, ["old", "new"]);
        chat.add_tool_result(r("x"));
        chat.reset();
        assert!(chat.take_tool_results().is_empty(), "a new conversation owes nothing");
    }

    #[test]
    fn a_turn_that_is_dropped_leaves_the_conversation_as_it_was() {
        // The person cancels mid-request: the future is simply dropped.
        let chat = Chat::default();
        chat.add_tool_result(ToolResult { tool_use_id: "toolu_1".into(), text: "done".into() });
        let owed = chat.take_tool_results();
        {
            let mut guard = TurnGuard { chat: &chat, owed: owed.clone(), pushed: false, done: false };
            chat.push(serde_json::json!({ "role": "user", "content": "open notepad" }));
            guard.pushed = true;
            // dropped here, before the model answered
        }
        assert!(chat.is_empty(), "the unanswered message is gone");
        assert_eq!(chat.take_tool_results(), owed, "and the answer it carried is owed again");
    }

    #[test]
    fn a_completed_turn_keeps_its_messages_and_does_not_give_answers_back() {
        let chat = Chat::default();
        let owed = vec![ToolResult { tool_use_id: "t".into(), text: "done".into() }];
        {
            let mut guard = TurnGuard { chat: &chat, owed: owed.clone(), pushed: false, done: false };
            chat.push(serde_json::json!({ "role": "user", "content": [] }));
            chat.push(serde_json::json!({ "role": "assistant", "content": [] }));
            guard.pushed = true;
            guard.done = true;
        }
        assert_eq!(chat.snapshot().len(), 2);
        assert!(chat.take_tool_results().is_empty());
    }

    // ── The API call itself, against a local server (no live service) ────────

    use crate::http::tests::{reply, run, Mock, Script};
    use crate::http::{ApiError, Retry};
    use serde_json::json;
    use std::time::Duration;

    const QUICK: Retry = Retry {
        max_retries: 2,
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        deadline: Duration::from_secs(10),
        attempt_timeout: Duration::from_secs(2),
    };

    fn client(ms: u64) -> reqwest::Client {
        crate::http::build_client(Duration::from_millis(ms)).unwrap()
    }

    const OK: &str = r#"{"content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn"}"#;

    #[test]
    fn a_message_request_carries_the_documented_headers_and_returns_the_reply() {
        run(async {
            let mock = Mock::start(vec![reply(200, OK)]).await;
            let body = json!({ "model": "claude-opus-5", "max_tokens": 8, "messages": [] });
            let value = super::call_at(&client(2000), &mock.url("/v1/messages"), "sk-test-key", &body, &QUICK)
                .await
                .unwrap();
            assert_eq!(value["content"][0]["text"], "hello");
            let seen = mock.seen_headers(0);
            assert_eq!(seen.get("x-api-key").map(String::as_str), Some("sk-test-key"));
            assert_eq!(seen.get("anthropic-version").map(String::as_str), Some("2023-06-01"));
            assert!(seen.contains_key("anthropic-beta"));
            assert_eq!(seen.get("content-type").map(String::as_str), Some("application/json"));
        });
    }

    #[test]
    fn every_failure_kind_reads_differently_and_leaks_nothing() {
        let status = |code, message: Option<&str>, retry_after| ApiError::Status {
            code,
            message: message.map(String::from),
            retry_after,
        };
        let cases = [
            (status(401, Some("invalid x-api-key sk-test-key"), None), "API key rejected (401). Check it in Settings."),
            (status(403, None, None), "Claude refused this request (403). The key may lack access to this model."),
            (status(429, None, Some(Duration::from_secs(12))), "Claude is rate limiting requests (429). Try again in 12 s."),
            (status(429, None, None), "Claude is rate limiting requests (429). Try again in a minute."),
            (status(529, None, None), "Claude is overloaded or unavailable (529). Try again in a moment."),
            (status(400, Some("max_tokens: must be positive"), None), "Claude API 400: max_tokens: must be positive"),
            (status(404, Some("model: claude-nope"), None), "Claude API 404: model: claude-nope"),
            (status(413, None, None), "Claude API 413"),
            (ApiError::Timeout, "Claude took too long to answer. Try again."),
            (ApiError::Connect, "No connection. Check your network and try again."),
            (ApiError::Transport, "The connection to Claude broke. Try again."),
            (ApiError::TooLarge, "Claude's reply was too large to read."),
            (ApiError::Malformed, "Bad API response."),
        ];
        for (err, expected) in cases {
            let shown = super::chat_error(&err);
            assert_eq!(shown, expected);
            assert!(!shown.contains("sk-test-key"), "{shown}");
        }
    }

    #[test]
    fn auth_and_invalid_requests_are_surfaced_once_without_retries() {
        run(async {
            let body = json!({});
            let unauthorised = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
            let mock = Mock::start(vec![reply(401, unauthorised)]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(super::chat_error(&err), "API key rejected (401). Check it in Settings.");
            assert_eq!(mock.hits(), 1);

            let invalid = r#"{"type":"error","error":{"type":"invalid_request_error","message":"model: claude-nope"}}"#;
            let mock = Mock::start(vec![reply(400, invalid)]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(super::chat_error(&err), "Claude API 400: model: claude-nope");
            assert_eq!(mock.hits(), 1);
        });
    }

    #[test]
    fn a_rejected_request_is_logged_with_the_apis_reason_and_never_the_key() {
        run(async {
            let body = json!({ "model": "claude-opus-5" });
            let rejected = r#"{"type":"error","error":{"type":"invalid_request_error","message":"tool web_search is not supported with sk-live-secret-key"}}"#;
            let mock = Mock::start(vec![reply(400, rejected)]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "sk-live-secret-key", &body, &QUICK).await.unwrap_err();
            let line = super::failure_line("claude-opus-5", &super::chat_error(&err));
            assert!(line.starts_with("chat request failed (model claude-opus-5): Claude API 400: tool web_search is not supported"), "{line}");
            assert!(!line.contains("sk-live-secret-key"), "{line}");
            assert!(!line.contains("127.0.0.1") && !line.contains("http"), "{line}");
        });
    }

    #[test]
    fn overload_and_rate_limits_are_retried_a_bounded_number_of_times() {
        run(async {
            let body = json!({});
            // Overloaded, then fine: the user never sees the blip.
            let mock = Mock::start(vec![reply(529, "{}"), reply(200, OK)]).await;
            super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap();
            assert_eq!(mock.hits(), 2);

            // Overloaded for good: 1 try + 2 retries, then a clear message.
            let mock = Mock::start(vec![reply(529, "{}")]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(mock.hits(), 3);
            assert_eq!(super::chat_error(&err), "Claude is overloaded or unavailable (529). Try again in a moment.");

            // Rate limited with a long Retry-After: reported with the wait, not slept through.
            let limited = Script::Reply { status: 429, headers: vec![("retry-after", "120".into())], body: b"{}".to_vec() };
            let mock = Mock::start(vec![limited]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(mock.hits(), 1);
            assert_eq!(super::chat_error(&err), "Claude is rate limiting requests (429). Try again in 120 s.");
        });
    }

    #[test]
    fn timeouts_connection_failures_and_bad_bodies_do_not_hang_or_crash() {
        run(async {
            let body = json!({});
            // Never answers: a timeout, and not asked twice.
            let mock = Mock::start(vec![Script::Hang]).await;
            let err = super::call_at(&client(150), &mock.url("/"), "k", &body, &Retry { attempt_timeout: Duration::from_millis(150), ..QUICK }).await.unwrap_err();
            assert_eq!(err, ApiError::Timeout);
            assert_eq!(mock.hits(), 1);

            // Cannot reach the service at all (`.invalid` never resolves).
            let err = super::call_at(&client(5000), "http://coucou-test.invalid/", "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(super::chat_error(&err), "No connection. Check your network and try again.");

            // A 200 that is not JSON.
            let mock = Mock::start(vec![reply(200, "<html>captive portal</html>")]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "k", &body, &QUICK).await.unwrap_err();
            assert_eq!(super::chat_error(&err), "Bad API response.");
        });
    }

    #[test]
    fn the_next_message_works_after_a_failed_one() {
        run(async {
            let body = json!({});
            let mock = Mock::start(vec![reply(401, "{}"), Script::Hangup, reply(200, OK)]).await;
            let c = client(2000);
            assert!(super::call_at(&c, &mock.url("/"), "k", &body, &QUICK).await.is_err());
            assert!(super::call_at(&c, &mock.url("/"), "k", &body, &QUICK).await.is_err());
            assert!(super::call_at(&c, &mock.url("/"), "k", &body, &QUICK).await.is_ok());
        });
    }

    #[test]
    fn a_key_with_a_stray_newline_is_a_bad_key_not_a_network_error() {
        run(async {
            let mock = Mock::start(vec![reply(200, OK)]).await;
            let err = super::call_at(&client(2000), &mock.url("/"), "sk-abc\n", &json!({}), &QUICK).await.unwrap_err();
            assert_eq!(super::chat_error(&err), "API key rejected (401). Check it in Settings.");
            assert_eq!(mock.hits(), 0, "nothing is sent with a key that cannot be carried");
        });
    }

    #[test]
    fn the_chat_attaches_text_pdf_and_images_from_the_inbox() {
        let d = Dirs::new();
        std::fs::write(d.inbox().join("a.txt"), "hello").unwrap();
        std::fs::write(d.inbox().join("a.pdf"), b"%PDF-1.4").unwrap();
        std::fs::write(d.inbox().join("a.PNG"), [0x89, b'P', b'N', b'G']).unwrap();
        // No extension: text, as before (inside the inbox that is harmless).
        std::fs::write(d.inbox().join("README"), "plain").unwrap();

        let text = d.block(&d.inbox().join("a.txt")).unwrap().unwrap();
        assert_eq!(text["text"], "File contents:\nhello");
        let pdf = d.block(&d.inbox().join("a.pdf")).unwrap().unwrap();
        assert_eq!((pdf["type"].as_str(), pdf["source"]["media_type"].as_str()), (Some("document"), Some("application/pdf")));
        assert_eq!(pdf["source"]["data"], base64(b"%PDF-1.4"));
        let png = d.block(&d.inbox().join("a.PNG")).unwrap().unwrap();
        assert_eq!(png["source"]["media_type"], "image/png");
        assert_eq!(d.block(&d.inbox().join("README")).unwrap().unwrap()["text"], "File contents:\nplain");
    }

    #[test]
    fn a_path_outside_the_inbox_is_refused_whatever_it_looks_like() {
        let d = Dirs::new();
        let secret = d.0.join("outside").join("id_rsa");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        // An extensionless "text" file elsewhere is exactly what used to be read.
        let err = d.block(&secret).unwrap_err();
        assert!(err.contains("inbox"), "{err}");
        assert!(!err.contains("id_rsa"), "the message must not echo the path");
        for sneaky in [
            d.inbox().join("..").join("outside").join("id_rsa"),
            PathBuf::from("id_rsa"),
            d.0.join("outside").join("id_rsa.png"),
            d.0.join("outside").join("id_rsa.pdf"),
        ] {
            assert!(d.block(&sneaky).is_err(), "{sneaky:?}");
        }
    }

    #[test]
    fn oversized_and_unusable_files_behave_as_documented() {
        let d = Dirs::new();
        // Text over the inline limit is left out quietly, as on macOS.
        std::fs::write(d.inbox().join("big.txt"), vec![b'a'; MAX_INLINE_TEXT as usize + 1]).unwrap();
        assert_eq!(d.block(&d.inbox().join("big.txt")).unwrap(), None);
        // Not UTF-8 and not a known type: left out, not an error.
        std::fs::write(d.inbox().join("blob.bin"), [0xFF, 0xFE, 0x00]).unwrap();
        assert_eq!(d.block(&d.inbox().join("blob.bin")).unwrap(), None);
        // A file that vanished is left out too.
        assert_eq!(d.block(&d.inbox().join("gone.txt")).unwrap(), None);
        // A PDF or image beyond the limit is an error the user can read.
        let huge = d.inbox().join("huge.pdf");
        std::fs::File::create(&huge).unwrap().set_len(files::MAX_ATTACHMENT + 1).unwrap();
        let err = d.block(&huge).unwrap_err();
        assert!(err.contains("too large") && err.contains("20 MB"), "{err}");
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
