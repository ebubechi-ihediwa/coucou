// Relay server for coucou-hook.
//
// Windows: the named pipe `\\.\pipe\coucou-<sid>`, one instance per connection.
// Linux: the Unix socket `$XDG_RUNTIME_DIR/coucou.sock`. Every hook event is
// forwarded to the island as a `hook` event. `PermissionRequest` is the only one
// that keeps its connection open: it waits for the island's decision and writes
// it back on the same connection, which is how approving from the island works.
//
// Claude Code is never blocked by us. Three things guarantee it:
//   * coucou-hook gives the connection 300 ms and exits cleanly if we are closed;
//   * we only wait for a human once the island has *confirmed* the card is on
//     screen, so a paused island or a webview that is not listening costs a few
//     hundred milliseconds, not two minutes;
//   * whatever happens we drop the connection after the decision timeout, and
//     the terminal takes over.
//
// What we write back is the bare word `allow` or `deny`. Turning that into the
// documented hookSpecificOutput JSON is coucou-hook's job, so the wire format
// Claude Code expects lives in exactly one place.

use std::collections::HashMap;
#[cfg(any(windows, test))]
use std::future::Future;
#[cfg(any(windows, test))]
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::island::WINDOW_LABEL;
use crate::log;

/// Slightly under coucou-hook's own 110 s wait, so we always answer first.
const DECISION_TIMEOUT: Duration = Duration::from_secs(108);
/// How long the island gets to say "the card is up". This is the whole of B4:
/// without it, an island that is paused, hidden behind a crashed webview or
/// simply not listening would leave Claude Code staring at a prompt nobody can
/// see for nearly two minutes.
const ACK_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_PAYLOAD: usize = 1 << 20;

/// What the island can say about a permission request.
pub enum Reply {
    /// The card is on screen and a human can act on it.
    Ack,
    /// A human clicked: `allow` or `deny`.
    Decision(String),
    /// Nobody can act on it — paused, or another request already holds the card.
    Decline,
}

/// Permission requests the island has been told about.
#[derive(Default)]
pub struct Pending(pub Mutex<HashMap<String, mpsc::Sender<Reply>>>);

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// `\\.\pipe\coucou-<sid>` — must match coucou-hook's `connect()` exactly.
#[cfg(windows)]
fn pipe_name(sid: &str) -> String {
    format!(r"\\.\pipe\coucou-{sid}")
}

#[cfg(windows)]
pub fn start(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // The SID names the pipe and is the only principal its DACL admits. Without
        // it neither can be built, and coucou-hook refuses to connect without it
        // too, so there is nothing to fall back to.
        let Some(sid) = crate::platform::current_user_sid() else {
            log::line("cannot read the user SID — the relay pipe is not opened, hooks are inactive");
            return;
        };
        let instances = match RelayPipe::new(pipe_name(&sid), &sid) {
            Ok(instances) => instances,
            Err(err) => {
                log::line(format!("cannot build the relay pipe's access rules: {err} — hooks are inactive"));
                return;
            }
        };
        // `serve` has already logged why it stopped.
        let _ = serve(instances, RELAY_BACKOFF, CONTESTED_ATTEMPTS, move |connected| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move { handle(app, connected).await });
        })
        .await;
    });
}

/// How the accept loop talks to the pipe. The real one is `RelayPipe`; the tests
/// use a fake so the recovery logic runs without a pipe, and on any OS.
#[cfg(any(windows, test))]
trait PipeInstances: Send + Sync + 'static {
    type Pipe: Send + Sync + 'static;
    /// Opens the first listening instance. It must fail if the name is already
    /// taken, so we never serve on top of a pipe somebody else owns.
    fn create_first(&self) -> io::Result<Self::Pipe>;
    /// Opens one more listening instance of the relay pipe.
    fn create(&self) -> io::Result<Self::Pipe>;
    /// Resolves once a client has connected to `pipe`.
    fn connect(pipe: &Self::Pipe) -> impl Future<Output = io::Result<()>> + Send;
    fn log(&self, message: String);
}

#[cfg(windows)]
struct RelayPipe {
    name: String,
    /// Admits only our own user. Every instance, first or not, is made with it.
    acl: crate::pipe_acl::OwnerOnly,
}

#[cfg(windows)]
impl RelayPipe {
    fn new(name: String, sid: &str) -> io::Result<Self> {
        Ok(RelayPipe { name, acl: crate::pipe_acl::OwnerOnly::for_sid(sid)? })
    }
}

#[cfg(windows)]
impl PipeInstances for RelayPipe {
    type Pipe = NamedPipeServer;

