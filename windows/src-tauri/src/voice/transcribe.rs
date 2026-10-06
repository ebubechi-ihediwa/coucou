// Speech to text: one recording in, one piece of text out.
//
// Anthropic has no speech-to-text, so this talks to a second service, chosen in
// Settings: OpenAI or Groq. Both take the same request (an OpenAI-style multipart
// upload) and answer the same JSON, so a provider is only an address, a model and the
// name of its key in the Credential Manager. It is a closed choice of two, not a
// framework.
//
// It goes through the shared HTTP layer: bounded body, bounded time, bounded retries,
// error text that never carries the key, the URL or the response body.
//
// Nothing here is trusted. The reply is read as JSON, must hold a string called
// `text`, and is cleaned and capped before anyone sees it; anything else is an error.

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;

use crate::http::{self, ApiError, Retry};
use crate::{log, secrets};

/// The speech services push-to-talk can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Groq,
}

pub const DEFAULT_PROVIDER: Provider = Provider::OpenAi;

impl Provider {
    /// The id written in settings.
    pub fn id(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai",
            Provider::Groq => "groq",
        }
    }

    /// Exactly an id from `id`; anything else is not a provider.
    pub fn from_id(id: &str) -> Option<Provider> {
        [Provider::OpenAi, Provider::Groq]
            .into_iter()
            .find(|p| p.id() == id)
    }

    /// What the person calls it.
    pub fn label(self) -> &'static str {
        match self {
            Provider::OpenAi => "OpenAI",
            Provider::Groq => "Groq",
        }
    }

    pub fn endpoint(self) -> &'static str {
        match self {
            Provider::OpenAi => "https://api.openai.com/v1/audio/transcriptions",
            Provider::Groq => "https://api.groq.com/openai/v1/audio/transcriptions",
        }
    }

    /// The model the request names. The only place each is written down.
    pub fn model(self) -> &'static str {
        match self {
            Provider::OpenAi => "gpt-4o-mini-transcribe",
            Provider::Groq => "whisper-large-v3-turbo",
        }
    }

    /// The Credential Manager entry holding this provider's key (see `secrets`).
    pub fn key_name(self) -> &'static str {
        match self {
            Provider::OpenAi => "openai-api-key",
            Provider::Groq => "groq-api-key",
        }
    }

    pub fn no_key_message(self) -> String {
        format!("Add your {} key in Settings to use voice.", self.label())
    }
}

/// Up to 45 s of audio goes up, and a short text comes back.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The answer is a line or two of text.
const MAX_RESPONSE: usize = 64 * 1024;
/// 45 s of speech is well under 1,500 characters. More than this is not a transcript.
const MAX_TRANSCRIPT_CHARS: usize = 4_000;

/// Asking again is safe (a transcription changes nothing on the server) but a person
/// is waiting, so: one more try for a rate limit or a brief outage, and a hard stop.
pub const RETRY: Retry = Retry {
    max_retries: 1,
    base: Duration::from_secs(1),
    cap: Duration::from_secs(2),
    deadline: Duration::from_secs(40),
    attempt_timeout: TIMEOUT,
};

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// The one thing the rest of the app needs from a speech service. `Err` is a
/// sentence fit to show.
pub trait Transcriber: Send + Sync + 'static {
    fn transcribe(&self, provider: Provider, wav: Vec<u8>) -> TranscribeFuture<'_>;
}

/// The real services, over the network.
pub struct Speech;

impl Transcriber for Speech {
    fn transcribe(&self, provider: Provider, wav: Vec<u8>) -> TranscribeFuture<'_> {
        Box::pin(async move {
            let key = secrets::get(provider.key_name()).ok_or_else(|| provider.no_key_message())?;
            let client = client().map_err(|e| voice_error(provider, &e))?;
            call_at(
                &client,
                provider.endpoint(),
                provider.model(),
                &key,
                wav,
                &RETRY,
            )
            .await
            .map_err(|e| {
                let shown = voice_error(provider, &e);
                log::line(format!("voice: transcription failed: {shown}"));
                shown
            })
        })
    }
}

