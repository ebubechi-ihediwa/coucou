// Integration pollers — the Rust side of StripePoller / GithubPoller /
// VercelPoller / N8nPoller / ResendPoller / NotionPoller / CalcomPoller.
//
// Same endpoints, same first-run delays and intervals as the Swift pollers. Each
// one emits an `integration` event; the island owns the badge, the sound and the
// 60 s auto-clear, exactly as the Swift handlers do.
//
// Nothing is polled until its key exists in the Credential Manager, and no
// request goes anywhere the user has not configured.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use reqwest::header::AUTHORIZATION;
use reqwest::{Client, RequestBuilder};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::http::{self, ApiError};
use crate::island::WINDOW_LABEL;
use crate::log;
use crate::secrets;

/// How long a rate-limited integration is left alone when the service did not say
/// ("Retry-After" wins when it did), and the longest we will hold off.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(5 * 60);
const MAX_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// What the island receives. `event` is only set when something actually changed,
/// which is what drives the pill badge and the sound.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationUpdate {
    pub id: &'static str,
    pub data: Value,
    pub error: Option<String>,
    pub event: Option<IntegrationEvent>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationEvent {
    pub success: bool,
    pub label: String,
    pub detail: Option<String>,
}

fn emit(app: &AppHandle, update: IntegrationUpdate) {
    let _ = app.emit_to(WINDOW_LABEL, "integration", update);
}

/// Every poll below goes through `fetch`/`try_fetch`: one shared client with a
/// timeout that cannot silently vanish, a size limit on what is read back, and a
/// real error — never an empty "success" — when the answer is not usable.
async fn try_fetch(
    build: impl FnOnce(&Client) -> Result<RequestBuilder, ApiError>,
) -> Result<Value, ApiError> {
    let client = http::poll_client()?;
    http::send_json(build(&client)?, http::POLL_MAX_BODY).await
}

/// `try_fetch`, reporting a failure to the island (and the log) before giving up.
async fn fetch(
    app: &AppHandle,
    id: &'static str,
    forbidden: &str,
    build: impl FnOnce(&Client) -> Result<RequestBuilder, ApiError>,
) -> Option<Value> {
    match try_fetch(build).await {
        Ok(json) => Some(json),
        Err(err) => {
            fail(app, id, &err, forbidden);
            None
        }
    }
}

fn fail(app: &AppHandle, id: &'static str, err: &ApiError, forbidden: &str) {
    let message = err.describe(forbidden);
    log::line(format!("{id}: {message}"));
    if err.code() == Some(429) {
        GATE.lock().unwrap().cool_down(id, err.retry_after(), Instant::now());
    }
    emit(
        app,
        IntegrationUpdate { id, data: json!({}), error: Some(message), event: None },
    );
}

/// Who may poll right now. One poll per integration at a time (a Refresh button
/// pressed over and over must not pile up requests), and none while a service has
/// told us to back off.
#[derive(Default)]
struct Gate {
    running: HashSet<&'static str>,
    cooling_until: HashMap<&'static str, Instant>,
}

#[derive(Debug, PartialEq)]
enum Entry {
    Go,
    /// Already polling.
    Busy,
    /// Rate limited; this much longer.
    Cooling(Duration),
}

impl Gate {
    fn enter(&mut self, id: &'static str, now: Instant) -> Entry {
        if let Some(until) = self.cooling_until.get(id).copied() {
            if until > now {
                return Entry::Cooling(until - now);
            }
            self.cooling_until.remove(id);
        }
        if self.running.insert(id) {
            Entry::Go
        } else {
            Entry::Busy
        }
    }

    fn leave(&mut self, id: &'static str) {
        self.running.remove(id);
    }

    fn cool_down(&mut self, id: &'static str, retry_after: Option<Duration>, now: Instant) {
        let wait = retry_after.unwrap_or(DEFAULT_COOLDOWN).min(MAX_COOLDOWN);
        self.cooling_until.insert(id, now + wait);
    }
}

static GATE: LazyLock<Mutex<Gate>> = LazyLock::new(|| Mutex::new(Gate::default()));

