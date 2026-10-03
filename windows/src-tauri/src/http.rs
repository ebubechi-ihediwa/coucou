// The one place outbound API requests are made and judged: the chat and every
// poller go through `send`, so they share a single answer to "what happened?".
//
//   * Every failure becomes an `ApiError` that says which kind it was (bad key,
//     rate limit, server, timeout, no connection, bad response), with a message
//     that never contains a credential, a URL or a response body.
//   * A body is read in chunks and refused past a size limit, so a hostile or
//     broken server cannot make us buffer without end. The client's timeout
//     covers the body too, so a slow drip is bounded as well.
//   * Redirects are followed only on the same host. reqwest strips `Authorization`
//     when a redirect changes host, but not a custom header such as
//     `x-api-key` or `X-N8N-API-KEY`; refusing the redirect is what keeps those
//     from being sent on to somebody else.
//   * Retrying is opt-in (`send_retrying`), bounded, backed off with jitter,
//     honours `Retry-After`, and only repeats what is safe to repeat.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{redirect, Client, RequestBuilder, Url};
use serde_json::Value;

/// The pollers: one small GET (or POST) every so often.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(10);
pub const POLL_MAX_BODY: usize = 8 * 1024 * 1024;
/// The chat: the model may think and search for a while.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(90);
pub const CHAT_MAX_BODY: usize = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Only the message inside an error body is wanted, and not much of it.
const ERROR_BODY_MAX: usize = 16 * 1024;
const MESSAGE_MAX_CHARS: usize = 120;
/// A `Retry-After` longer than this is reported, not waited for.
const RETRY_AFTER_LIMIT: Duration = Duration::from_secs(30);

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The service answered with an error status.
    Status {
        code: u16,
        /// The service's own short explanation, if it gave one. Never kept for 401.
        message: Option<String>,
        retry_after: Option<Duration>,
    },
    /// No answer within the time allowed (connecting, waiting or reading).
    Timeout,
    /// Could not reach the service at all: DNS, refused, unreachable.
    Connect,
    /// The connection broke or the exchange was not valid HTTP.
    Transport,
    /// The response was bigger than we are willing to hold.
    TooLarge,
    /// A success status, but the body is not what was promised.
    Malformed,
}