    fn create_first(&self) -> io::Result<NamedPipeServer> {
        // first_pipe_instance also means we refuse to join a pipe somebody else
        // already owns under our name, rather than serving on top of it.
        self.acl.create(ServerOptions::new().first_pipe_instance(true), &self.name)
    }

    fn create(&self) -> io::Result<NamedPipeServer> {
        // Not first_pipe_instance: while clients are being served, instances of
        // the name already exist. The first one is the guard; the DACL, which
        // lets nobody but our user create or open an instance, protects the rest.
        self.acl.create(&ServerOptions::new(), &self.name)
    }

    fn connect(pipe: &NamedPipeServer) -> impl Future<Output = io::Result<()>> + Send {
        pipe.connect()
    }

    fn log(&self, message: String) {
        log::line(message);
    }
}

/// Pause between attempts to open a pipe instance that failed: it doubles up to
/// `max`, so a lasting failure costs one attempt and one log line every few
/// seconds instead of a spinning task. Without a listening instance clients get
/// "pipe not found", coucou-hook exits silently and the terminal takes over, so
/// waiting is fail-closed, never an approval.
#[cfg(any(windows, test))]
#[derive(Clone, Copy)]
struct Backoff {
    initial: Duration,
    max: Duration,
}

#[cfg(any(windows, test))]
const RELAY_BACKOFF: Backoff = Backoff {
    initial: Duration::from_millis(100),
    max: Duration::from_secs(5),
};

/// A connect that fails on a live instance is retried at this pace.
#[cfg(any(windows, test))]
const CONNECT_RETRY: Duration = Duration::from_millis(200);

/// How many times the name may be refused (see `Failure::Contested`) before the
/// first instance gives up: about 20 s with `RELAY_BACKOFF`, long enough for a
/// previous Coucou that is still exiting to let go of it.
#[cfg(any(windows, test))]
const CONTESTED_ATTEMPTS: u32 = 10;

#[cfg(any(windows, test))]
struct Retry {
    backoff: Backoff,
    delay: Duration,
    failures: u32,
    /// "opened" or "reopened", for the log line that ends a run of failures.
    done: &'static str,
}

#[cfg(any(windows, test))]
impl Retry {
    fn new(backoff: Backoff, done: &'static str) -> Self {
        Retry { backoff, delay: backoff.initial, failures: 0, done }
    }

    fn succeeded<I: PipeInstances>(&mut self, instances: &I) {
        if self.failures > 0 {
            instances.log(format!(
                "relay pipe {} after {} failed attempt(s)",
                self.done, self.failures
            ));
        }
        self.failures = 0;
        self.delay = self.backoff.initial;
    }

    async fn failed<I: PipeInstances>(&mut self, instances: &I, err: io::Error) {
        self.failures += 1;
        instances.log(format!(
            "cannot open a relay pipe instance (attempt {}, retrying in {} ms): {err}",
            self.failures,
            self.delay.as_millis()
        ));
        tokio::time::sleep(self.delay).await;
        self.delay = (self.delay * 2).min(self.backoff.max);
    }
}

/// What a failure to open the *first* instance means for the next attempt.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq)]
enum Failure {
    /// Resources, a busy system: waiting may fix it. Retried without limit, at
    /// the backoff's slowest pace.
    Transient,
    /// The name is refused (access denied, already exists). Either a previous
    /// Coucou has not finished exiting, or another process holds the name. The
    /// first clears by itself, the second never will, so this is retried only for
    /// a while.
    Contested,
    /// The request itself is wrong (bad name, invalid parameter): retrying
    /// changes nothing.
    Permanent,
}

/// Windows errors reach us as `io::ErrorKind` (ERROR_ACCESS_DENIED is
/// `PermissionDenied`, ERROR_ALREADY_EXISTS `AlreadyExists`, ERROR_INVALID_PARAMETER
/// `InvalidInput`, and so on), which keeps this testable on any OS. Anything not
/// listed is treated as transient: one log line every few seconds is cheap, and
/// the alternative is approvals gone for the whole session.
#[cfg(any(windows, test))]
fn classify(err: &io::Error) -> Failure {
    use io::ErrorKind::*;
    match err.kind() {
        InvalidInput | InvalidData | InvalidFilename | NotFound | Unsupported => Failure::Permanent,
        PermissionDenied | AlreadyExists => Failure::Contested,
        _ => Failure::Transient,
    }
}