fn client() -> Result<Client, ApiError> {
    static CLIENT: LazyLock<Result<Client, ApiError>> =
        LazyLock::new(|| http::build_client(TIMEOUT));
    CLIENT.clone()
}

/// One transcription request. Dropping the future cancels it.
pub(crate) async fn call_at(
    client: &Client,
    endpoint: &str,
    model: &str,
    key: &str,
    wav: Vec<u8>,
    retry: &Retry,
) -> Result<String, ApiError> {
    let auth = http::bearer(key)?;
    let boundary = boundary_for(&wav);
    let content_type = format!("multipart/form-data; boundary={boundary}");
    let body = multipart_body(&boundary, model, &wav);
    drop(wav);
    let bytes = http::send_retrying(
        || {
            client
                .post(endpoint)
                .header(AUTHORIZATION, auth.clone())
                .header(CONTENT_TYPE, content_type.clone())
                .body(body.clone())
        },
        MAX_RESPONSE,
        retry,
    )
    .await?;
    parse_transcript(&bytes)
}

/// A boundary that does not occur in the audio.
fn boundary_for(wav: &[u8]) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    loop {
        let random = |_| RandomState::new().build_hasher().finish();
        let boundary = format!("coucou{:016x}{:016x}", random(0), random(1));
        let needle = boundary.as_bytes();
        if !wav.windows(needle.len()).any(|w| w == needle) {
            return boundary;
        }
    }
}

/// `multipart/form-data` by hand: three small parts, no extra crate.
pub(crate) fn multipart_body(boundary: &str, model: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    for (name, value) in [("model", model), ("response_format", "json")] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// The text out of the service's answer, cleaned: control characters and runs of
/// whitespace become single spaces. A reply that is not `{"text": "…"}`, or whose
/// text is absurdly long, is `Malformed`.
pub(crate) fn parse_transcript(body: &[u8]) -> Result<String, ApiError> {
    let json: Value = serde_json::from_slice(body).map_err(|_| ApiError::Malformed)?;
    let text = json
        .get("text")
        .and_then(Value::as_str)
        .ok_or(ApiError::Malformed)?;
    if text.chars().count() > MAX_TRANSCRIPT_CHARS {
        return Err(ApiError::Malformed);
    }
    Ok(text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" "))
}

/// Whether the service's explanation of a 429 is about money rather than speed.
fn mentions_credit(message: &str) -> bool {
    let lower = message.to_lowercase();
    ["quota", "billing", "credit"]
        .iter()
        .any(|word| lower.contains(word))
}