impl ApiError {
    pub fn code(&self) -> Option<u16> {
        match self {
            ApiError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Worth asking again: the service said "later" (429), is struggling
    /// (500/502/503/504, 529 overloaded), or we never reached it. A timeout is not
    /// here: the request may well have been processed, and waiting again would
    /// double the wait. Bad keys and bad requests are not here either.
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Status { code, .. } => matches!(code, 429 | 500 | 502 | 503 | 504 | 529),
            ApiError::Connect => true,
            _ => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// What the island may show. `forbidden` is the integration's own advice for
    /// a 403, because what a 403 means differs per service.
    pub fn describe(&self, forbidden: &str) -> String {
        match self {
            ApiError::Status { code: 401, .. } => "Invalid API key (401)".into(),
            ApiError::Status {
                code: 403, message, ..
            } => with_detail(forbidden, message),
            ApiError::Status {
                code: 429,
                retry_after,
                ..
            } => match retry_after {
                Some(wait) => format!("Rate limited (429) — try again in {}", wait_text(*wait)),
                None => "Rate limited (429)".into(),
            },
            ApiError::Status { code, .. } if *code >= 500 => {
                format!("Service unavailable ({code})")
            }
            ApiError::Status { code, .. } if (300..400).contains(code) => {
                format!("Unexpected redirect ({code})")
            }
            ApiError::Status { code, message, .. } => {
                with_detail(&format!("Request rejected ({code})"), message)
            }
            ApiError::Timeout => "Timed out".into(),
            ApiError::Connect => "No connection".into(),
            ApiError::Transport => "Network error".into(),
            ApiError::TooLarge => "Response too large".into(),
            ApiError::Malformed => "Unexpected response".into(),
        }
    }
}

fn with_detail(head: &str, message: &Option<String>) -> String {
    match message {
        Some(m) => format!("{head} — {m}"),
        None => head.to_string(),
    }
}

fn wait_text(wait: Duration) -> String {
    let secs = wait.as_secs().max(1);
    if secs < 120 {
        format!("{secs} s")
    } else {
        format!("{} min", secs.div_ceil(60))
    }
}

fn classify(err: reqwest::Error) -> ApiError {
    if err.is_builder() {
        // The request itself could not be made: a URL that is not one.
        ApiError::Status {
            code: 400,
            message: Some("invalid URL".into()),
            retry_after: None,
        }
    } else if err.is_connect() {
        // Checked before the timeout: a connection that timed out while being made
        // never reached the service (Windows takes ~2 s to give up on a refused
        // port), which is "no connection" and safe to try again.
        ApiError::Connect
    } else if err.is_timeout() {
        ApiError::Timeout
    } else {
        ApiError::Transport
    }
}

/// The service's own words for what went wrong, trimmed to something that fits a
/// pill: `{"error":{"message":…}}`, `{"message":…}` or `{"error":"…"}`.
///
/// `secrets` are the credentials this request carried. A service may quote one back
/// in an error ("invalid token abc123…"), and the message goes to the island and to
/// the log file on disk, so every occurrence is replaced first. It is done before the
/// length cut: cutting first could leave the front half of a key behind.
fn error_message(body: &[u8], secrets: &[String]) -> Option<String> {
    let json: Value = serde_json::from_slice(body).ok()?;
    let text = json
        .pointer("/error/message")
        .or_else(|| json.get("message"))
        .or_else(|| json.get("error"))
        .and_then(Value::as_str)?;
    let mut cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for secret in secrets {
        cleaned = cleaned.replace(secret.as_str(), "[redacted]");
    }
    if cleaned.is_empty() {
        return None;
    }
    let mut short: String = cleaned.chars().take(MESSAGE_MAX_CHARS).collect();
    if cleaned.chars().count() > MESSAGE_MAX_CHARS {
        short.push('…');
    }
    Some(short)
}

/// The credentials a request carries, in every form a service might quote back: each
/// header marked sensitive (by `secret`/`bearer`), the token after `Bearer`/`Basic`,
/// the user and key inside a decoded `Basic` value, and a user or password written
/// into the URL. Anything under four characters is not treated as one, so a short
/// word cannot blank out ordinary text.
fn secrets_of(request: &reqwest::Request) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (_, value) in request.headers() {
        if !value.is_sensitive() {
            continue;
        }
        let Ok(text) = value.to_str() else { continue };
        found.push(text.to_string());
        if let Some(token) = text.strip_prefix("Bearer ") {
            found.push(token.to_string());
        }
        if let Some(encoded) = text.strip_prefix("Basic ") {
            found.push(encoded.to_string());
            if let Some(decoded) = base64_decode(encoded) {
                found.extend(decoded.split(':').map(str::to_string));
                found.push(decoded);
            }
        }
    }
    let url = request.url();
    found.push(url.username().to_string());
    if let Some(password) = url.password() {
        found.push(password.to_string());
    }
    found.retain(|s| s.chars().count() >= 4);
    found.sort_by_key(|s| std::cmp::Reverse(s.len())); // longest first: no half-redacted overlaps
    found.dedup();
    found
}

/// Standard base64 (padding optional) to text, or `None` if it is not base64 or not
/// UTF-8. Only used to recover the key inside a `Basic` credential, so that it can be
/// kept out of messages (see `secrets_of`).
fn base64_decode(input: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in input.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// `Retry-After` in whole seconds. (The HTTP-date form is not read: none of the
/// services used here send it, and ignoring it only means the normal backoff.)
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

// ── Clients ───────────────────────────────────────────────────────────────────

/// Same host only (see the top of the file). A plain `http://` → `https://`
/// upgrade on the same host is the one change of port that is also allowed.
fn redirect_policy() -> redirect::Policy {
    redirect::Policy::custom(|attempt| {
        let allowed = attempt.previous().len() < 5
            && attempt
                .previous()
                .last()
                .is_some_and(|from| redirect_allowed(from, attempt.url()));
        if allowed {
            attempt.follow()
        } else {
            attempt.stop()
        }
    })
}

fn redirect_allowed(from: &Url, to: &Url) -> bool {
    // Never down from https to http, whatever the port: that would put the key on
    // the wire in clear text.
    if from.scheme() == "https" && to.scheme() != "https" {
        return false;
    }
    from.host_str() == to.host_str()
        && (from.port_or_known_default() == to.port_or_known_default()
            || (from.scheme() == "http" && to.scheme() == "https" && to.port().is_none()))
}

pub fn build_client(timeout: Duration) -> Result<Client, ApiError> {
    Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT.min(timeout))
        .redirect(redirect_policy())
        .build()
        .map_err(|_| ApiError::Transport)
}

/// One client for all pollers: connections are reused, and the timeout can never
/// silently disappear (the old `unwrap_or_default()` fallback had none).
pub fn poll_client() -> Result<Client, ApiError> {
    static CLIENT: LazyLock<Result<Client, ApiError>> =
        LazyLock::new(|| build_client(POLL_TIMEOUT));
    CLIENT.clone()
}

pub fn chat_client() -> Result<Client, ApiError> {
    static CLIENT: LazyLock<Result<Client, ApiError>> =
        LazyLock::new(|| build_client(CHAT_TIMEOUT));
    CLIENT.clone()
}

/// A header value that is a credential: marked sensitive so it never shows up in
/// a debug print of the request. A key with characters HTTP cannot carry (a stray
/// newline from a paste) is a bad key, said as such rather than as a network fault.
pub fn secret(value: &str) -> Result<HeaderValue, ApiError> {
    let mut header = HeaderValue::from_str(value).map_err(|_| ApiError::Status {
        code: 401,
        message: None,
        retry_after: None,
    })?;
    header.set_sensitive(true);
    Ok(header)
}

pub fn bearer(token: &str) -> Result<HeaderValue, ApiError> {
    secret(&format!("Bearer {token}"))
}

// ── Sending ───────────────────────────────────────────────────────────────────

/// Sends one request and returns the body of a successful answer.
pub async fn send(request: RequestBuilder, max_body: usize) -> Result<Vec<u8>, ApiError> {
    // Built here rather than by `send()` so the credentials it carries are known
    // before the answer arrives (see `error_message`).
    let (client, request) = request.build_split();
    let request = request.map_err(classify)?;
    let secrets = secrets_of(&request);
    let mut response = client.execute(request).await.map_err(classify)?;
    let status = response.status();
    if !status.is_success() {
        let retry_after = retry_after(response.headers());
        // The body is only read for its message; failing to read it changes nothing.
        let body = read_capped(&mut response, ERROR_BODY_MAX)
            .await
            .unwrap_or_default();
        let message = if status.as_u16() == 401 {
            None
        } else {
            error_message(&body, &secrets)
        };
        return Err(ApiError::Status {
            code: status.as_u16(),
            message,
            retry_after,
        });
    }
    if response
        .content_length()
        .is_some_and(|n| n > max_body as u64)
    {
        return Err(ApiError::TooLarge);
    }
    read_capped(&mut response, max_body).await
}

/// `send`, for a JSON answer. A body that is not JSON is `Malformed`, never an
/// empty success.
pub async fn send_json(request: RequestBuilder, max_body: usize) -> Result<Value, ApiError> {
    let body = send(request, max_body).await?;
    serde_json::from_slice(&body).map_err(|_| ApiError::Malformed)
}

async fn read_capped(response: &mut reqwest::Response, max: usize) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(classify)? {
        if body.len() + chunk.len() > max {
            return Err(ApiError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

// ── Retrying ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Extra attempts after the first.
    pub max_retries: u32,
    /// The first wait, doubled each time up to `cap`.
    pub base: Duration,
    pub cap: Duration,
    /// Give up rather than start a wait that would end past this, counted from
    /// the first attempt. It also caps each attempt: one gets the smaller of
    /// `attempt_timeout` and the time left, so the whole exchange, slow answers
    /// included, ends within the deadline.
    pub deadline: Duration,
    /// The longest one attempt may take (headers and body). It replaces the
    /// client's own timeout for these requests, so it should match it.
    pub attempt_timeout: Duration,
}

/// For the chat: a message request changes nothing on the server, so asking again
/// is safe, and two more tries ride out a rate limit or a brief overload without
/// making a person wait minutes.
pub const CHAT_RETRY: Retry = Retry {
    max_retries: 2,
    base: Duration::from_secs(1),
    cap: Duration::from_secs(8),
    deadline: Duration::from_secs(100),
    attempt_timeout: CHAT_TIMEOUT,
};

/// How long to wait before retry number `attempt + 1`, or `None` to stop. Half the
/// wait is fixed and half is random (`unit_random`, 0..1), so clients that failed
/// together do not come back together; a `Retry-After` is a floor.
pub fn backoff_delay(
    retry: &Retry,
    attempt: u32,
    retry_after: Option<Duration>,
    unit_random: f64,
) -> Option<Duration> {
    if retry_after.is_some_and(|wait| wait > RETRY_AFTER_LIMIT) {
        return None;
    }
    let exponential = retry
        .base
        .saturating_mul(1u32 << attempt.min(16))
        .min(retry.cap);
    let jittered = exponential / 2 + exponential.mul_f64(unit_random.clamp(0.0, 1.0) / 2.0);
    Some(retry_after.map_or(jittered, |wait| wait.max(jittered)))
}

/// A random number in 0..1 without a dependency: `RandomState` is seeded from the OS.
fn unit_random() -> f64 {
    (RandomState::new().build_hasher().finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// `send`, asking again when the failure is transient (see `is_transient`) and the
/// budget allows. `make` builds a fresh request each time. Dropping the future
/// ends the attempt in flight and any wait between attempts.
pub async fn send_retrying(
    make: impl Fn() -> RequestBuilder,
    max_body: usize,
    retry: &Retry,
) -> Result<Vec<u8>, ApiError> {
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        // No attempt may run past the overall deadline, however slow the answer.
        let timeout = retry
            .attempt_timeout
            .min(retry.deadline.saturating_sub(started.elapsed()));
        if timeout.is_zero() {
            return Err(ApiError::Timeout);
        }
        let err = match send(make().timeout(timeout), max_body).await {
            Ok(body) => return Ok(body),
            Err(err) => err,
        };
        if !err.is_transient() || attempt >= retry.max_retries {
            return Err(err);
        }
        let Some(delay) = backoff_delay(retry, attempt, err.retry_after(), unit_random()) else {
            return Err(err);
        };
        if started.elapsed() + delay > retry.deadline {
            return Err(err);
        }
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;

    pub(crate) fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// What the mock server does with one connection.
    #[derive(Clone)]
    pub(crate) enum Script {
        /// A normal HTTP answer.
        Reply {
            status: u16,
            headers: Vec<(&'static str, String)>,
            body: Vec<u8>,
        },
        /// Reads the request and never answers.
        Hang,
        /// Reads the request, then hangs up without a word.
        Hangup,
        /// Sends a head that promises `claimed` bytes, delivers `body`, and closes.
        Truncated { claimed: usize, body: Vec<u8> },
        /// Sends a 200 with no length and keeps writing `total` bytes.
        Endless { total: usize },
    }

    pub(crate) fn reply(status: u16, body: &str) -> Script {
        Script::Reply {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }

    struct Seen {
        path: String,
        headers: Vec<(String, String)>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    pub(crate) struct Mock {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
        /// Set when a client closed a connection the server was holding open.
        client_gone: Arc<Notify>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Mock {
        /// Serves `scripts` in order, one per connection; the last repeats.
        pub(crate) async fn start(scripts: Vec<Script>) -> Mock {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let client_gone = Arc::new(Notify::new());
            let (seen2, gone2) = (seen.clone(), client_gone.clone());
            let counter = Arc::new(AtomicUsize::new(0));
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let script = scripts[n.min(scripts.len() - 1)].clone();
                    let (seen, gone) = (seen2.clone(), gone2.clone());
                    tokio::spawn(async move { serve(stream, script, seen, gone).await });
                }
            });
            Mock {
                addr,
                seen,
                client_gone,
                _task: task,
            }
        }

        pub(crate) fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        pub(crate) fn hits(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        /// The headers (lower-cased names) of request number `n`.
        pub(crate) fn seen_headers(&self, n: usize) -> std::collections::HashMap<String, String> {
            self.seen.lock().unwrap()[n]
                .headers
                .iter()
                .cloned()
                .collect()
        }
    }

    async fn serve(
        mut stream: TcpStream,
        script: Script,
        seen: Arc<Mutex<Vec<Seen>>>,
        gone: Arc<Notify>,
    ) {
        // Read the request head.
        let mut head = Vec::new();
        let mut chunk = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
            }
        }
        let text = String::from_utf8_lossy(&head).to_string();
        let mut lines = text.split("\r\n");
        let path = lines
            .next()
            .unwrap_or("")
            .split(' ')
            .nth(1)
            .unwrap_or("")
            .to_string();
        let headers = lines
            .take_while(|l| !l.is_empty())
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        seen.lock().unwrap().push(Seen { path, headers });

        match script {
            Script::Reply {
                status,
                headers,
                body,
            } => {
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = stream.write_all(out.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
            Script::Hang => {
                // Wait until the client hangs up (or the test ends).
                let mut buf = [0u8; 64];
                while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {}
                gone.notify_one();
            }
            Script::Hangup => {}
            Script::Truncated { claimed, body } => {
                let head = format!(
                    "HTTP/1.1 200 X\r\ncontent-length: {claimed}\r\nconnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
            Script::Endless { total } => {
                let _ = stream
                    .write_all(b"HTTP/1.1 200 X\r\nconnection: close\r\n\r\n")
                    .await;
                let block = vec![b'x'; 16 * 1024];
                let mut sent = 0;
                while sent < total && stream.write_all(&block).await.is_ok() {
                    sent += block.len();
                }
            }
        }
    }

    fn client(timeout_ms: u64) -> Client {
        build_client(Duration::from_millis(timeout_ms)).unwrap()
    }

    const MAX: usize = 64 * 1024;
    const QUICK: Retry = Retry {
        max_retries: 2,
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        deadline: Duration::from_secs(10),
        attempt_timeout: Duration::from_secs(2),
    };

    // ── The matrix, against a local server ───────────────────────────────────

    #[test]
    fn a_successful_request_returns_the_body() {
        run(async {
            let mock = Mock::start(vec![reply(200, r#"{"ok":true}"#)]).await;
            let value = send_json(client(2000).get(mock.url("/v1")), MAX)
                .await
                .unwrap();
            assert_eq!(value["ok"], true);
            assert_eq!(mock.seen.lock().unwrap()[0].path, "/v1");
        });
    }

    #[test]
    fn an_invalid_key_is_an_auth_error_and_the_message_never_leaks_it() {
        run(async {
            let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key sk-secret-1234"}}"#;
            let mock = Mock::start(vec![reply(401, body)]).await;
            let err = send(
                client(2000)
                    .get(mock.url("/"))
                    .header("x-api-key", secret("sk-secret-1234").unwrap()),
                MAX,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code(), Some(401));
            assert!(!err.is_transient(), "a bad key is never retried");
            let shown = err.describe("whatever");
            assert_eq!(shown, "Invalid API key (401)");
            assert!(!shown.contains("sk-secret") && !format!("{err:?}").contains("sk-secret"));
        });
    }

    #[test]
    fn what_each_status_means_matches_what_the_real_services_send() {
        // Statuses seen from the live services with an invalid key on 2026-10-02:
        // Vercel and Cal.com answer 403, Resend answers 400, the rest 401.
        let vercel = ApiError::Status {
            code: 403,
            message: error_message(
                br#"{"error":{"code":"forbidden","message":"Not authorized","invalidToken":true}}"#,
                &[],
            ),
            retry_after: None,
        };
        assert_eq!(
            vercel.describe("Token lacks access"),
            "Token lacks access — Not authorized"
        );
        let resend = ApiError::Status {
            code: 400,
            message: error_message(
                br#"{"statusCode":400,"message":"API key is invalid","name":"validation_error"}"#,
                &[],
            ),
            retry_after: None,
        };
        assert_eq!(
            resend.describe(""),
            "Request rejected (400) — API key is invalid"
        );
        assert_eq!(
            ApiError::Status {
                code: 503,
                message: None,
                retry_after: None
            }
            .describe(""),
            "Service unavailable (503)"
        );
        assert_eq!(
            ApiError::Status {
                code: 429,
                message: None,
                retry_after: Some(Duration::from_secs(45))
            }
            .describe(""),
            "Rate limited (429) — try again in 45 s"
        );
        assert_eq!(
            ApiError::Status {
                code: 302,
                message: None,
                retry_after: None
            }
            .describe(""),
            "Unexpected redirect (302)"
        );
    }

    #[test]
    fn an_invalid_request_surfaces_the_services_explanation() {
        run(async {
            let mock = Mock::start(vec![reply(
                400,
                r#"{"error":{"message":"max_tokens: must be positive"}}"#,
            )])
            .await;
            let err = send(client(2000).get(mock.url("/")), MAX)
                .await
                .unwrap_err();
            assert_eq!(
                err.describe(""),
                "Request rejected (400) — max_tokens: must be positive"
            );
            assert!(
                !err.is_transient(),
                "retrying the same bad request cannot help"
            );
            assert_eq!(mock.hits(), 1);
        });
    }

    #[test]
    fn error_messages_are_cleaned_and_shortened() {
        let long = format!(r#"{{"message":"{}"}}"#, "a".repeat(500));
        let msg = error_message(long.as_bytes(), &[]).unwrap();
        assert_eq!(msg.chars().count(), MESSAGE_MAX_CHARS + 1);
        assert!(msg.ends_with('…'));
        assert_eq!(
            error_message(br#"{"error":"line one\nline\ttwo"}"#, &[]).unwrap(),
            "line one line two"
        );
        assert_eq!(error_message(b"<html>502 Bad Gateway</html>", &[]), None);
        assert_eq!(error_message(br#"{"message":"   "}"#, &[]), None);
        assert_eq!(error_message(br#"{"message":42}"#, &[]), None);
    }

    #[test]
    fn rate_limiting_is_reported_with_the_wait_and_retried_within_limits() {
        run(async {
            // 429 with Retry-After, then success: one retry, after at least the wait.
            let limited = Script::Reply {
                status: 429,
                headers: vec![("retry-after", "1".into())],
                body: b"{}".to_vec(),
            };
            let mock = Mock::start(vec![limited, reply(200, "{}")]).await;
            let started = Instant::now();
            let c = client(5000);
            send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                .await
                .unwrap();
            assert_eq!(mock.hits(), 2);
            assert!(
                started.elapsed() >= Duration::from_secs(1),
                "Retry-After is a floor: {:?}",
                started.elapsed()
            );

            // A wait longer than we are willing to sit through is reported, not waited for.
            let long = Script::Reply {
                status: 429,
                headers: vec![("retry-after", "3600".into())],
                body: b"{}".to_vec(),
            };
            let mock = Mock::start(vec![long]).await;
            let started = Instant::now();
            let err = send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                .await
                .unwrap_err();
            assert_eq!(err.retry_after(), Some(Duration::from_secs(3600)));
            assert_eq!(mock.hits(), 1, "no retry");
            assert!(started.elapsed() < Duration::from_secs(1));

            // Persistent rate limiting stops at the retry limit.
            let always = Script::Reply {
                status: 429,
                headers: vec![],
                body: b"{}".to_vec(),
            };
            let mock = Mock::start(vec![always]).await;
            let err = send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Some(429));
            assert_eq!(mock.hits(), 1 + QUICK.max_retries as usize);
        });
    }

    #[test]
    fn transient_server_errors_are_retried_then_surfaced_and_others_are_not() {
        run(async {
            let c = client(2000);
            // 503 then 200: recovered.
            let mock =
                Mock::start(vec![reply(503, "{}"), reply(529, "{}"), reply(200, "{}")]).await;
            send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                .await
                .unwrap();
            assert_eq!(mock.hits(), 3);

            // 500 forever: gives up after the limit with the server's status.
            let mock = Mock::start(vec![reply(500, "{}")]).await;
            let err = send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                .await
                .unwrap_err();
            assert_eq!(err.describe(""), "Service unavailable (500)");
            assert_eq!(mock.hits(), 3);

            // Auth, validation, not-found: exactly one request each.
            for status in [400, 401, 403, 404, 413, 422] {
                let mock = Mock::start(vec![reply(status, "{}")]).await;
                assert!(send_retrying(|| c.get(mock.url("/")), MAX, &QUICK)
                    .await
                    .is_err());
                assert_eq!(mock.hits(), 1, "{status} must not be retried");
            }
        });
    }

    #[test]
    fn a_connection_failure_is_reported_and_retried_but_never_a_crash() {
        run(async {
            // A host that cannot be resolved (`.invalid` is reserved for exactly this),
            // which fails at once on every OS, unlike a refused port.
            let nowhere = "http://coucou-test.invalid/";
            let c = client(5000);
            let err = send(c.get(nowhere), MAX).await.unwrap_err();
            assert_eq!(err, ApiError::Connect);
            assert_eq!(err.describe(""), "No connection");
            assert!(err.is_transient());

            // It is retried, within the limits, and then reported.
            let started = Instant::now();
            let err = send_retrying(|| c.get(nowhere), MAX, &QUICK)
                .await
                .unwrap_err();
            assert_eq!(err, ApiError::Connect);
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "{:?}",
                started.elapsed()
            );

            // A port nothing listens on is "no connection" too (slowly, on Windows).
            let addr = {
                let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                l.local_addr().unwrap()
            };
            let err = send(client(10_000).get(format!("http://{addr}/")), MAX)
                .await
                .unwrap_err();
            assert_eq!(err, ApiError::Connect);

            // The server hanging up mid-exchange is a transport failure, not a hang.
            let mock = Mock::start(vec![Script::Hangup]).await;
            assert_eq!(
                send(c.get(mock.url("/")), MAX).await.unwrap_err(),
                ApiError::Transport
            );
        });
    }

    #[test]
    fn a_service_that_never_answers_times_out_and_is_not_retried() {
        run(async {
            let mock = Mock::start(vec![Script::Hang]).await;
            let c = client(150);
            let started = Instant::now();
            let err = send_retrying(
                || c.get(mock.url("/")),
                MAX,
                &Retry {
                    attempt_timeout: Duration::from_millis(150),
                    ..QUICK
                },
            )
            .await
            .unwrap_err();
            assert_eq!(err, ApiError::Timeout);
            assert_eq!(err.describe(""), "Timed out");
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(
                mock.hits(),
                1,
                "a timeout may have been processed: no second attempt"
            );
        });
    }

    #[test]
    fn a_slow_drip_is_bounded_by_the_same_timeout() {
        run(async {
            // Headers promise 1000 bytes; ten arrive and then nothing.
            let mock = Mock::start(vec![Script::Truncated {
                claimed: 1000,
                body: vec![b'x'; 10],
            }])
            .await;
            // The server closes right after, which reads as a broken body.
            let err = send(client(500).get(mock.url("/")), MAX).await.unwrap_err();
            assert!(
                matches!(err, ApiError::Transport | ApiError::Timeout),
                "{err:?}"
            );
        });
    }

    #[test]
    fn malformed_and_oversized_responses_are_errors_not_empty_successes() {
        run(async {
            let c = client(3000);
            // Not JSON, empty, and valid-but-wrong-shape JSON all fail the JSON read
            // or are left for the caller to judge.
            for body in ["{not json", "", "<html>oops</html>"] {
                let mock = Mock::start(vec![reply(200, body)]).await;
                assert_eq!(
                    send_json(c.get(mock.url("/")), MAX).await.unwrap_err(),
                    ApiError::Malformed,
                    "{body:?}"
                );
            }

            // A declared length over the cap is refused before a byte is read...
            let big = Script::Reply {
                status: 200,
                headers: vec![],
                body: vec![b'x'; 2048],
            };
            let mock = Mock::start(vec![big]).await;
            assert_eq!(
                send(c.get(mock.url("/")), 1024).await.unwrap_err(),
                ApiError::TooLarge
            );

            // ...and one with no length is cut off when it passes the cap.
            let mock = Mock::start(vec![Script::Endless {
                total: 50 * 1024 * 1024,
            }])
            .await;
            let started = Instant::now();
            assert_eq!(
                send(c.get(mock.url("/")), 1024 * 1024).await.unwrap_err(),
                ApiError::TooLarge
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "it stopped reading at the cap"
            );
        });
    }

    #[test]
    fn a_failure_does_not_stop_the_next_request() {
        run(async {
            let c = client(1000);
            let mock = Mock::start(vec![
                reply(401, "{}"),
                Script::Hangup,
                reply(200, r#"{"ok":1}"#),
            ])
            .await;
            assert!(send_json(c.get(mock.url("/")), MAX).await.is_err());
            assert!(send_json(c.get(mock.url("/")), MAX).await.is_err());
            assert_eq!(send_json(c.get(mock.url("/")), MAX).await.unwrap()["ok"], 1);
        });
    }

    // ── Backoff, cancellation ────────────────────────────────────────────────

    #[test]
    fn backoff_doubles_is_capped_jittered_and_honours_retry_after() {
        let r = Retry {
            max_retries: 5,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(8),
            deadline: Duration::from_secs(100),
            attempt_timeout: Duration::from_secs(90),
        };
        // At the low end of the jitter the wait is half the exponential; at the
        // high end it is the whole of it.
        let ms = |attempt, rnd| backoff_delay(&r, attempt, None, rnd).unwrap().as_millis();
        assert_eq!((ms(0, 0.0), ms(0, 1.0)), (500, 1000));
        assert_eq!((ms(1, 0.0), ms(1, 1.0)), (1000, 2000));
        assert_eq!((ms(2, 0.0), ms(2, 1.0)), (2000, 4000));
        assert_eq!((ms(3, 0.0), ms(3, 1.0)), (4000, 8000));
        assert_eq!(
            (ms(4, 1.0), ms(30, 1.0)),
            (8000, 8000),
            "capped, and no overflow far out"
        );
        // Out-of-range randomness is clamped.
        assert_eq!(ms(0, 7.0), 1000);
        // Retry-After raises the wait but never lowers it; too long means stop.
        assert_eq!(
            backoff_delay(&r, 0, Some(Duration::from_secs(5)), 0.0),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            backoff_delay(&r, 3, Some(Duration::from_secs(1)), 0.0),
            Some(Duration::from_secs(4))
        );
        assert_eq!(
            backoff_delay(&r, 0, Some(Duration::from_secs(31)), 0.0),
            None
        );
        // The real source of randomness stays in range.
        for _ in 0..1000 {
            let x = unit_random();
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn the_deadline_stops_retries_that_would_run_past_it() {
        run(async {
            let c = client(2000);
            let mock = Mock::start(vec![reply(503, "{}")]).await;
            let tight = Retry {
                deadline: Duration::from_millis(5),
                ..QUICK
            };
            let err = send_retrying(|| c.get(mock.url("/")), MAX, &tight)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Some(503));
            assert_eq!(
                mock.hits(),
                1,
                "the first wait alone would have passed the deadline"
            );
        });
    }

    #[test]
    fn dropping_the_future_cancels_the_request_in_flight() {
        run(async {
            let mock = Mock::start(vec![Script::Hang]).await;
            let c = client(30_000);
            let url = mock.url("/");
            let task =
                tokio::spawn(
                    async move { send_retrying(|| c.get(url.clone()), MAX, &QUICK).await },
                );
            while mock.hits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            // The server sees the client hang up: the connection really closed.
            tokio::time::timeout(Duration::from_secs(3), mock.client_gone.notified())
                .await
                .expect("the connection stayed open after cancel");
        });
    }

    #[test]
    fn cancelling_during_a_backoff_wait_ends_promptly_and_stops_retrying() {
        run(async {
            let c = client(2000);
            let mock = Mock::start(vec![reply(503, "{}")]).await;
            let url = mock.url("/");
            // A wait far longer than the test, with a deadline that allows it.
            let slow = Retry {
                base: Duration::from_secs(3600),
                cap: Duration::from_secs(3600),
                deadline: Duration::from_secs(100_000),
                ..QUICK
            };
            let task =
                tokio::spawn(async move { send_retrying(|| c.get(url.clone()), MAX, &slow).await });
            while mock.hits() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            task.abort();
            assert!(tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled());
            assert_eq!(mock.hits(), 1);
        });
    }

    // ── Credentials and redirects ────────────────────────────────────────────

    #[test]
    fn credentials_are_sent_and_marked_sensitive() {
        run(async {
            let mock = Mock::start(vec![reply(200, "{}")]).await;
            let c = client(2000);
            send(
                c.get(mock.url("/"))
                    .header("authorization", bearer("tok-123").unwrap()),
                MAX,
            )
            .await
            .unwrap();
            assert_eq!(
                mock.seen.lock().unwrap()[0].header("authorization"),
                Some("Bearer tok-123")
            );
            let header = bearer("tok-123").unwrap();
            assert!(header.is_sensitive());
            assert!(
                !format!("{header:?}").contains("tok-123"),
                "a debug print must not show it"
            );
            // A key HTTP cannot carry is a bad key, not a network fault.
            assert_eq!(secret("two\nlines").unwrap_err().code(), Some(401));
        });
    }

    #[test]
    fn a_redirect_to_another_host_or_port_never_receives_the_key() {
        run(async {
            let elsewhere = Mock::start(vec![reply(200, "{}")]).await;
            let target = elsewhere.url("/stolen");
            let redirect = Script::Reply {
                status: 302,
                headers: vec![("location", target)],
                body: vec![],
            };
            let origin = Mock::start(vec![redirect]).await;

            let err = send(
                client(2000)
                    .get(origin.url("/"))
                    .header("x-api-key", secret("sk-private").unwrap()),
                MAX,
            )
            .await
            .unwrap_err();
            assert_eq!(
                err.code(),
                Some(302),
                "the redirect is reported, not followed"
            );
            assert_eq!(origin.hits(), 1);
            assert_eq!(elsewhere.hits(), 0, "the key never left");
        });
    }

    #[test]
    fn which_redirects_are_allowed() {
        let u = |s: &str| Url::parse(s).unwrap();
        // Same host and port, whatever the path: fine.
        assert!(redirect_allowed(
            &u("https://n8n.example.com/a"),
            &u("https://n8n.example.com/b")
        ));
        // The usual http → https upgrade.
        assert!(redirect_allowed(
            &u("http://n8n.example.com/"),
            &u("https://n8n.example.com/")
        ));
        // Not: another host, a downgrade, another port, a lookalike.
        assert!(!redirect_allowed(
            &u("https://n8n.example.com/"),
            &u("https://evil.example.net/")
        ));
        assert!(!redirect_allowed(
            &u("https://n8n.example.com/"),
            &u("http://n8n.example.com/")
        ));
        assert!(!redirect_allowed(
            &u("https://n8n.example.com/"),
            &u("https://n8n.example.com:8443/")
        ));
        assert!(!redirect_allowed(
            &u("https://n8n.example.com/"),
            &u("https://n8n.example.com.evil.net/")
        ));
        assert!(!redirect_allowed(
            &u("http://n8n.example.com/"),
            &u("https://n8n.example.com:444/")
        ));
    }

    // ── Found in review of the merged code ───────────────────────────────────

    #[test]
    fn a_redirect_never_steps_down_from_https_to_http() {
        let u = |s: &str| Url::parse(s).unwrap();
        // Same host and the same (explicit) port: only the scheme changes, and the key
        // would travel in clear text.
        assert!(!redirect_allowed(
            &u("https://n8n.example.com:8080/"),
            &u("http://n8n.example.com:8080/")
        ));
        assert!(!redirect_allowed(
            &u("https://n8n.example.com/"),
            &u("http://n8n.example.com:443/")
        ));
        // The other direction, on the same port, is an upgrade and stays allowed.
        assert!(redirect_allowed(
            &u("http://n8n.example.com:8080/"),
            &u("https://n8n.example.com:8080/")
        ));
    }

    #[test]
    fn a_provider_cannot_make_a_credential_appear_in_an_error_message() {
        run(async {
            let c = client(2000);
            // A server that quotes the credential back in its error, for each way
            // Coucou sends one.
            let echo = |token: &str| {
                reply(
                    403,
                    &format!(r#"{{"error":{{"message":"rejected {token} for this request"}}}}"#),
                )
            };

            // Bearer token.
            let mock = Mock::start(vec![echo("tok-bearer-12345")]).await;
            let err = send(
                c.get(mock.url("/"))
                    .header("authorization", bearer("tok-bearer-12345").unwrap()),
                MAX,
            )
            .await
            .unwrap_err();
            let shown = err.describe("forbidden");
            assert!(!shown.contains("tok-bearer-12345"), "{shown}");
            assert!(!format!("{err:?}").contains("tok-bearer-12345"));
            assert!(shown.contains("[redacted]"), "{shown}");

            // A custom key header (x-api-key, X-N8N-API-KEY).
            let mock = Mock::start(vec![echo("sk-custom-header-key")]).await;
            let err = send(
                c.get(mock.url("/"))
                    .header("x-n8n-api-key", secret("sk-custom-header-key").unwrap()),
                MAX,
            )
            .await
            .unwrap_err();
            assert!(!err.describe("").contains("sk-custom-header-key"));

            // Basic auth: the server may quote either the encoded value or the key itself.
            let basic = secret("Basic c2stbGl2ZV9zZWNyZXRrZXk6").unwrap(); // "sk-live_secretkey:"
            for quoted in ["c2stbGl2ZV9zZWNyZXRrZXk6", "sk-live_secretkey"] {
                let mock = Mock::start(vec![echo(quoted)]).await;
                let err = send(
                    c.get(mock.url("/")).header("authorization", basic.clone()),
                    MAX,
                )
                .await
                .unwrap_err();
                let shown = err.describe("");
                assert!(!shown.contains(quoted), "{quoted} leaked: {shown}");
            }

            // A credential in the URL (an n8n address with a user and password).
            let mock = Mock::start(vec![echo("p4ssw0rd-in-url")]).await;
            let url = format!("http://admin:p4ssw0rd-in-url@{}/", mock.addr);
            let err = send(c.get(url), MAX).await.unwrap_err();
            assert!(!err.describe("").contains("p4ssw0rd-in-url"));

            // A secret cut in half by the length limit must not leave its first half behind.
            let padding = "x".repeat(MESSAGE_MAX_CHARS - 8);
            let body = format!(r#"{{"message":"{padding}tok-split-across-the-limit"}}"#);
            let mock = Mock::start(vec![reply(400, &body)]).await;
            let err = send(
                c.get(mock.url("/")).header(
                    "authorization",
                    bearer("tok-split-across-the-limit").unwrap(),
                ),
                MAX,
            )
            .await
            .unwrap_err();
            let shown = err.describe("");
            assert!(!shown.contains("tok-spl"), "{shown}");

            // Ordinary explanations still come through untouched.
            let mock = Mock::start(vec![reply(400, r#"{"message":"API key is invalid"}"#)]).await;
            let err = send(
                c.get(mock.url("/"))
                    .header("authorization", bearer("tok-bearer-12345").unwrap()),
                MAX,
            )
            .await
            .unwrap_err();
            assert_eq!(
                err.describe(""),
                "Request rejected (400) — API key is invalid"
            );
        });
    }

    #[test]
    fn base64_decoding_reads_what_the_encoder_wrote() {
        assert_eq!(base64_decode("Zm9vOg==").as_deref(), Some("foo:"));
        assert_eq!(base64_decode("Zm9vOg").as_deref(), Some("foo:"));
        assert_eq!(
            base64_decode("c2stbGl2ZV9zZWNyZXRrZXk6").as_deref(),
            Some("sk-live_secretkey:")
        );
        assert_eq!(base64_decode("not base64!"), None);
        assert_eq!(base64_decode("").as_deref(), Some(""));
    }

    #[test]
    fn the_overall_deadline_also_limits_one_slow_attempt() {
        run(async {
            // The service never answers. The client would wait 3 s, but the retry
            // budget is 400 ms, and that is what ends it.
            let mock = Mock::start(vec![Script::Hang]).await;
            let c = client(3000);
            let tight = Retry {
                deadline: Duration::from_millis(400),
                ..QUICK
            };
            let started = Instant::now();
            let err = send_retrying(|| c.get(mock.url("/")), MAX, &tight)
                .await
                .unwrap_err();
            assert_eq!(err, ApiError::Timeout);
            assert!(
                started.elapsed() < Duration::from_millis(1500),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(mock.hits(), 1);
        });
    }

    #[test]
    fn a_client_always_has_a_timeout() {
        // The old pollers fell back to `Client::default()`, which has none.
        assert!(poll_client().is_ok());
        assert!(chat_client().is_ok());
        assert_eq!(POLL_TIMEOUT, Duration::from_secs(10));
        assert_eq!(CHAT_TIMEOUT, Duration::from_secs(90));
    }

    // ── Live checks (never run by default) ───────────────────────────────────
    //
    // Reproduce with:  cargo test -p coucou --lib live_ -- --ignored --nocapture
    //
    // `live_every_endpoint_rejects_a_fake_key_the_way_the_code_expects` needs only
    // the network: it sends an obviously fake key to each real endpoint, and costs
    // nothing. `live_anthropic_models_with_the_stored_key` uses the key stored in
    // the Credential Manager / Secret Service, calls the free model-list endpoint,
    // and prints only a status and a count. Neither prints a key or a body.

    #[test]
    #[ignore = "live: needs the network"]
    fn live_every_endpoint_rejects_a_fake_key_the_way_the_code_expects() {
        run(async {
            let fake = "coucou-live-check-invalid-key";
            let c = build_client(Duration::from_secs(10)).unwrap();
            // (name, request, statuses an invalid key is expected to produce)
            let checks: Vec<(&str, RequestBuilder, &[u16])> = vec![
                (
                    "anthropic",
                    c.get("https://api.anthropic.com/v1/models")
                        .header("x-api-key", fake)
                        .header("anthropic-version", "2023-06-01"),
                    &[401],
                ),
                (
                    "github",
                    c.get("https://api.github.com/user")
                        .header("authorization", format!("Bearer {fake}"))
                        .header("user-agent", "Coucou"),
                    &[401],
                ),
                (
                    "stripe",
                    c.get("https://api.stripe.com/v1/balance")
                        .header("authorization", format!("Bearer {fake}")),
                    &[401],
                ),
                (
                    "vercel",
                    c.get("https://api.vercel.com/v6/deployments?limit=1")
                        .header("authorization", format!("Bearer {fake}")),
                    &[403],
                ),
                (
                    "resend",
                    c.get("https://api.resend.com/emails?limit=1")
                        .header("authorization", format!("Bearer {fake}")),
                    &[400, 401],
                ),
                (
                    "notion",
                    c.get("https://api.notion.com/v1/users/me")
                        .header("authorization", format!("Bearer {fake}"))
                        .header("notion-version", "2022-06-28"),
                    &[401],
                ),
                (
                    "calcom",
                    c.get("https://api.cal.com/v2/bookings?status=upcoming")
                        .header("authorization", format!("Bearer {fake}"))
                        .header("cal-api-version", "2024-08-13"),
                    &[401, 403],
                ),
            ];
            for (name, request, expected) in checks {
                let err = send(request, POLL_MAX_BODY).await.expect_err(name);
                println!("live {name}: {}", err.describe("forbidden"));
                assert!(
                    err.code().is_some_and(|code| expected.contains(&code)),
                    "{name}: unexpected outcome {:?}",
                    err.code()
                );
            }
        });
    }

    #[test]
    #[ignore = "live: needs the network and a stored Anthropic key"]
    fn live_anthropic_models_with_the_stored_key() {
        let Some(key) = crate::secrets::get("anthropic-api-key") else {
            println!("live: no Anthropic key is stored; nothing was sent");
            return;
        };
        run(async {
            let c = chat_client().unwrap();
            let request = c
                .get("https://api.anthropic.com/v1/models?limit=100")
                .header("x-api-key", secret(&key).unwrap())
                .header("anthropic-version", "2023-06-01");
            match send_json(request, CHAT_MAX_BODY).await {
                Ok(models) => {
                    let ids: Vec<&str> = models["data"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|m| m["id"].as_str()).collect())
                        .unwrap_or_default();
                    println!(
                        "live anthropic /v1/models: 200, {} models listed",
                        ids.len()
                    );
                    for wanted in ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"] {
                        println!(
                            "live model {wanted}: {}",
                            if ids.iter().any(|id| id.starts_with(wanted)) {
                                "listed"
                            } else {
                                "NOT listed"
                            }
                        );
                    }
                }
                Err(err) => panic!(
                    "live anthropic /v1/models failed: {}",
                    err.describe("forbidden")
                ),
            }
        });
    }
}