/// Releases the integration's slot when the poll ends, however it ends.
struct Running(&'static str);

impl Drop for Running {
    fn drop(&mut self) {
        GATE.lock().unwrap().leave(self.0);
    }
}

/// Runs `poll` unless the integration is already polling or cooling down. A manual
/// refresh during a cooldown is told so instead of being ignored.
async fn guarded(app: &AppHandle, id: &'static str, manual: bool, poll: impl Future<Output = ()>) {
    let entry = GATE.lock().unwrap().enter(id, Instant::now());
    match entry {
        Entry::Go => {
            let _slot = Running(id);
            poll.await;
        }
        Entry::Busy => {}
        Entry::Cooling(left) if manual => {
            let err = ApiError::Status { code: 429, message: None, retry_after: Some(left) };
            emit(
                app,
                IntegrationUpdate { id, data: json!({}), error: Some(err.describe("")), event: None },
            );
        }
        Entry::Cooling(_) => {}
    }
}

/// Set from the tray's Pause item. While it is on, nothing reaches the network:
/// pausing Coucou has to mean pausing Coucou, not just hiding the island.
pub static PAUSED: AtomicBool = AtomicBool::new(false);

pub fn set_paused(on: bool) {
    PAUSED.store(on, Ordering::Relaxed);
}

/// Spawns every poller with the macOS delays and intervals.
pub fn start(app: AppHandle) {
    spawn(app.clone(), "integration_n8n", 3, 15, poll_n8n);
    spawn(app.clone(), "integration_vercel", 5, 30, poll_vercel);
    spawn(app.clone(), "integration_stripe", 6, 30, poll_stripe);
    spawn(app.clone(), "integration_resend", 6, 60, poll_resend);
    spawn(app.clone(), "integration_github", 7, 300, poll_github);
    spawn(app.clone(), "integration_calcom", 8, 300, poll_calcom);
    spawn(app, "integration_notion", 9, 300, poll_notion);
}

/// True when the user has this integration switched on in settings.
fn enabled(app: &AppHandle, id: &str) -> bool {
    app.try_state::<crate::Shared>()
        .map(|shared| {
            let settings = shared.settings.lock().unwrap();
            settings.active_integrations.iter().any(|x| x == id)
        })
        .unwrap_or(false)
}

fn spawn<F, Fut>(app: AppHandle, id: &'static str, delay_secs: u64, every_secs: u64, poll: F)
where
    F: Fn(AppHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(every_secs));
        loop {
            ticker.tick().await;
            // The ticker keeps its cadence; we just decline to do the work. An
            // integration the user switched off, or a paused app, must make no
            // network calls at all — CLAUDE.md allows talking only to services
            // the user configured, and a disabled one is not configured.
            if PAUSED.load(Ordering::Relaxed) || !enabled(&app, id) {
                continue;
            }
            guarded(&app, id, false, poll(app.clone())).await;
        }
    });
}

/// One-shot refresh from the Refresh buttons in the island.
pub async fn poll_once(app: AppHandle, id: &str) {
    let a = app.clone();
    match id {
        "integration_stripe" => guarded(&app, "integration_stripe", true, poll_stripe(a)).await,
        "integration_github" => guarded(&app, "integration_github", true, poll_github(a)).await,
        "integration_vercel" => guarded(&app, "integration_vercel", true, poll_vercel(a)).await,
        "integration_n8n" => guarded(&app, "integration_n8n", true, poll_n8n(a)).await,
        "integration_resend" => guarded(&app, "integration_resend", true, poll_resend(a)).await,
        "integration_notion" => guarded(&app, "integration_notion", true, poll_notion(a)).await,
        "integration_calcom" => guarded(&app, "integration_calcom", true, poll_calcom(a)).await,
        _ => {}
    }
}