/// Opens the first instance, retrying what can plausibly clear up. Returns the
/// error that ended it when it cannot be opened. Every wait is an await point, so
/// aborting the task cancels it at once, and nothing is created between attempts.
#[cfg(any(windows, test))]
async fn open_first<I: PipeInstances>(
    instances: &I,
    backoff: Backoff,
    contested_attempts: u32,
) -> io::Result<I::Pipe> {
    let mut retry = Retry::new(backoff, "opened");
    let mut refused = 0;
    loop {
        let err = match instances.create_first() {
            Ok(pipe) => {
                retry.succeeded(instances);
                return Ok(pipe);
            }
            Err(err) => err,
        };
        match classify(&err) {
            Failure::Permanent => {
                instances.log(format!(
                    "cannot open the relay pipe: {err} — approvals from the island are unavailable, Claude Code asks in the terminal"
                ));
                return Err(err);
            }
            Failure::Contested => {
                refused += 1;
                if refused >= contested_attempts {
                    instances.log(format!(
                        "another process holds the relay pipe name ({err}); gave up after {refused} attempts — approvals from the island are unavailable, Claude Code asks in the terminal"
                    ));
                    return Err(err);
                }
            }
            Failure::Transient => {}
        }
        retry.failed(instances, err).await;
    }
}

/// The relay: open the first instance (with recovery), then serve for as long as
/// the task lives. Errs only when the first instance can never be opened, after
/// logging why.
#[cfg(any(windows, test))]
async fn serve<I: PipeInstances>(
    instances: I,
    backoff: Backoff,
    contested_attempts: u32,
    on_connected: impl Fn(I::Pipe),
) -> io::Result<()> {
    let first = open_first(&instances, backoff, contested_attempts).await?;
    accept_loop(instances, first, backoff, on_connected).await;
    Ok(())
}

