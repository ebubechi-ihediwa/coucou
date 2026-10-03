// Claude API client — the same integration as ClaudeService.swift: multi-turn
// chat with web search, and files sent as document/image/text blocks.
//
// Everything happens here rather than in the island: the API key never leaves
// the Credential Manager, and file bytes never cross the IPC boundary.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
No markdown formatting (no **, no ##, no bullet dashes). Use plain text with line breaks.";

#[derive(Default)]
pub struct Chat {
    /// Full multi-turn history, including tool_use / tool_result blocks.
    messages: Mutex<Vec<Value>>,
}

impl Chat {
    pub fn reset(&self) {
        self.messages.lock().unwrap().clear();
    }

    fn is_empty(&self) -> bool {
        self.messages.lock().unwrap().is_empty()
    }

    fn push(&self, message: Value) {
        self.messages.lock().unwrap().push(message);
    }

    fn pop(&self) {
        self.messages.lock().unwrap().pop();
    }

    fn snapshot(&self) -> Vec<Value> {
        self.messages.lock().unwrap().clone()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ChatContext {
    File { name: String, path: String },
    Window { app_name: String, title: String, url: Option<String> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatReply {
    pub text: String,
}

/// One chat turn. Returns the assistant's text, or a message the island shows
/// in the note view.
pub async fn send(
    chat: &Chat,
    model: &str,
    query: String,
    context: Option<ChatContext>,
) -> Result<ChatReply, String> {
    let key = secrets::get("anthropic-api-key")
        .ok_or_else(|| "API key missing. Open settings.".to_string())?;

    let mut content: Vec<Value> = Vec::new();

    // File / window context rides along with the first message only, exactly
    // like ClaudeService.chat().
    if chat.is_empty() {
        match &context {
            Some(ChatContext::File { name, path }) => {
                // An Err returns before anything is pushed: the history stays as it was.
                if let Some(block) = file_block(path)? {
                    content.push(block);
                }
                content.push(json!({ "type": "text", "text": format!("File: {name}") }));
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

    let body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "system": SYSTEM_PROMPT,
        "tools": [{ "type": "web_search_20260209", "name": "web_search", "max_uses": 5 }],
        "fallbacks": "default",
        "messages": chat.snapshot(),
    });

    let response = match call(&key, &body).await {
        Ok(v) => v,
        Err(err) => {
            chat.pop(); // keep the history consistent with what the model saw
            return Err(err);
        }
    };

    // A policy decline comes back as HTTP 200 with stop_reason "refusal".
    if response.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        chat.pop();
        let why = response
            .get("stop_details")
            .and_then(|d| d.get("explanation"))
            .and_then(Value::as_str)
            .unwrap_or("Claude declined this one.");
        return Err(why.to_string());
    }

    let Some(blocks) = response.get("content").and_then(Value::as_array).cloned() else {
        chat.pop();
        return Err("Unexpected API response.".into());
    };

    // Store the whole content — tool_use / tool_result blocks included — so the
    // next turn has the right context.
    chat.push(json!({ "role": "assistant", "content": blocks.clone() }));

    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();

    if text.is_empty() {
        return Err("No response text.".into());
    }
    Ok(ChatReply { text })
}

async fn call(key: &str, body: &Value) -> Result<Value, String> {
    let client = http::chat_client().map_err(|e| chat_error(&e))?;
    call_at(&client, ENDPOINT, key, body, &http::CHAT_RETRY)
        .await
        .map_err(|e| chat_error(&e))
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