/// Remembers the newest id per integration so an event fires once, not on every poll.
struct Seen(Mutex<std::collections::HashMap<&'static str, String>>);

static SEEN: std::sync::LazyLock<Seen> =
    std::sync::LazyLock::new(|| Seen(Mutex::new(std::collections::HashMap::new())));

/// Returns true the first time a given id is seen (and false on the very first
/// load, which only fills the card).
fn is_new(key: &'static str, id: &str) -> bool {
    let mut map = SEEN.0.lock().unwrap();
    match map.insert(key, id.to_string()) {
        Some(previous) => previous != id,
        None => false, // first poll: populate silently, like the Swift pollers
    }
}

// ── Stripe ────────────────────────────────────────────────────────────────────

const STRIPE_HINT: &str = "Use a secret key (sk_live_… not pk_live_…)";

async fn poll_stripe(app: AppHandle) {
    let Some(key) = secrets::get("stripe-api-key") else { return };
    let basic = || http::secret(&format!("Basic {}", crate::claude::base64_for(format!("{key}:").as_bytes())));

    let Some(json) = fetch(&app, "integration_stripe", STRIPE_HINT, |c| {
        Ok(c.get("https://api.stripe.com/v1/balance").header(AUTHORIZATION, basic()?))
    })
    .await
    else {
        return;
    };
    let (amount, currency) = {
        let mut buckets: Vec<Value> = Vec::new();
        for k in ["available", "pending"] {
            if let Some(arr) = json.get(k).and_then(Value::as_array) {
                buckets.extend(arr.iter().cloned());
            }
        }
        let currency = buckets
            .first()
            .and_then(|b| b.get("currency"))
            .and_then(Value::as_str)
            .unwrap_or("eur")
            .to_string();
        let amount: i64 = buckets
            .iter()
            .filter_map(|b| b.get("amount").and_then(Value::as_i64))
            .sum();
        (amount, currency)
    };

    let Some(json) = fetch(&app, "integration_stripe", STRIPE_HINT, |c| {
        Ok(c.get("https://api.stripe.com/v1/charges?limit=3").header(AUTHORIZATION, basic()?))
    })
    .await
    else {
        return;
    };
    let payments: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    let description = c
                        .get("description")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            c.get("billing_details")
                                .and_then(|b| b.get("name"))
                                .and_then(Value::as_str)
                        })
                        .map(str::to_string);
                    Some(json!({
                        "id": c.get("id")?.as_str()?,
                        "amount": c.get("amount")?.as_i64()?,
                        "currency": c.get("currency")?.as_str()?,
                        "description": description,
                        "createdAt": c.get("created").and_then(Value::as_i64).unwrap_or(0) * 1000,
                        "status": c.get("status").and_then(Value::as_str).unwrap_or("succeeded"),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let newest = payments
        .first()
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let event = if !newest.is_empty() && is_new("stripe", &newest) {
        let label = payments[0]
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| {
                let cents = payments[0].get("amount").and_then(Value::as_i64).unwrap_or(0);
                format!("{:.2}", cents as f64 / 100.0)
            });
        Some(IntegrationEvent { success: true, label, detail: None })
    } else {
        None
    };

    emit(&app, IntegrationUpdate {
        id: "integration_stripe",
        data: json!({ "balance": amount, "currency": currency, "payments": payments }),
        error: None,
        event,
    });
}

// ── GitHub ────────────────────────────────────────────────────────────────────

async fn poll_github(app: AppHandle) {
    let Some(token) = secrets::get("github-token") else { return };
    let github = |c: &Client, url: &str| -> Result<RequestBuilder, ApiError> {
        Ok(c.get(url)
            .header(AUTHORIZATION, http::bearer(&token)?)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "Coucou"))
    };

    let Some(json) = fetch(&app, "integration_github", "Token lacks the needed scope", |c| {
        github(c, "https://api.github.com/user")
    })
    .await
    else {
        return;
    };
    let public = json.get("public_repos").and_then(Value::as_i64).unwrap_or(0);
    let private = json
        .get("owned_private_repos")
        .or_else(|| json.get("total_private_repos"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    // The star count is a bonus on top of the user call above: if it fails the card
    // still shows the repositories, with the reason in the log rather than a zero
    // that looks like an answer.
    let stars: i64 = match try_fetch(|c| {
        github(c, "https://api.github.com/user/repos?per_page=100&affiliation=owner&sort=pushed")
    })
    .await
    {
        Ok(v) => v
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|r| r.get("stargazers_count").and_then(Value::as_i64))
                    .sum()
            })
            .unwrap_or(0),
        Err(err) => {
            log::line(format!("integration_github: star count unavailable ({})", err.describe("")));
            0
        }
    };

    emit(&app, IntegrationUpdate {
        id: "integration_github",
        data: json!({ "totalRepos": public + private, "totalStars": stars }),
        error: None,
        event: None,
    });
}