/// Serves the pipe for as long as the task lives: wait for a client, hand the
/// connected instance to `on_connected`, listen on a fresh one.
///
/// A fresh instance that cannot be opened no longer ends the loop (it used to,
/// and approvals stopped for the rest of the session with one log line to show
/// for it). The client already connected is still served, then the loop retries
/// with `Retry`'s backoff. There is no shutdown flag because the app has none:
/// the task ends with the process, and aborting it is clean at every await.
#[cfg(any(windows, test))]
async fn accept_loop<I: PipeInstances>(
    instances: I,
    first: I::Pipe,
    backoff: Backoff,
    on_connected: impl Fn(I::Pipe),
) {
    let mut listening = Some(first);
    let mut retry = Retry::new(backoff, "reopened");
    loop {
        let server = match listening.take() {
            Some(server) => server,
            None => {
                match instances.create() {
                    Ok(server) => {
                        retry.succeeded(&instances);
                        listening = Some(server);
                    }
                    Err(err) => retry.failed(&instances, err).await,
                }
                continue;
            }
        };
        if I::connect(&server).await.is_err() {
            listening = Some(server);
            tokio::time::sleep(CONNECT_RETRY).await;
            continue;
        }
        match instances.create() {
            Ok(next) => {
                retry.succeeded(&instances);
                listening = Some(next);
                on_connected(server);
            }
            Err(err) => {
                on_connected(server);
                retry.failed(&instances, err).await;
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub fn start(app: AppHandle) {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    tauri::async_runtime::spawn(async move {
        let Some(path) = crate::platform::relay_socket_path() else {
            log::line("no private runtime directory ($XDG_RUNTIME_DIR) — Claude Code hooks are inactive");
            return;
        };
        // A socket file left behind by a crash answers nothing and can go. One
        // that answers belongs to a Coucou that is still running: like
        // first_pipe_instance on Windows, we refuse to serve on top of it.
        if path.exists() {
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                log::line("another Coucou already serves the relay socket");
                return;
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(err) => {
                log::line(format!("cannot open the relay socket: {err}"));
                return;
            }
        };
        // The runtime directory is already 0700; this is belt and braces.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        let uid = unsafe { libc::getuid() };
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            // Only the relay run by our own user may drive the island.
            if !matches!(stream.peer_cred(), Ok(c) if c.uid() == uid) {
                log::line("refused a relay connection from another user");
                continue;
            }
            let app = app.clone();
            tauri::async_runtime::spawn(async move { handle(app, stream).await });
        }
    });
}

/// One accepted relay connection, whatever carries it.
trait Relay: AsyncRead + AsyncWrite + Unpin {
    /// Ends the conversation once everything has been written.
    fn finish(&mut self) {}
}

#[cfg(windows)]
impl Relay for NamedPipeServer {
    fn finish(&mut self) {
        let _ = self.disconnect();
    }
}

/// Dropping the stream closes it; the relay reads up to our newline first.
#[cfg(target_os = "linux")]
impl Relay for tokio::net::UnixStream {}

async fn handle(app: AppHandle, mut pipe: impl Relay) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() > MAX_PAYLOAD {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let line = match buf.iter().position(|b| *b == b'\n') {
        Some(i) => &buf[..i],
        None => &buf[..],
    };
    let Ok(mut payload) = serde_json::from_slice::<Value>(line) else { return };
    if !payload.is_object() {
        return;
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if event != "PermissionRequest" {
        log::line(format!("hook {event}"));
        let _ = app.emit_to(WINDOW_LABEL, "hook", payload);
        pipe.finish();
        return;
    }

    let id = format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
    let (tx, mut rx) = mpsc::channel::<Reply>(4);
    {
        let pending = app.state::<Pending>();
        pending.0.lock().unwrap().insert(id.clone(), tx);
    }
    payload["request_id"] = json!(id);
    log::line(format!("hook PermissionRequest id={id}"));
    let _ = app.emit_to(WINDOW_LABEL, "hook", payload);

    let decision = wait_for_decision(&id, &mut rx).await;
    app.state::<Pending>().0.lock().unwrap().remove(&id);

    // No decision: say nothing at all. coucou-hook then writes nothing to stdout
    // and Claude Code asks in the terminal, exactly as if Coucou were closed.
    if let Some(d) = decision {
        let _ = pipe.write_all(format!("{d}\n").as_bytes()).await;
        let _ = pipe.flush().await;
    }
    pipe.finish();
}

/// Two waits: a short one for "the card is up", then the long one for a human.
async fn wait_for_decision(id: &str, rx: &mut mpsc::Receiver<Reply>) -> Option<String> {
    match tokio::time::timeout(ACK_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Ack)) => {}
        // A click that beats the ack is still a click.
        Ok(Some(Reply::Decision(d))) => {
            log::line(format!("hook id={id} answered {d}"));
            return Some(d);
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={id} not shown — terminal takes over"));
            return None;
        }
        Ok(None) => return None,
        Err(_) => {
            log::line(format!("hook id={id} island never acknowledged — terminal takes over"));
            return None;
        }
    }

    match tokio::time::timeout(DECISION_TIMEOUT, rx.recv()).await {
        Ok(Some(Reply::Decision(d))) => {
            log::line(format!("hook id={id} answered {d}"));
            Some(d)
        }
        Ok(Some(Reply::Decline)) => {
            log::line(format!("hook id={id} released without a decision"));
            None
        }
        _ => {
            log::line(format!("hook id={id} timed out — terminal takes over"));
            None
        }
    }
}

fn send(app: &AppHandle, request_id: &str, reply: Reply, keep: bool) {
    let sender = {
        let pending = app.state::<Pending>();
        let mut map = pending.0.lock().unwrap();
        if keep { map.get(request_id).cloned() } else { map.remove(request_id) }
    };
    match sender {
        Some(tx) => {
            let _ = tx.try_send(reply);
        }
        None => log::line(format!("reply for id={request_id} — no pending request")),
    }
}

/// The island has the card on screen; the long wait may begin.
pub fn acknowledge(app: &AppHandle, request_id: &str) {
    send(app, request_id, Reply::Ack, true);
}

/// Nobody can act on this one — paused, or another card already holds the view.
pub fn decline(app: &AppHandle, request_id: &str) {
    log::line(format!("decline id={request_id}"));
    send(app, request_id, Reply::Decline, false);
}

/// Called by the island's Allow / Deny buttons. Only ever a bare word: turning
/// it into Claude Code's JSON is coucou-hook's job.
pub fn answer(app: &AppHandle, request_id: &str, decision: &str) {
    let word = match decision {
        "allow" | "always" => "allow",
        _ => "deny",
    };
    log::line(format!("decision id={request_id} {word}"));
    send(app, request_id, Reply::Decision(word.to_string()), false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;
    use tokio::sync::{mpsc::unbounded_channel, Notify};

    const FAST: Backoff = Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(80),
    };

    fn run<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A runtime whose clock only moves when everything is waiting on it, so
    /// backoff tests are exact and take no real time. Never for tests that do real
    /// I/O: they would see the clock race ahead of the OS.
    fn run_paused<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(f)
    }

    struct FakePipe {
        id: usize,
        client: Notify,
    }

    #[derive(Default)]
    struct State {
        /// Outcomes of the next `create` calls; `fail_by_default` applies after.
        script: Mutex<VecDeque<bool>>,
        fail_by_default: AtomicBool,
        attempts: AtomicUsize,
        /// How many of those were for the first instance (`create_first`).
        first_attempts: AtomicUsize,
        /// When each attempt happened, on the runtime's clock (virtual when paused).
        attempt_times: Mutex<Vec<tokio::time::Instant>>,
        /// The kind of error a failed attempt reports; `Other` if unset.
        error_kind: Mutex<Option<io::ErrorKind>>,
        pipes: Mutex<Vec<Arc<FakePipe>>>,
        logs: Mutex<Vec<String>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<State>);

    impl Fake {
        fn next(&self) -> io::Result<Arc<FakePipe>> {
            let s = &self.0;
            s.attempts.fetch_add(1, Ordering::SeqCst);
            s.attempt_times.lock().unwrap().push(tokio::time::Instant::now());
            let ok = match s.script.lock().unwrap().pop_front() {
                Some(ok) => ok,
                None => !s.fail_by_default.load(Ordering::SeqCst),
            };
            if !ok {
                let kind = s.error_kind.lock().unwrap().unwrap_or(io::ErrorKind::Other);
                return Err(io::Error::new(kind, "simulated"));
            }
            let mut pipes = s.pipes.lock().unwrap();
            let pipe = Arc::new(FakePipe { id: pipes.len(), client: Notify::new() });
            pipes.push(pipe.clone());
            Ok(pipe)
        }
    }

    impl PipeInstances for Fake {
        type Pipe = Arc<FakePipe>;

        fn create_first(&self) -> io::Result<Arc<FakePipe>> {
            self.0.first_attempts.fetch_add(1, Ordering::SeqCst);
            self.next()
        }

        fn create(&self) -> io::Result<Arc<FakePipe>> {
            self.next()
        }

        fn connect(pipe: &Arc<FakePipe>) -> impl Future<Output = io::Result<()>> + Send {
            let pipe = pipe.clone();
            async move {
                pipe.client.notified().await;
                Ok(())
            }
        }

        fn log(&self, message: String) {
            self.0.logs.lock().unwrap().push(message);
        }
    }

    impl Fake {
        fn attempts(&self) -> usize {
            self.0.attempts.load(Ordering::SeqCst)
        }

        fn first_attempts(&self) -> usize {
            self.0.first_attempts.load(Ordering::SeqCst)
        }

        /// A client connects to pipe instance `id`, once the loop has opened it.
        async fn client(&self, id: usize) {
            for _ in 0..500 {
                if let Some(p) = self.0.pipes.lock().unwrap().get(id).cloned() {
                    p.client.notify_one();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("pipe instance {id} was never opened");
        }
    }

    async fn within<T>(f: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), f).await.expect("timed out")
    }

    #[test]
    fn the_loop_survives_a_failed_instance_and_serves_the_next_client() {
        run(async {
            let fake = Fake::default();
            let first = fake.create().unwrap();
            // The reopen after the first client fails once, then works.
            fake.0.script.lock().unwrap().push_back(false);
            let (tx, mut served) = unbounded_channel();
            let task = tokio::spawn(accept_loop(fake.clone(), first, FAST, move |p| {
                let _ = tx.send(p.id);
            }));

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0), "the client in hand is still served");
            fake.client(1).await;
            assert_eq!(within(served.recv()).await, Some(1), "a client connects after recovery");

            let logs = fake.0.logs.lock().unwrap().clone();
            assert!(logs.iter().any(|l| l.contains("cannot open a relay pipe instance")), "{logs:?}");
            assert!(logs.iter().any(|l| l.contains("reopened after 1")), "{logs:?}");
            task.abort();
        });
    }

    #[test]
    fn repeated_failures_back_off_instead_of_spinning_then_recover() {
        run(async {
            let fake = Fake::default();
            let first = fake.create().unwrap();
            fake.0.fail_by_default.store(true, Ordering::SeqCst);
            let (tx, mut served) = unbounded_channel();
            let task = tokio::spawn(accept_loop(fake.clone(), first, FAST, move |p| {
                let _ = tx.send(p.id);
            }));

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0));
            tokio::time::sleep(Duration::from_millis(400)).await;
            // 10, 20, 40, 80, 80... ms apart: about eight attempts in 400 ms.
            // A busy loop would make thousands.
            let tries = fake.attempts() - 1;
            assert!((2..=20).contains(&tries), "{tries} attempts in 400 ms");

            fake.0.fail_by_default.store(false, Ordering::SeqCst);
            fake.client(1).await;
            assert_eq!(within(served.recv()).await, Some(1), "recovers once creation works");
            task.abort();
        });
    }

    #[test]
    fn aborting_the_loop_ends_it_promptly_while_backing_off() {
        run(async {
            let fake = Fake::default();
            let first = fake.create().unwrap();
            fake.0.fail_by_default.store(true, Ordering::SeqCst);
            // A backoff far longer than the test: only the abort can end it.
            let slow = Backoff { initial: Duration::from_secs(60), max: Duration::from_secs(60) };
            let task = tokio::spawn(accept_loop(fake.clone(), first, slow, |_| {}));
            fake.client(0).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            task.abort();
            assert!(within(task).await.unwrap_err().is_cancelled());
        });
    }

    #[test]
    fn aborting_the_loop_releases_the_pipe_while_waiting_for_a_client() {
        run(async {
            let fake = Fake::default();
            let first = fake.create().unwrap();
            let task = tokio::spawn(accept_loop(fake.clone(), first, FAST, |_| {}));
            tokio::time::sleep(Duration::from_millis(50)).await;
            task.abort();
            assert!(within(task).await.unwrap_err().is_cancelled());
            let pipe = fake.0.pipes.lock().unwrap()[0].clone();
            // Ours and the fake's own list: the loop's copy is gone.
            assert_eq!(Arc::strong_count(&pipe), 2);
        });
    }

    // ── Startup: the first instance ───────────────────────────────────────────

    /// Runs `serve` on the fake and returns the task plus the channel of served pipes.
    fn start_serving(
        fake: &Fake,
        backoff: Backoff,
        contested_attempts: u32,
    ) -> (tokio::task::JoinHandle<io::Result<()>>, tokio::sync::mpsc::UnboundedReceiver<usize>) {
        let (tx, served) = unbounded_channel();
        let task = tokio::spawn(serve(fake.clone(), backoff, contested_attempts, move |p: Arc<FakePipe>| {
            let _ = tx.send(p.id);
        }));
        (task, served)
    }

    #[test]
    fn startup_recovers_after_one_transient_failure() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.script.lock().unwrap().push_back(false);
            let (task, mut served) = start_serving(&fake, FAST, 3);

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0), "serves once the first instance opens");
            assert_eq!(fake.first_attempts(), 2, "one failure, one success");
            let logs = fake.0.logs.lock().unwrap().clone();
            assert!(logs.iter().any(|l| l.contains("relay pipe opened after 1 failed")), "{logs:?}");
            task.abort();
        });
    }

    #[test]
    fn startup_recovers_after_several_transient_failures() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.script.lock().unwrap().extend([false, false, false, false]);
            let (task, mut served) = start_serving(&fake, FAST, 3);

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0));
            assert_eq!(fake.first_attempts(), 5, "four failures, then the one that works");
            task.abort();
        });
    }

    #[test]
    fn startup_retries_follow_the_backoff_and_stop_doubling_at_the_cap() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.script.lock().unwrap().extend([false; 6]);
            let (task, mut served) = start_serving(&fake, FAST, 3);

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0));
            let times = fake.0.attempt_times.lock().unwrap()[..fake.first_attempts()].to_vec();
            let gaps: Vec<u128> = times.windows(2).map(|w| (w[1] - w[0]).as_millis()).collect();
            // FAST doubles 10 → 80 ms and stays there. The timer rounds up to the
            // millisecond, so a gap may exceed its delay by a hair, never fall short.
            let expected = [10u128, 20, 40, 80, 80, 80];
            assert_eq!(gaps.len(), expected.len(), "{gaps:?}");
            for (gap, want) in gaps.iter().zip(expected) {
                assert!((want..=want + 2).contains(gap), "gaps {gaps:?}, wanted {expected:?}");
            }
            task.abort();
        });
    }

    #[test]
    fn a_permanent_startup_failure_stops_at_once_and_says_so() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.fail_by_default.store(true, Ordering::SeqCst);
            *fake.0.error_kind.lock().unwrap() = Some(io::ErrorKind::InvalidInput);
            let (task, _served) = start_serving(&fake, FAST, 3);

            let err = within(task).await.unwrap().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(fake.attempts(), 1, "a permanent error is not retried");
            let logs = fake.0.logs.lock().unwrap().clone();
            assert!(logs.iter().any(|l| l.contains("cannot open the relay pipe") && l.contains("terminal")), "{logs:?}");
        });
    }

    #[test]
    fn a_name_that_stays_refused_is_given_up_after_the_bounded_attempts() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.fail_by_default.store(true, Ordering::SeqCst);
            *fake.0.error_kind.lock().unwrap() = Some(io::ErrorKind::PermissionDenied);
            let (task, _served) = start_serving(&fake, FAST, 4);

            let err = within(task).await.unwrap().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(fake.attempts(), 4);
            let logs = fake.0.logs.lock().unwrap().clone();
            assert!(logs.iter().any(|l| l.contains("another process holds the relay pipe name")), "{logs:?}");
        });
    }

    #[test]
    fn a_name_that_frees_up_while_refused_is_taken() {
        run_paused(async {
            // The previous Coucou was still exiting: refused twice, then free.
            let fake = Fake::default();
            fake.0.script.lock().unwrap().extend([false, false]);
            *fake.0.error_kind.lock().unwrap() = Some(io::ErrorKind::PermissionDenied);
            let (task, mut served) = start_serving(&fake, FAST, 4);

            fake.client(0).await;
            assert_eq!(within(served.recv()).await, Some(0));
            task.abort();
        });
    }

    #[test]
    fn aborting_during_startup_retries_ends_promptly_and_stops_retrying() {
        run_paused(async {
            let fake = Fake::default();
            fake.0.fail_by_default.store(true, Ordering::SeqCst);
            // A wait far longer than the test: only the abort can end it.
            let slow = Backoff { initial: Duration::from_secs(3600), max: Duration::from_secs(3600) };
            let (task, _served) = start_serving(&fake, slow, 3);
            while fake.attempts() == 0 {
                tokio::task::yield_now().await;
            }
            task.abort();
            assert!(within(task).await.unwrap_err().is_cancelled());

            // Two virtual hours pass; nothing tries again.
            tokio::time::sleep(Duration::from_secs(7200)).await;
            assert_eq!(fake.attempts(), 1);
        });
    }

    #[test]
    fn errors_are_sorted_by_whether_retrying_can_help() {
        use io::ErrorKind::*;
        for kind in [InvalidInput, InvalidData, InvalidFilename, NotFound, Unsupported] {
            assert_eq!(classify(&io::Error::from(kind)), Failure::Permanent, "{kind:?}");
        }
        for kind in [PermissionDenied, AlreadyExists] {
            assert_eq!(classify(&io::Error::from(kind)), Failure::Contested, "{kind:?}");
        }
        for kind in [Other, OutOfMemory, TimedOut, Interrupted, WouldBlock] {
            assert_eq!(classify(&io::Error::from(kind)), Failure::Transient, "{kind:?}");
        }
    }

    // ── The real pipe (Windows) ───────────────────────────────────────────────

    #[cfg(windows)]
    fn unique_name(tag: &str) -> String {
        use std::sync::atomic::AtomicU32;
        static N: AtomicU32 = AtomicU32::new(0);
        format!(r"\\.\pipe\coucou-test-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    #[cfg(windows)]
    fn my_sid() -> String {
        crate::platform::current_user_sid().expect("the test process has a user SID")
    }

    /// The SDDL of the DACL on a live pipe instance, read back from the kernel.
    #[cfg(windows)]
    fn dacl_of(pipe: &NamedPipeServer) -> String {
        use std::os::windows::io::AsRawHandle;
        use windows::core::PWSTR;
        use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
            SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

        // SAFETY: the handle is live (borrowed from `pipe`); both out-pointers are
        // valid; the descriptor and the string are LocalFree'd exactly once.
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            let status = GetSecurityInfo(
                HANDLE(pipe.as_raw_handle()),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            );
            assert_eq!(status.0, 0, "GetSecurityInfo failed: {status:?}");
            let mut text = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                None,
            )
            .unwrap();
            let sddl = text.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            sddl
        }
    }

    /// Two clients in a row over a real named pipe made by the real `RelayPipe`
    /// (so with its DACL), through `serve`: the legitimate client of the legitimate
    /// user connects, talks both ways, goes away, and the next one connects too.
    #[cfg(windows)]
    #[test]
    fn a_real_named_pipe_serves_clients_one_after_another() {
        use tokio::net::windows::named_pipe::ClientOptions;

        run(async {
            let name = unique_name("serve");
            let (tx, mut served) = unbounded_channel::<NamedPipeServer>();
            let task = tokio::spawn(serve(
                RelayPipe::new(name.clone(), &my_sid()).unwrap(),
                FAST,
                CONTESTED_ATTEMPTS,
                move |p| {
                    let _ = tx.send(p);
                },
            ));
            for _ in 0..2 {
                // The first instance is made by the task; wait for it to exist.
                let mut client = loop {
                    match ClientOptions::new().open(&name) {
                        Ok(c) => break c,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => tokio::task::yield_now().await,
                        Err(e) => panic!("a legitimate client was refused: {e}"),
                    }
                };
                client.write_all(b"hello\n").await.unwrap();
                let mut server = within(served.recv()).await.unwrap();
                let mut buf = [0u8; 6];
                within(server.read_exact(&mut buf)).await.unwrap();
                assert_eq!(&buf, b"hello\n");
                // And the answer travels back, as an approval does.
                server.write_all(b"allow\n").await.unwrap();
                let mut answer = [0u8; 6];
                within(client.read_exact(&mut answer)).await.unwrap();
                assert_eq!(&answer, b"allow\n");
                server.disconnect().unwrap();
            }
            task.abort();
        });
    }

    /// The descriptor really is enforced: a pipe that admits some other SID refuses
    /// us, even though we made it and run as the user that connects.
    #[cfg(windows)]
    #[test]
    fn a_pipe_that_admits_another_sid_refuses_our_own_client() {
        use tokio::net::windows::named_pipe::ClientOptions;

        run(async {
            let name = unique_name("foreign");
            // S-1-5-19 is LOCAL SERVICE: a real principal that is not this user.
            let pipe = RelayPipe::new(name.clone(), "S-1-5-19").unwrap();
            let _first = pipe.create_first().unwrap();
            let err = ClientOptions::new().open(&name).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        });
    }

    /// What SDDL writes for a descriptor made from `sddl`. Windows prints some
    /// accounts by alias instead of by SID (`LA` for the local Administrator, `BA`
    /// for the Administrators group, …), so a DACL read back from a pipe cannot be
    /// compared with a SID string; it has to be compared with this.
    #[cfg(windows)]
    fn canonical_sddl(sddl: &str) -> String {
        use windows::core::{PCWSTR, PWSTR};
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW,
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

        let wide: Vec<u16> = format!("{sddl}\0").encode_utf16().collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the descriptor and
        // the string are LocalFree'd exactly once.
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .unwrap();
            let mut text = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                None,
            )
            .unwrap();
            let out = text.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(text.0.cast())));
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            out
        }
    }

    /// First and later instances carry the same single-ACE DACL naming only us.
    ///
    /// The expectation is built from our SID in the same canonical form the pipe's
    /// DACL is read back in. Searching for the SID string instead failed on GitHub's
    /// Windows runner, whose user is the built-in Administrator (RID 500): SDDL
    /// prints that account as `LA`, so the long SID is nowhere in the text.
    #[cfg(windows)]
    #[test]
    fn every_instance_admits_only_our_sid() {
        run(async {
            let pipe = RelayPipe::new(unique_name("dacl"), &my_sid()).unwrap();
            let first = pipe.create_first().unwrap();
            let later = pipe.create().unwrap();
            // Protected, one allow entry, full file access, to this SID and no one else.
            let expected = canonical_sddl(&format!("D:P(A;;FA;;;{})", my_sid()));
            for (which, instance) in [("first", &first), ("later", &later)] {
                assert_eq!(dacl_of(instance), expected, "{which} instance");
            }
        });
    }

    /// The mechanism behind that CI failure, reproduced with an account that is
    /// printed by alias on every machine: the Administrators group.
    #[cfg(windows)]
    #[test]
    fn a_dacl_names_well_known_accounts_by_alias_so_it_is_compared_canonically() {
        run(async {
            let administrators = "S-1-5-32-544";
            let pipe = RelayPipe::new(unique_name("alias"), administrators).unwrap();
            let instance = pipe.create_first().unwrap();
            let sddl = dacl_of(&instance);
            assert!(!sddl.contains(administrators), "printed by alias, not by SID: {sddl}");
            assert!(sddl.contains(";;;BA)"), "{sddl}");
            assert_eq!(sddl, canonical_sddl(&format!("D:P(A;;FA;;;{administrators})")));
        });
    }

    /// Somebody already holding the name makes the first instance fail, and with
    /// the kind `open_first` treats as "refused", never as success.
    #[cfg(windows)]
    #[test]
    fn the_first_instance_refuses_a_name_that_is_already_taken() {
        run(async {
            let name = unique_name("taken");
            let holder = RelayPipe::new(name.clone(), &my_sid()).unwrap();
            let _held = holder.create_first().unwrap();
            let err = RelayPipe::new(name, &my_sid()).unwrap().create_first().unwrap_err();
            assert_eq!(classify(&err), Failure::Contested, "{err}");
        });
    }
}