/// What the island may say when a transcription fails: the kind of failure and what
/// to do, never the key, the URL or the raw body.
pub(crate) fn voice_error(provider: Provider, err: &ApiError) -> String {
    use ApiError::*;
    let service = provider.label();
    match err {
        Status { code: 401, .. } => {
            format!("The {service} key was rejected (401). Check it in Settings.")
        }
        Status { code: 403, .. } => {
            format!("{service} refused this request (403). The key may lack access.")
        }
        // A 429 can mean "too many requests" or "this account has no credit left"
        // (OpenAI's `insufficient_quota`). The two need different things from the
        // person, so the service's own words decide which one is said.
        Status { code: 429, message: Some(m), .. } if mentions_credit(m) => format!(
            "{service} says this account has no credit or quota left (429). Check your {service} billing settings."
        ),
        Status { code: 429, message: Some(m), .. } => {
            format!("{service} is limiting requests (429): {m}")
        }
        Status { code: 429, .. } => {
            format!("{service} is rate limiting requests (429). Try again in a moment.")
        }
        Status { code, .. } if *code >= 500 => {
            format!("{service} is unavailable ({code}). Try again in a moment.")
        }
        Status {
            code,
            message: Some(m),
            ..
        } => format!("I couldn't transcribe that ({code}): {m}"),
        Status { code, .. } => format!("I couldn't transcribe that ({code})."),
        Timeout => "Transcription took too long. Try again.".into(),
        Connect | Transport => "I couldn't transcribe that. Check your connection.".into(),
        TooLarge | Malformed => format!("{service} sent a reply I couldn't read."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::tests::{reply, run, Mock, Script};

    const QUICK: Retry = Retry {
        max_retries: 1,
        base: Duration::from_millis(10),
        cap: Duration::from_millis(40),
        deadline: Duration::from_secs(10),
        attempt_timeout: Duration::from_secs(2),
    };

    fn client(ms: u64) -> Client {
        http::build_client(Duration::from_millis(ms)).unwrap()
    }

    /// A tiny WAV: the whole request then fits in the one read the mock server makes.
    fn wav() -> Vec<u8> {
        crate::voice::audio::encode_wav(&[100; 4])
    }

    async fn go(mock: &Mock, retry: &Retry) -> Result<String, ApiError> {
        call_at(
            &client(2000),
            &mock.url("/v1/audio/transcriptions"),
            Provider::OpenAi.model(),
            "sk-voice-key",
            wav(),
            retry,
        )
        .await
    }

    #[test]
    fn a_transcription_request_carries_the_key_and_the_recording_as_a_form() {
        run(async {
            let mock =
                Mock::start(vec![reply(200, r#"{"text":"Hey Coucou, open Notepad."}"#)]).await;
            let text = go(&mock, &QUICK).await.unwrap();
            assert_eq!(text, "Hey Coucou, open Notepad.");
            let seen = mock.seen_headers(0);
            assert_eq!(
                seen.get("authorization").map(String::as_str),
                Some("Bearer sk-voice-key")
            );
            let content_type = seen.get("content-type").unwrap();
            assert!(
                content_type.starts_with("multipart/form-data; boundary=coucou"),
                "{content_type}"
            );
        });
    }

    #[test]
    fn the_form_names_the_model_and_holds_the_wav_untouched() {
        let wav = wav();
        let body = multipart_body("BOUND", Provider::OpenAi.model(), &wav);
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--BOUND\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-4o-mini-transcribe\r\n"));
        assert!(text.contains("name=\"response_format\"\r\n\r\njson\r\n"));
        assert!(text.contains(
            "name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF"
        ));
        assert!(text.ends_with("\r\n--BOUND--\r\n"));
        // The audio is in there byte for byte.
        assert!(body.windows(wav.len()).any(|w| w == wav.as_slice()));
        // And the key is nowhere in what is uploaded.
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn the_boundary_never_collides_with_the_audio() {
        let b = boundary_for(&wav());
        assert!(b.starts_with("coucou") && b.len() == 6 + 32);
        assert!(!wav().windows(b.len()).any(|w| w == b.as_bytes()));
    }

    #[test]
    fn the_text_is_cleaned_and_anything_else_is_refused() {
        assert_eq!(
            parse_transcript(br#"{"text":"  Open\n\tNotepad.  "}"#).unwrap(),
            "Open Notepad."
        );
        assert_eq!(
            parse_transcript(br#"{"text":"a\u0000b\u0007c"}"#).unwrap(),
            "a b c"
        );
        assert_eq!(parse_transcript(br#"{"text":""}"#).unwrap(), "");
        assert_eq!(
            parse_transcript(br#"{"text":"ok","usage":{"x":1}}"#).unwrap(),
            "ok"
        );
        for bad in [
            &b"<html>captive portal</html>"[..],
            br#"{}"#,
            br#"{"text":42}"#,
            br#"{"text":null}"#,
            br#"{"text":["a"]}"#,
            br#"{"transcript":"hi"}"#,
            br#"[]"#,
            br#""text""#,
            b"",
        ] {
            assert_eq!(
                parse_transcript(bad),
                Err(ApiError::Malformed),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(r#"{{"text":"{}"}}"#, "a".repeat(MAX_TRANSCRIPT_CHARS + 1));
        assert_eq!(parse_transcript(long.as_bytes()), Err(ApiError::Malformed));
        let fits = format!(r#"{{"text":"{}"}}"#, "a".repeat(MAX_TRANSCRIPT_CHARS));
        assert!(parse_transcript(fits.as_bytes()).is_ok());
    }

    #[test]
    fn a_rejected_key_is_reported_once_and_never_echoed() {
        run(async {
            let body = r#"{"error":{"message":"Incorrect API key provided: sk-voice-key","type":"invalid_request_error"}}"#;
            let mock = Mock::start(vec![reply(401, body)]).await;
            let err = go(&mock, &QUICK).await.unwrap_err();
            let shown = voice_error(Provider::OpenAi, &err);
            assert_eq!(
                shown,
                "The OpenAI key was rejected (401). Check it in Settings."
            );
            assert!(!shown.contains("sk-voice-key"));
            assert_eq!(mock.hits(), 1, "a bad key is not retried");
        });
    }

    #[test]
    fn a_key_quoted_back_in_another_error_is_redacted() {
        run(async {
            let body = r#"{"error":{"message":"bad request for sk-voice-key"}}"#;
            let mock = Mock::start(vec![reply(400, body)]).await;
            let shown = voice_error(Provider::OpenAi, &go(&mock, &QUICK).await.unwrap_err());
            assert_eq!(
                shown,
                "I couldn't transcribe that (400): bad request for [redacted]"
            );
            assert!(!shown.contains("sk-voice-key") && !shown.contains("127.0.0.1"));
        });
    }

    #[test]
    fn a_malformed_answer_fails_safely() {
        run(async {
            for body in ["<html>oops</html>", "{}", r#"{"text":7}"#] {
                let mock = Mock::start(vec![reply(200, body)]).await;
                let err = go(&mock, &QUICK).await.unwrap_err();
                assert_eq!(err, ApiError::Malformed, "{body}");
                assert_eq!(
                    voice_error(Provider::OpenAi, &err),
                    "OpenAI sent a reply I couldn't read."
                );
            }
        });
    }

    #[test]
    fn rate_limits_are_retried_once_and_then_reported() {
        run(async {
            let mock = Mock::start(vec![reply(429, "{}"), reply(200, r#"{"text":"fine"}"#)]).await;
            assert_eq!(go(&mock, &QUICK).await.unwrap(), "fine");
            assert_eq!(mock.hits(), 2);

            let mock = Mock::start(vec![reply(429, "{}")]).await;
            let err = go(&mock, &QUICK).await.unwrap_err();
            assert_eq!(mock.hits(), 2, "one try and one retry, no more");
            assert_eq!(
                voice_error(Provider::OpenAi, &err),
                "OpenAI is rate limiting requests (429). Try again in a moment."
            );
        });
    }

    #[test]
    fn a_429_for_lack_of_credit_is_not_called_rate_limiting() {
        run(async {
            // What OpenAI sends for an account with no credit.
            let body = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
            let mock = Mock::start(vec![reply(429, body)]).await;
            let shown = voice_error(Provider::OpenAi, &go(&mock, &QUICK).await.unwrap_err());
            assert_eq!(
                shown,
                "OpenAI says this account has no credit or quota left (429). Check your OpenAI billing settings."
            );
            assert!(!shown.contains("rate limit") && !shown.contains("sk-voice-key"));

            // A genuine rate limit with words of its own keeps them; a bare one stays generic.
            let busy = r#"{"error":{"message":"Rate limit reached for requests per minute.","type":"requests"}}"#;
            let mock = Mock::start(vec![reply(429, busy)]).await;
            let shown = voice_error(Provider::OpenAi, &go(&mock, &QUICK).await.unwrap_err());
            assert_eq!(
                shown,
                "OpenAI is limiting requests (429): Rate limit reached for requests per minute."
            );
        });
    }

    #[test]
    fn each_provider_has_its_own_address_model_and_key() {
        use Provider::*;
        assert_eq!(
            OpenAi.endpoint(),
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(
            Groq.endpoint(),
            "https://api.groq.com/openai/v1/audio/transcriptions"
        );
        assert_eq!(
            (OpenAi.model(), Groq.model()),
            ("gpt-4o-mini-transcribe", "whisper-large-v3-turbo")
        );
        assert_eq!(
            (OpenAi.key_name(), Groq.key_name()),
            ("openai-api-key", "groq-api-key")
        );
        for p in [OpenAi, Groq] {
            assert!(
                p.endpoint().starts_with("https://"),
                "audio only ever goes over https"
            );
            assert_eq!(Provider::from_id(p.id()), Some(p));
            assert!(
                crate::secrets::KNOWN_KEYS.contains(&p.key_name()),
                "its key can be stored"
            );
        }
        // Exactly the ids settings writes; nothing near-miss is a provider.
        for bad in [
            "",
            "OpenAI",
            "GROQ",
            "openai ",
            "anthropic",
            "https://evil.example",
        ] {
            assert_eq!(Provider::from_id(bad), None, "{bad:?}");
        }
        assert_eq!(
            Groq.no_key_message(),
            "Add your Groq key in Settings to use voice."
        );
        assert_eq!(DEFAULT_PROVIDER, OpenAi);
    }

    #[test]
    fn a_groq_request_names_its_model_and_sends_its_own_key() {
        run(async {
            let mock =
                Mock::start(vec![reply(200, r#"{"text":"Hey Coucou, open Notepad."}"#)]).await;
            let text = call_at(
                &client(2000),
                &mock.url("/openai/v1/audio/transcriptions"),
                Provider::Groq.model(),
                "gsk-groq-key",
                wav(),
                &QUICK,
            )
            .await
            .unwrap();
            assert_eq!(text, "Hey Coucou, open Notepad.");
            let seen = mock.seen_headers(0);
            assert_eq!(
                seen.get("authorization").map(String::as_str),
                Some("Bearer gsk-groq-key")
            );
        });
        let form = String::from_utf8_lossy(&multipart_body("B", Provider::Groq.model(), &wav()))
            .to_string();
        assert!(form.contains("name=\"model\"\r\n\r\nwhisper-large-v3-turbo\r\n"));
        assert!(
            !form.contains("gpt-4o"),
            "the other provider's model is nowhere in it"
        );
        assert!(form.contains("name=\"response_format\"\r\n\r\njson\r\n"));
    }

    #[test]
    fn errors_name_the_service_that_was_used() {
        let status = |code, message: Option<&str>| ApiError::Status {
            code,
            message: message.map(String::from),
            retry_after: None,
        };
        assert_eq!(
            voice_error(Provider::Groq, &status(401, None)),
            "The Groq key was rejected (401). Check it in Settings."
        );
        assert_eq!(
            voice_error(
                Provider::Groq,
                &status(
                    429,
                    Some("Rate limit reached for model whisper-large-v3-turbo")
                )
            ),
            "Groq is limiting requests (429): Rate limit reached for model whisper-large-v3-turbo"
        );
        assert_eq!(
            voice_error(Provider::Groq, &status(429, Some("Your account has no credits left"))),
            "Groq says this account has no credit or quota left (429). Check your Groq billing settings."
        );
        assert_eq!(
            voice_error(Provider::Groq, &status(503, None)),
            "Groq is unavailable (503). Try again in a moment."
        );
        assert_eq!(
            voice_error(Provider::Groq, &ApiError::Malformed),
            "Groq sent a reply I couldn't read."
        );
        // And never the other one's name.
        for err in [
            status(401, None),
            status(403, None),
            status(429, None),
            status(500, None),
            ApiError::Malformed,
        ] {
            assert!(!voice_error(Provider::Groq, &err).contains("OpenAI"));
            assert!(!voice_error(Provider::OpenAi, &err).contains("Groq"));
        }
    }

    #[test]
    fn a_server_error_is_reported_without_its_body() {
        run(async {
            let mock = Mock::start(vec![reply(
                503,
                r#"{"error":{"message":"internal detail"}}"#,
            )])
            .await;
            let shown = voice_error(Provider::OpenAi, &go(&mock, &QUICK).await.unwrap_err());
            assert_eq!(shown, "OpenAI is unavailable (503). Try again in a moment.");
            assert!(!shown.contains("internal detail"));
        });
    }

    #[test]
    fn a_service_that_never_answers_times_out_and_is_not_asked_twice() {
        run(async {
            let mock = Mock::start(vec![Script::Hang]).await;
            let quick = Retry {
                attempt_timeout: Duration::from_millis(200),
                ..QUICK
            };
            let err = call_at(
                &client(200),
                &mock.url("/"),
                Provider::OpenAi.model(),
                "k",
                wav(),
                &quick,
            )
            .await
            .unwrap_err();
            assert_eq!(err, ApiError::Timeout);
            assert_eq!(mock.hits(), 1);
            assert_eq!(
                voice_error(Provider::OpenAi, &err),
                "Transcription took too long. Try again."
            );
        });
    }

    #[test]
    fn no_connection_is_a_clear_message() {
        run(async {
            let err = call_at(
                &client(5000),
                "http://coucou-test.invalid/",
                Provider::OpenAi.model(),
                "k",
                wav(),
                &QUICK,
            )
            .await
            .unwrap_err();
            assert_eq!(
                voice_error(Provider::OpenAi, &err),
                "I couldn't transcribe that. Check your connection."
            );
        });
    }

    #[test]
    fn a_reply_that_is_too_large_is_refused() {
        run(async {
            let huge = format!(r#"{{"text":"{}"}}"#, "a".repeat(MAX_RESPONSE + 10));
            let mock = Mock::start(vec![reply(200, &huge)]).await;
            assert_eq!(go(&mock, &QUICK).await, Err(ApiError::TooLarge));
        });
    }

    #[test]
    fn cancelling_drops_the_request_without_waiting_for_the_service() {
        run(async {
            let mock = Mock::start(vec![Script::Hang]).await;
            let url = mock.url("/");
            let task = tokio::spawn(async move {
                call_at(
                    &client(30_000),
                    &url,
                    Provider::OpenAi.model(),
                    "k",
                    wav(),
                    &QUICK,
                )
                .await
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        });
    }

    #[test]
    fn a_key_with_a_stray_newline_is_a_bad_key_not_a_network_error() {
        run(async {
            let mock = Mock::start(vec![reply(200, "{}")]).await;
            let err = call_at(
                &client(2000),
                &mock.url("/"),
                Provider::OpenAi.model(),
                "sk-bad\nkey",
                wav(),
                &QUICK,
            )
            .await
            .unwrap_err();
            assert_eq!(
                voice_error(Provider::OpenAi, &err),
                "The OpenAI key was rejected (401). Check it in Settings."
            );
            assert_eq!(
                mock.hits(),
                0,
                "nothing is sent with a key that cannot be sent"
            );
        });
    }

    #[test]
    fn every_failure_kind_reads_differently_and_leaks_nothing() {
        let status = |code, message: Option<&str>| ApiError::Status {
            code,
            message: message.map(String::from),
            retry_after: None,
        };
        let cases = [
            status(403, Some("sk-voice-key")),
            status(404, Some("model not found")),
            status(413, None),
            ApiError::Timeout,
            ApiError::Connect,
            ApiError::Transport,
            ApiError::TooLarge,
            ApiError::Malformed,
        ];
        let mut seen = std::collections::HashSet::new();
        for err in cases {
            let shown = voice_error(Provider::OpenAi, &err);
            assert!(
                !shown.contains("http") && !shown.contains("/v1/"),
                "{shown}"
            );
            seen.insert(shown);
        }
        // Connect/Transport and TooLarge/Malformed read alike on purpose; the rest differ.
        assert_eq!(seen.len(), 6);
    }
}