// ── Vercel ────────────────────────────────────────────────────────────────────

async fn poll_vercel(app: AppHandle) {
    let Some(token) = secrets::get("vercel-token") else { return };
    // Vercel answers an invalid token with 403, not 401 (checked live), so the
    // 403 text has to cover both.
    let Some(json) = fetch(&app, "integration_vercel", "Token invalid or lacks access", |c| {
        Ok(c.get("https://api.vercel.com/v6/deployments?limit=5")
            .header(AUTHORIZATION, http::bearer(&token)?)
            .header("Accept", "application/json"))
    })
    .await
    else {
        return;
    };
    let terminal = ["READY", "ERROR", "CANCELED"];
    let deployments: Vec<Value> = json
        .get("deployments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|d| {
                    let state = d.get("state")?.as_str()?;
                    if !terminal.contains(&state) {
                        return None;
                    }
                    let meta = d.get("meta");
                    let pick = |keys: [&str; 3]| {
                        meta.and_then(|m| keys.iter().find_map(|k| m.get(*k).and_then(Value::as_str)))
                            .map(str::to_string)
                    };
                    Some(json!({
                        "id": d.get("uid")?.as_str()?,
                        "projectName": d.get("name")?.as_str()?,
                        "url": d.get("url").and_then(Value::as_str).unwrap_or(""),
                        "state": state,
                        "createdAt": d.get("createdAt").and_then(Value::as_f64).unwrap_or(0.0),
                        "commitMessage": pick(["githubCommitMessage", "gitlabCommitMessage", "bitbucketCommitMessage"]),
                        "branch": pick(["githubCommitRef", "gitlabCommitRef", "bitbucketBranch"]),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let event = deployments.first().and_then(|latest| {
        let id = latest.get("id")?.as_str()?;
        if !is_new("vercel", id) {
            return None;
        }
        let success = latest.get("state")?.as_str()? == "READY";
        Some(IntegrationEvent {
            success,
            label: latest.get("projectName")?.as_str()?.to_string(),
            detail: None,
        })
    });

    emit(&app, IntegrationUpdate {
        id: "integration_vercel",
        data: json!({ "deployments": deployments }),
        error: None,
        event,
    });
}

// ── Resend ────────────────────────────────────────────────────────────────────

async fn poll_resend(app: AppHandle) {
    let Some(key) = secrets::get("resend-api-key") else { return };
    // An invalid key is a 400 here ("API key is invalid", checked live); the
    // service's message is shown with it.
    let Some(json) = fetch(&app, "integration_resend", "Key lacks access", |c| {
        Ok(c.get("https://api.resend.com/emails?limit=100")
            .header(AUTHORIZATION, http::bearer(&key)?)
            .header("Accept", "application/json"))
    })
    .await
    else {
        return;
    };
    let total = json
        .get("total")
        .or_else(|| json.get("count"))
        .and_then(Value::as_i64);
    let emails: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(5)
                .filter_map(|e| {
                    let to = match e.get("to") {
                        Some(Value::Array(a)) => a.clone(),
                        Some(Value::String(s)) => vec![Value::String(s.clone())],
                        _ => vec![],
                    };
                    Some(json!({
                        "id": e.get("id")?.as_str()?,
                        "to": to,
                        "subject": e.get("subject").and_then(Value::as_str).unwrap_or(""),
                        "createdAt": e.get("created_at").and_then(Value::as_str).unwrap_or(""),
                        "lastEvent": e.get("last_event").and_then(Value::as_str).unwrap_or(""),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_resend",
        data: json!({ "emails": emails, "total": total }),
        error: None,
        event: None,
    });
}

// ── Notion ────────────────────────────────────────────────────────────────────

async fn poll_notion(app: AppHandle) {
    let Some(token) = secrets::get("notion-api-key") else { return };
    let Some(json) = fetch(&app, "integration_notion", "Integration lacks access", |c| {
        Ok(c.post("https://api.notion.com/v1/search")
            .header(AUTHORIZATION, http::bearer(&token)?)
            .header("Notion-Version", "2022-06-28")
            .json(&json!({
                "sort": { "direction": "descending", "timestamp": "last_edited_time" },
                "page_size": 3
            })))
    })
    .await
    else {
        return;
    };
    let pages: Vec<Value> = json
        .get("results")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_notion_page).collect())
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_notion",
        data: json!({ "pages": pages }),
        error: None,
        event: None,
    });
}

fn parse_notion_page(obj: &Value) -> Option<Value> {
    let id = obj.get("id")?.as_str()?;
    let is_database = obj.get("object").and_then(Value::as_str) == Some("database");

    let mut title = "Untitled".to_string();
    if is_database {
        if let Some(text) = obj
            .get("title")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|t| t.get("plain_text"))
            .and_then(Value::as_str)
        {
            if !text.is_empty() {
                title = text.to_string();
            }
        }
    } else if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for prop in props.values() {
            if prop.get("type").and_then(Value::as_str) != Some("title") {
                continue;
            }
            if let Some(text) = prop
                .get("title")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|t| t.get("plain_text"))
                .and_then(Value::as_str)
            {
                if !text.is_empty() {
                    title = text.to_string();
                    break;
                }
            }
        }
    }

    let emoji = obj
        .get("icon")
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("emoji"))
        .and_then(|i| i.get("emoji"))
        .and_then(Value::as_str);

    Some(json!({
        "id": id,
        "title": title,
        "emoji": emoji,
        "lastEditedAt": obj.get("last_edited_time").and_then(Value::as_str)?,
        "url": obj.get("url").and_then(Value::as_str).unwrap_or("https://notion.so"),
    }))
}

// ── Cal.com ───────────────────────────────────────────────────────────────────

async fn poll_calcom(app: AppHandle) {
    let Some(key) = secrets::get("calcom-api-key") else { return };
    // Cal.com answers an invalid key with 403, not 401 (checked live).
    let Some(json) = fetch(&app, "integration_calcom", "Key invalid or lacks access", |c| {
        Ok(c.get("https://api.cal.com/v2/bookings?status=upcoming")
            .header(AUTHORIZATION, http::bearer(&key)?)
            .header("cal-api-version", "2024-08-13"))
    })
    .await
    else {
        return;
    };
    let bookings: Vec<Value> = json
        .get("data")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|b| {
                    let start = b
                        .get("start")
                        .or_else(|| b.get("startTime"))
                        .and_then(Value::as_str)?;
                    let attendee = b.get("attendees").and_then(Value::as_array).and_then(|a| a.first());
                    let notes = b
                        .get("responses")
                        .and_then(|r| r.get("notes"))
                        .and_then(|n| n.get("value"))
                        .and_then(Value::as_str)
                        .or_else(|| b.get("description").and_then(Value::as_str))
                        .filter(|s| !s.is_empty());
                    Some(json!({
                        "id": b.get("id").map(|v| v.to_string()).unwrap_or_default(),
                        "title": b.get("title").and_then(Value::as_str).unwrap_or("Meeting"),
                        "start": start,
                        "status": b.get("status").and_then(Value::as_str).unwrap_or("accepted"),
                        "attendeeName": attendee.and_then(|a| a.get("name")).and_then(Value::as_str),
                        "attendeeEmail": attendee.and_then(|a| a.get("email")).and_then(Value::as_str),
                        "attendeeNotes": notes,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    emit(&app, IntegrationUpdate {
        id: "integration_calcom",
        data: json!({ "bookings": bookings }),
        error: None,
        event: None,
    });
}

// ── n8n ───────────────────────────────────────────────────────────────────────

async fn poll_n8n(app: AppHandle) {
    let (Some(key), Some(raw_base)) = (secrets::get("n8n-api-key"), secrets::get("n8n-url")) else {
        return;
    };
    let base = raw_base.trim_end_matches('/').to_string();
    let n8n = |c: &Client, url: &str| -> Result<RequestBuilder, ApiError> {
        Ok(c.get(url)
            .header("X-N8N-API-KEY", http::secret(&key)?)
            .header("Accept", "application/json"))
    };

    // Same two shapes as the Swift poller: the public API first, then /rest.
    let list_urls = [
        format!("{base}/api/v1/executions?limit=1&includeData=false"),
        format!("{base}/rest/executions?limit=1&includeData=false"),
    ];

    let mut items: Option<Vec<Value>> = None;
    let mut last_error: Option<ApiError> = None;
    for url in &list_urls {
        match try_fetch(|c| n8n(c, url)).await {
            Ok(json) => {
                items = match &json {
                    Value::Object(o) => o.get("data").and_then(Value::as_array).cloned(),
                    Value::Array(a) => Some(a.clone()),
                    _ => None,
                };
                if items.is_some() {
                    break;
                }
            }
            Err(err) => {
                // The code and kind only: a self-hosted base URL can carry credentials.
                log::line(format!("n8n list: {}", err.describe("Access denied")));
                last_error = Some(err);
            }
        }
    }

    let Some(first) = items.and_then(|list| list.into_iter().next()) else {
        // Both shapes failed: say so, instead of leaving the card as it was.
        if let Some(err) = last_error {
            fail(&app, "integration_n8n", &err, "API key lacks access");
        }
        return;
    };
    let id = match first.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return,
    };

    let status = first.get("status").and_then(Value::as_str).unwrap_or("");
    if !["success", "error", "crashed", "canceled", "failed"].contains(&status) {
        return;
    }
    if !is_new("n8n", &id) {
        return;
    }
    let success = status == "success";

    let detail_urls = [
        format!("{base}/api/v1/executions/{id}?includeData=true"),
        format!("{base}/api/v1/executions/{id}"),
        format!("{base}/rest/executions/{id}?includeData=true"),
        format!("{base}/rest/executions/{id}"),
    ];
    let mut name = "Workflow".to_string();
    let mut detail = None;
    for url in &detail_urls {
        // A failure here only costs the detail: the next shape is tried, and the
        // event still fires with the generic name. (A response too large to hold,
        // which includeData=true can produce, falls through to the lighter URL.)
        let Ok(json) = try_fetch(|c| n8n(c, url)).await else { continue };
        name = json
            .get("workflowData")
            .and_then(|w| w.get("name"))
            .and_then(Value::as_str)
            .or_else(|| json.get("name").and_then(Value::as_str))
            .unwrap_or("Workflow")
            .to_string();
        detail = n8n_detail(&json, success);
        break;
    }

    log::line(format!("n8n execution {id} {status} · {name}"));
    emit(&app, IntegrationUpdate {
        id: "integration_n8n",
        data: json!({ "workflow": name, "status": status }),
        error: None,
        event: Some(IntegrationEvent { success, label: name, detail }),
    });
}

fn n8n_detail(json: &Value, success: bool) -> Option<String> {
    let result = json.get("data")?.get("resultData")?;
    if !success {
        if let Some(error) = result.get("error") {
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            if let Some(node) = error.get("node").and_then(|n| n.get("name")).and_then(Value::as_str) {
                if !node.is_empty() {
                    return Some(format!("{node}\n{message}"));
                }
            }
            return Some(message.to_string());
        }
        let runs = result.get("runData")?.as_object()?;
        for (node, value) in runs {
            if let Some(message) = value
                .as_array()
                .and_then(|a| a.first())
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
            {
                return Some(format!("{node}\n{message}"));
            }
        }
        return None;
    }

    let last_node = result.get("lastNodeExecuted")?.as_str()?;
    let items = result
        .get("runData")?
        .get(last_node)?
        .as_array()?
        .first()?
        .get("data")?
        .get("main")?
        .as_array()?
        .first()?
        .as_array()?;
    let count = items.len();
    let header = format!("→ {last_node} · {count} item{}", if count == 1 { "" } else { "s" });

    let fields = items
        .first()
        .and_then(|i| i.get("json"))
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .take(4)
                .map(|(k, v)| format!("{k}: {}", fmt_value(v)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|s| !s.is_empty());

    Some(match fields {
        Some(f) => format!("{header}\n{f}"),
        None => header,
    })
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.chars().take(50).collect(),
        Value::Array(a) => format!("[{}]", a.len()),
        Value::Object(_) => "{…}".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_poll_per_integration_at_a_time() {
        let mut gate = Gate::default();
        let now = Instant::now();
        assert_eq!(gate.enter("integration_stripe", now), Entry::Go);
        // A Refresh pressed while a poll is running does not start a second one.
        assert_eq!(gate.enter("integration_stripe", now), Entry::Busy);
        assert_eq!(gate.enter("integration_stripe", now), Entry::Busy);
        // Another integration is independent.
        assert_eq!(gate.enter("integration_github", now), Entry::Go);
        gate.leave("integration_stripe");
        assert_eq!(gate.enter("integration_stripe", now), Entry::Go);
    }

    #[test]
    fn a_rate_limited_integration_is_left_alone_for_the_time_it_was_told() {
        let mut gate = Gate::default();
        let t0 = Instant::now();
        gate.cool_down("integration_vercel", Some(Duration::from_secs(90)), t0);
        assert_eq!(gate.enter("integration_vercel", t0 + Duration::from_secs(30)), Entry::Cooling(Duration::from_secs(60)));
        // Others are unaffected, and once the time has passed it polls again.
        assert_eq!(gate.enter("integration_github", t0), Entry::Go);
        assert_eq!(gate.enter("integration_vercel", t0 + Duration::from_secs(91)), Entry::Go);
    }

    #[test]
    fn the_cooldown_has_a_default_and_a_ceiling() {
        let mut gate = Gate::default();
        let t0 = Instant::now();
        gate.cool_down("a", None, t0);
        assert_eq!(gate.enter("a", t0), Entry::Cooling(DEFAULT_COOLDOWN));
        // A service cannot park an integration for hours.
        gate.cool_down("b", Some(Duration::from_secs(86_400)), t0);
        assert_eq!(gate.enter("b", t0), Entry::Cooling(MAX_COOLDOWN));
    }

    #[test]
    fn failures_are_worded_for_the_island_and_never_contain_a_key() {
        let key = "sk_live_SECRETKEY";
        let cases = [
            (ApiError::Status { code: 401, message: Some(format!("Invalid API Key provided: {key}")), retry_after: None }, "Invalid API key (401)"),
            (ApiError::Status { code: 429, message: None, retry_after: Some(Duration::from_secs(30)) }, "Rate limited (429) — try again in 30 s"),
            (ApiError::Status { code: 502, message: None, retry_after: None }, "Service unavailable (502)"),
            (ApiError::Timeout, "Timed out"),
            (ApiError::Connect, "No connection"),
            (ApiError::Malformed, "Unexpected response"),
            (ApiError::TooLarge, "Response too large"),
        ];
        for (err, expected) in cases {
            let shown = err.describe(STRIPE_HINT);
            assert_eq!(shown, expected);
            assert!(!shown.contains(key));
        }
    }

    /// An n8n URL that is not a URL is reported as such, not as "no connection".
    #[test]
    fn a_malformed_n8n_url_is_a_clear_error() {
        crate::http::tests::run(async {
            let err = try_fetch(|c| Ok(c.get("not a url at all"))).await.unwrap_err();
            assert_eq!(err.describe(""), "Request rejected (400) — invalid URL");
            assert!(!err.is_transient());
        });
    }
}

