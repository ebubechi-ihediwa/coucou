// A whole chat turn, screen awareness included, against a model and a screen that are
// scripted: no network, no display. What is looked at is what the model was sent, what
// the conversation kept, what was captured and how often, and what is left behind when
// the turn fails or is stopped.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use super::{
    base64, send_with, Backend, BoxFuture, Chat, ModelReply, Stage, KEPT, NOT_RUN_YET,
    NO_ARGUMENTS, NO_VISION, SCREEN_OFF, SECOND_LOOK, TOO_BIG,
};
use crate::assistant::ToolResult;
use crate::executor::Files;
use crate::http::tests::run;
use crate::screen::{ScreenError, Screenshot};

const MODEL: &str = "claude-opus-5";
/// Stands in for the screenshot's bytes. If it turns up anywhere it should not, a test fails.
const PIXELS: &[u8] = b"SECRET-PIXELS-NOT-A-REAL-JPEG";

fn shot() -> Result<Screenshot, ScreenError> {
    Ok(Screenshot::fake(100, 50, PIXELS.to_vec()))
}

fn said(text: &str) -> Result<Value, String> {
    Ok(json!({ "content": [{ "type": "text", "text": text }], "stop_reason": "end_turn" }))
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

fn asks_to_look(id: &str) -> Result<Value, String> {
    Ok(json!({
        "content": [{ "type": "text", "text": "One moment." }, tool_use(id, "capture_screen", json!({}))],
        "stop_reason": "tool_use",
    }))
}

fn replying(content: Vec<Value>) -> Result<Value, String> {
    Ok(json!({ "content": content, "stop_reason": "tool_use" }))
}

/// A model and a screen that do what the test says, and write down what they were asked.
struct Fake {
    replies: Mutex<VecDeque<Result<Value, String>>>,
    shots: Mutex<VecDeque<Result<Screenshot, ScreenError>>>,
    bodies: Mutex<Vec<Value>>,
    looks: AtomicUsize,
    cancels: Mutex<Vec<Arc<AtomicBool>>>,
    /// The capture never finishes, as when it is stopped part-way.
    hangs: bool,
}

impl Fake {
    fn new(
        replies: Vec<Result<Value, String>>,
        shots: Vec<Result<Screenshot, ScreenError>>,
    ) -> Fake {
        Fake {
            replies: Mutex::new(replies.into()),
            shots: Mutex::new(shots.into()),
            bodies: Mutex::new(Vec::new()),
            looks: AtomicUsize::new(0),
            cancels: Mutex::new(Vec::new()),
            hangs: false,
        }
    }

    fn looks(&self) -> usize {
        self.looks.load(Ordering::SeqCst)
    }

    fn asks(&self) -> usize {
        self.bodies.lock().unwrap().len()
    }

    fn body(&self, n: usize) -> Value {
        self.bodies.lock().unwrap()[n].clone()
    }
}

impl Backend for Fake {
    fn ask(&self, body: Value) -> BoxFuture<'_, Result<Value, String>> {
        self.bodies.lock().unwrap().push(body);
        let next = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the model was asked more often than the test expected");
        Box::pin(async move { next })
    }

    fn look(&self, cancel: Arc<AtomicBool>) -> BoxFuture<'_, Result<Screenshot, ScreenError>> {
        self.looks.fetch_add(1, Ordering::SeqCst);
        self.cancels.lock().unwrap().push(cancel);
        if self.hangs {
            return Box::pin(std::future::pending());
        }
        let next = self
            .shots
            .lock()
            .unwrap()
            .pop_front()
            .expect("the screen was captured more often than the test expected");
        Box::pin(async move { next })
    }
}

fn files() -> Files {
    // Only used for a dropped file, which none of these turns has.
    Files::new(std::env::temp_dir().join("coucou-turn-tests-unused"))
}

/// One turn, and the stages the island was told about.
fn turn_with(
    fake: &Fake,
    chat: &Chat,
    model: &str,
    screen_on: bool,
    query: &str,
) -> (Result<ModelReply, String>, Vec<Stage>) {
    let stages = Mutex::new(Vec::new());
    let progress = |stage| stages.lock().unwrap().push(stage);
    let result = run(send_with(
        fake,
        chat,
        model,
        query.to_string(),
        None,
        &files(),
        screen_on,
        &progress,
    ));
    (result, stages.into_inner().unwrap())
}

fn turn(fake: &Fake, chat: &Chat, query: &str) -> (Result<ModelReply, String>, Vec<Stage>) {
    turn_with(fake, chat, MODEL, true, query)
}

fn text_of(result: Result<ModelReply, String>) -> String {
    match result {
        Ok(reply) => reply.text,
        Err(e) => panic!("the turn failed: {e}"),
    }
}

fn failure_of(result: Result<ModelReply, String>) -> String {
    match result {
        Err(e) => e,
        Ok(_) => panic!("the turn should have failed"),
    }
}

fn has_image(value: &Value) -> bool {
    value.to_string().contains(r#""type":"image""#)
}

/// The answer to one tool call, as the request that carried it said it.
fn answer_to(body: &Value, id: &str) -> Value {
    let messages = body["messages"].as_array().unwrap();
    messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
        .unwrap_or_else(|| panic!("no answer to {id}"))
        .clone()
}

// ── Asking for a look ────────────────────────────────────────────────────────

#[test]
fn a_question_that_needs_no_screen_takes_no_screenshot() {
    let fake = Fake::new(vec![said("Paris.")], vec![]);
    let chat = Chat::default();
    let (result, stages) = turn(&fake, &chat, "What's the capital of France?");
    assert_eq!(text_of(result), "Paris.");
    assert_eq!(fake.looks(), 0);
    assert_eq!(fake.asks(), 1);
    assert!(stages.is_empty(), "nothing to say about the screen");
    assert!(!has_image(&fake.body(0)));
    assert_eq!(chat.snapshot().len(), 2);
}

#[test]
fn a_screen_request_takes_one_screenshot_and_sends_it_with_the_next_request() {
    let fake = Fake::new(
        vec![
            asks_to_look("c1"),
            said("That is an error about a missing file."),
        ],
        vec![shot()],
    );
    let chat = Chat::default();
    let (result, stages) = turn(&fake, &chat, "what does this error mean?");
    assert_eq!(text_of(result), "That is an error about a missing file.");

    assert_eq!(fake.looks(), 1);
    assert_eq!(fake.asks(), 2);
    assert!(
        !has_image(&fake.body(0)),
        "the first request has no picture"
    );
    let answer = answer_to(&fake.body(1), "c1");
    let image = &answer["content"][0];
    assert_eq!(image["type"], "image");
    assert_eq!(image["source"]["type"], "base64");
    assert_eq!(image["source"]["media_type"], "image/jpeg");
    assert_eq!(image["source"]["data"], base64(PIXELS));
    assert!(answer.get("is_error").is_none());
    assert_eq!(stages, [Stage::Capturing, Stage::Thinking]);
}

#[test]
fn both_requests_offer_the_same_tools_and_the_second_carries_the_first_reply() {
    let fake = Fake::new(vec![asks_to_look("c1"), said("Done.")], vec![shot()]);
    let chat = Chat::default();
    let _ = turn(&fake, &chat, "look at this");
    for n in 0..2 {
        assert_eq!(fake.body(n)["tools"].as_array().unwrap().len(), 3);
    }
    let messages = fake.body(1)["messages"].as_array().unwrap().clone();
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user"],
        "the model's call, then its answer"
    );
    assert_eq!(messages[1]["content"][1]["name"], "capture_screen");
}

#[test]
fn the_conversation_keeps_a_note_in_place_of_the_screenshot() {
    let fake = Fake::new(
        vec![asks_to_look("c1"), said("It is a terminal."), said("Yes.")],
        vec![shot()],
    );
    let chat = Chat::default();
    let _ = turn(&fake, &chat, "what am I looking at?");

    let kept = chat.snapshot();
    assert_eq!(kept.len(), 4, "question, the call, its answer, the reply");
    let everything = Value::Array(kept.clone()).to_string();
    assert!(
        !everything.contains(&base64(PIXELS)),
        "no picture in the history"
    );
    assert!(!everything.contains("SECRET-PIXELS"));
    assert!(!has_image(&Value::Array(kept.clone())));
    assert_eq!(kept[2]["content"][0]["content"], KEPT);

    // A later turn does not send it again, and does not take another.
    let (later, _) = turn(&fake, &chat, "and is it open?");
    assert_eq!(text_of(later), "Yes.");
    assert_eq!(fake.looks(), 1);
    assert!(!has_image(&fake.body(2)));
}

#[test]
fn each_turn_may_look_once_so_a_second_request_to_look_is_a_second_turn() {
    let fake = Fake::new(
        vec![
            asks_to_look("c1"),
            said("First."),
            asks_to_look("c2"),
            said("Second."),
        ],
        vec![shot(), shot()],
    );
    let chat = Chat::default();
    assert_eq!(text_of(turn(&fake, &chat, "look").0), "First.");
    assert_eq!(text_of(turn(&fake, &chat, "look again").0), "Second.");
    assert_eq!(fake.looks(), 2);
}

// ── Not more than once ───────────────────────────────────────────────────────

#[test]
fn two_calls_in_one_reply_take_one_screenshot_and_both_are_answered() {
    let both = replying(vec![
        tool_use("c1", "capture_screen", json!({})),
        tool_use("c2", "capture_screen", json!({})),
    ]);
    let fake = Fake::new(vec![both, said("Fine.")], vec![shot()]);
    let chat = Chat::default();
    assert_eq!(text_of(turn(&fake, &chat, "look").0), "Fine.");
    assert_eq!(fake.looks(), 1);
    assert_eq!(
        answer_to(&fake.body(1), "c1")["content"][0]["type"],
        "image"
    );
    let second = answer_to(&fake.body(1), "c2");
    assert_eq!(second["content"], SECOND_LOOK);
    assert_eq!(second["is_error"], true);
    assert!(!has_image(&second));
}

#[test]
fn asking_again_after_the_look_gets_no_second_screenshot() {
    let again = replying(vec![
        json!({ "type": "text", "text": "Let me check once more." }),
        tool_use("c2", "capture_screen", json!({})),
    ]);
    let fake = Fake::new(
        vec![asks_to_look("c1"), again, said("Later.")],
        vec![shot()],
    );
    let chat = Chat::default();
    let (result, _) = turn(&fake, &chat, "look");
    assert_eq!(text_of(result), "Let me check once more.");
    assert_eq!(
        fake.looks(),
        1,
        "never a third request, never a second picture"
    );
    assert_eq!(fake.asks(), 2);

    // The call still has to be answered, in the next message.
    assert_eq!(
        chat.take_tool_results(),
        [ToolResult {
            tool_use_id: "c2".into(),
            text: SECOND_LOOK.into()
        }]
    );
}

#[test]
fn another_tool_in_the_same_reply_is_not_run_and_a_screenshot_grants_nothing() {
    let first = replying(vec![
        tool_use(
            "a",
            "propose_action",
            json!({ "type": "open_app", "app": "notepad" }),
        ),
        tool_use("c", "capture_screen", json!({})),
    ]);
    let fake = Fake::new(
        vec![first, said("I can see it, but I did not open anything.")],
        vec![shot()],
    );
    let chat = Chat::default();
    let (result, _) = turn(&fake, &chat, "look at this and open notepad");
    let reply = result.expect("the turn works");
    assert!(
        reply.proposal.is_none(),
        "the action in the first reply was not proposed"
    );
    assert_eq!(answer_to(&fake.body(1), "a")["content"], NOT_RUN_YET);
    assert_eq!(answer_to(&fake.body(1), "c")["content"][0]["type"], "image");
}

#[test]
fn an_action_asked_for_after_the_look_is_still_only_a_proposal_to_approve() {
    let then_act = replying(vec![
        json!({ "type": "text", "text": "I see Notepad is not open. Shall I open it?" }),
        tool_use(
            "p",
            "propose_action",
            json!({ "type": "open_app", "app": "notepad" }),
        ),
    ]);
    let fake = Fake::new(vec![asks_to_look("c1"), then_act], vec![shot()]);
    let chat = Chat::default();
    let (result, _) = turn(&fake, &chat, "look and open notepad if it is closed");
    let reply = result.expect("the turn works");
    // What comes back is an untrusted proposal. Nothing is done here; the runtime asks the person.
    let proposal = reply.proposal.expect("a proposal");
    assert_eq!(proposal.tool_use_id, "p");
    assert_eq!(
        proposal.input,
        json!({ "type": "open_app", "app": "notepad" })
    );
}

// ── When there is no screenshot to take ──────────────────────────────────────

#[test]
fn with_screen_awareness_off_nothing_is_captured_and_the_model_is_told() {
    let fake = Fake::new(vec![asks_to_look("c1"), said("It is off.")], vec![]);
    let chat = Chat::default();
    let (result, stages) = turn_with(&fake, &chat, MODEL, false, "look at this");
    assert_eq!(text_of(result), "It is off.");
    assert_eq!(fake.looks(), 0);
    let answer = answer_to(&fake.body(1), "c1");
    assert_eq!(answer["content"], SCREEN_OFF);
    assert_eq!(answer["is_error"], true);
    assert!(stages.is_empty(), "never said it was looking");
}

#[test]
fn a_model_that_cannot_read_images_is_never_sent_one_and_nothing_is_swapped() {
    let fake = Fake::new(vec![asks_to_look("c1"), said("I can't.")], vec![]);
    let chat = Chat::default();
    let (result, _) = turn_with(&fake, &chat, "claude-instant-1.2", true, "look at this");
    assert_eq!(text_of(result), "I can't.");
    assert_eq!(fake.looks(), 0);
    assert_eq!(answer_to(&fake.body(1), "c1")["content"], NO_VISION);
    // The model asked of is the model chosen, both times.
    assert!(
        fake.body(0)["model"] == "claude-instant-1.2"
            && fake.body(1)["model"] == "claude-instant-1.2"
    );
}

#[test]
fn a_call_with_arguments_is_refused_before_anything_is_captured() {
    let sneaky = replying(vec![tool_use(
        "c1",
        "capture_screen",
        json!({ "path": "C:\\Users\\me\\secret.txt", "display_id": 2 }),
    )]);
    let fake = Fake::new(vec![sneaky, said("No.")], vec![]);
    let chat = Chat::default();
    let _ = turn(&fake, &chat, "look");
    assert_eq!(fake.looks(), 0);
    assert_eq!(answer_to(&fake.body(1), "c1")["content"], NO_ARGUMENTS);
}

#[test]
fn a_failed_capture_is_told_to_the_model_by_kind_and_the_turn_goes_on() {
    for (error, words) in [
        (ScreenError::Unavailable, "can't be captured"),
        (ScreenError::PermissionDenied, "did not allow"),
        (ScreenError::CaptureFailed, "could not be captured"),
        (ScreenError::TooLarge, "too large"),
        (ScreenError::EncodingFailed, "could not be prepared"),
        (ScreenError::Cancelled, "cancelled"),
    ] {
        let fake = Fake::new(vec![asks_to_look("c1"), said("Sorry.")], vec![Err(error)]);
        let chat = Chat::default();
        let (result, stages) = turn(&fake, &chat, "look");
        assert_eq!(text_of(result), "Sorry.");
        let answer = answer_to(&fake.body(1), "c1");
        assert_eq!(answer["is_error"], true);
        let shown = answer["content"].as_str().unwrap();
        assert!(shown.contains(words), "{shown}");
        assert!(!has_image(&fake.body(1)));
        assert_eq!(
            stages,
            [Stage::Capturing, Stage::Thinking],
            "the island is not left saying it is looking"
        );
    }
}

#[test]
fn a_screenshot_too_big_for_the_request_is_left_out_and_the_model_is_told() {
    let huge = Screenshot::fake(1568, 882, vec![7u8; 9 * 1024 * 1024]);
    let fake = Fake::new(
        vec![asks_to_look("c1"), said("It was too big.")],
        vec![Ok(huge)],
    );
    let chat = Chat::default();
    let (result, _) = turn(&fake, &chat, "look");
    assert_eq!(text_of(result), "It was too big.");
    let body = fake.body(1);
    assert!(!has_image(&body));
    assert!(
        body.to_string().len() < 1024 * 1024,
        "the request that was sent is small"
    );
    let answer = answer_to(&body, "c1");
    assert_eq!(answer["content"], TOO_BIG);
    assert_eq!(answer["is_error"], true);
}

// ── Failing and stopping ─────────────────────────────────────────────────────

#[test]
fn a_failure_after_the_screenshot_leaves_no_trace_of_it_in_the_conversation() {
    let fake = Fake::new(
        vec![
            asks_to_look("c1"),
            Err("No connection. Check your network and try again.".into()),
        ],
        vec![shot()],
    );
    let chat = Chat::default();
    chat.add_tool_result(ToolResult {
        tool_use_id: "earlier".into(),
        text: "done".into(),
    });
    let (result, _) = turn(&fake, &chat, "look");
    assert_eq!(
        failure_of(result),
        "No connection. Check your network and try again."
    );
    assert!(
        chat.is_empty(),
        "the question is dropped, as for any failed turn"
    );
    assert_eq!(
        chat.take_tool_results().len(),
        1,
        "and the answer it carried is owed again"
    );

    // The next message works.
    let fake = Fake::new(vec![said("Back.")], vec![]);
    assert_eq!(text_of(turn(&fake, &chat, "hello").0), "Back.");
}

#[test]
fn stopping_during_the_capture_stops_the_capture_and_leaves_the_conversation_as_it_was() {
    let mut fake = Fake::new(vec![asks_to_look("c1")], vec![]);
    fake.hangs = true;
    let chat = Chat::default();
    chat.add_tool_result(ToolResult {
        tool_use_id: "earlier".into(),
        text: "done".into(),
    });
    run(async {
        let progress = |_: Stage| {};
        let inbox = files();
        let turn = send_with(
            &fake,
            &chat,
            MODEL,
            "look".into(),
            None,
            &inbox,
            true,
            &progress,
        );
        // The person presses Stop: the future is dropped while the capture is under way.
        let outcome = tokio::time::timeout(Duration::from_millis(100), turn).await;
        assert!(outcome.is_err(), "the capture was still going");
    });
    assert_eq!(fake.looks(), 1);
    assert!(
        fake.cancels.lock().unwrap()[0].load(Ordering::SeqCst),
        "the capture was told to stop"
    );
    assert_eq!(
        fake.asks(),
        1,
        "no request was made without the picture or with a partial one"
    );
    assert!(chat.is_empty());
    assert_eq!(chat.take_tool_results().len(), 1);

    let fake = Fake::new(vec![said("Ready.")], vec![]);
    assert_eq!(text_of(turn(&fake, &chat, "hello").0), "Ready.");
}

#[test]
fn a_capture_that_finishes_is_not_told_to_stop() {
    let fake = Fake::new(vec![asks_to_look("c1"), said("Done.")], vec![shot()]);
    let chat = Chat::default();
    let _ = turn(&fake, &chat, "look");
    // The flag is only a signal for work still under way; it is set once the turn is over.
    assert_eq!(fake.cancels.lock().unwrap().len(), 1);
}

// ── What does not leak ───────────────────────────────────────────────────────

#[test]
fn nothing_from_the_picture_reaches_the_log_an_error_or_the_debug_output() {
    let fake = Fake::new(vec![asks_to_look("c1"), said("Done.")], vec![shot()]);
    let chat = Chat::default();
    let _ = turn(&fake, &chat, "look");

    let log = std::fs::read_to_string(crate::settings::local_dir().join("coucou.log"))
        .unwrap_or_default();
    assert!(
        !log.contains("SECRET-PIXELS") && !log.contains(&base64(PIXELS)),
        "the log holds the picture"
    );

    let shown = format!("{:?}", Screenshot::fake(100, 50, PIXELS.to_vec()));
    assert!(
        !shown.contains("SECRET") && shown.contains("100x50"),
        "{shown}"
    );

    for error in [
        ScreenError::Unavailable,
        ScreenError::PermissionDenied,
        ScreenError::CaptureFailed,
        ScreenError::TooLarge,
        ScreenError::EncodingFailed,
        ScreenError::Cancelled,
    ] {
        let fake = Fake::new(vec![asks_to_look("c1"), said("Sorry.")], vec![Err(error)]);
        let _ = turn(&fake, &Chat::default(), "look");
        let told = answer_to(&fake.body(1), "c1")["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !told.contains('\\') && !told.contains(":/"),
            "no path in {told}"
        );
    }
}

// ── Through the assistant's runtime, as `chat_send` drives it ────────────────

mod through_the_runtime {
    use super::*;
    use crate::actions::PolicyConfig;
    use crate::assistant::{self, Phase, Runtime, Settled};
    use crate::executor::Launcher;
    use std::io;
    use std::path::Path;

    /// Records what the system was asked to open; opens nothing.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Recorder {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Launcher for &'static Recorder {
        fn launch_app(&self, app: crate::actions::AppId) -> io::Result<()> {
            self.0.lock().unwrap().push(format!("app:{}", app.id()));
            Ok(())
        }
        fn open_url(&self, url: &str) -> io::Result<()> {
            self.0.lock().unwrap().push(format!("url:{url}"));
            Ok(())
        }
        fn open_path(&self, path: &Path) -> io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("path:{}", path.display()));
            Ok(())
        }
    }

    fn runtime() -> (Runtime<&'static Recorder>, &'static Recorder) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let recorder: &'static Recorder = Box::leak(Box::default());
        let root = std::env::temp_dir().join(format!(
            "coucou-screen-turn-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        (
            Runtime::new(recorder, Files::new(root), PolicyConfig::default()),
            recorder,
        )
    }

    /// One request as `chat_send` runs it: begin, say each stage, ask, settle. Also the
    /// phase the island would have been shown at each stage.
    fn drive(
        rt: &Runtime<&'static Recorder>,
        chat: &Chat,
        fake: &Fake,
        query: &str,
    ) -> (Result<Settled, String>, Vec<Phase>) {
        let (token, superseded) = rt.begin_turn().expect("the runtime is idle");
        assistant::settle(chat, superseded);
        let phases = Mutex::new(Vec::new());
        let progress = |stage| {
            rt.set_stage(token, stage);
            phases.lock().unwrap().push(rt.snapshot().phase);
        };
        let sent = run(send_with(
            fake,
            chat,
            MODEL,
            query.to_string(),
            None,
            rt.files(),
            true,
            &progress,
        ));
        let settled = match sent {
            Err(message) => {
                rt.fail_turn(token, message.clone());
                Err(message)
            }
            Ok(reply) => Ok(rt
                .on_reply(token, reply.text, reply.proposal)
                .ok()
                .expect("the turn was not stale")),
        };
        (settled, phases.into_inner().unwrap())
    }

    fn words(settled: Result<Settled, String>) -> String {
        match settled {
            Ok(Settled::Reply { text }) => text,
            other => panic!("expected a plain reply, got {other:?}"),
        }
    }

    #[test]
    fn a_typed_question_that_needs_no_screen_never_shows_the_looking_state() {
        let (rt, _) = runtime();
        let chat = Chat::default();
        let fake = Fake::new(vec![said("4.")], vec![]);
        let (settled, phases) = drive(&rt, &chat, &fake, "2 + 2");
        assert_eq!(words(settled), "4.");
        assert!(phases.is_empty());
        assert_eq!(fake.looks(), 0);
        assert_eq!(rt.snapshot().phase, Phase::Idle);
    }

    #[test]
    fn a_screen_request_goes_capturing_then_thinking_then_idle_with_one_capture() {
        let (rt, _) = runtime();
        let chat = Chat::default();
        let fake = Fake::new(
            vec![
                asks_to_look("c1"),
                said("A code editor with a red squiggle."),
            ],
            vec![shot()],
        );
        let (settled, phases) = drive(&rt, &chat, &fake, "what's on my screen?");
        assert_eq!(words(settled), "A code editor with a red squiggle.");
        assert_eq!(phases, [Phase::Capturing, Phase::Thinking]);
        assert_eq!(fake.looks(), 1);
        assert_eq!(
            rt.snapshot(),
            assistant::Snapshot {
                phase: Phase::Idle,
                proposal: None,
                message: None
            }
        );
    }

    #[test]
    fn a_spoken_request_is_the_same_request_and_is_not_captured_just_for_being_spoken() {
        // What was said into the microphone arrives as text and goes through the very same
        // turn. Whether the screen is needed is the model's call, once the words are read.
        let (rt, _) = runtime();
        let chat = Chat::default();
        let fake = Fake::new(
            vec![
                said("Paris."),
                asks_to_look("c1"),
                said("A missing semicolon."),
            ],
            vec![shot()],
        );

        let (settled, phases) = drive(&rt, &chat, &fake, "What's the capital of France?");
        assert_eq!(words(settled), "Paris.");
        assert!(phases.is_empty());
        assert_eq!(
            fake.looks(),
            0,
            "a question with no screen in it takes no screenshot"
        );

        let (settled, phases) = drive(
            &rt,
            &chat,
            &fake,
            "Look at this error and tell me what's wrong.",
        );
        assert_eq!(words(settled), "A missing semicolon.");
        assert_eq!(phases, [Phase::Capturing, Phase::Thinking]);
        assert_eq!(fake.looks(), 1);
    }

    #[test]
    fn looking_at_the_screen_does_not_approve_an_action_the_person_has_not_approved() {
        let then_act = replying(vec![
            json!({ "type": "text", "text": "Notepad is not open. I can open it." }),
            tool_use(
                "p",
                "propose_action",
                json!({ "type": "open_app", "app": "notepad" }),
            ),
        ]);
        let (rt, launcher) = runtime();
        let chat = Chat::default();
        let fake = Fake::new(vec![asks_to_look("c1"), then_act], vec![shot()]);
        let (settled, _) = drive(
            &rt,
            &chat,
            &fake,
            "look at my screen and open notepad if it's closed",
        );

        let id = match settled {
            Ok(Settled::Proposed { proposal, .. }) => proposal.id,
            other => panic!("expected a proposal to approve, got {other:?}"),
        };
        assert_eq!(rt.snapshot().phase, Phase::AwaitingApproval);
        assert!(launcher.calls().is_empty(), "the screenshot opened nothing");
        assert_eq!(fake.looks(), 1);

        // Only the person's click starts it.
        let approved = rt.approve(id).expect("approval");
        let _ = rt.execute(&approved);
        assert_eq!(launcher.calls(), ["app:notepad"]);
    }

    #[test]
    fn a_model_that_keeps_asking_to_look_still_gets_one_screenshot_per_request() {
        let (rt, _) = runtime();
        let chat = Chat::default();
        let greedy = replying(vec![
            json!({ "type": "text", "text": "Again." }),
            tool_use("c2", "capture_screen", json!({})),
            tool_use("c3", "capture_screen", json!({})),
        ]);
        let fake = Fake::new(
            vec![asks_to_look("c1"), greedy],
            vec![shot(), shot(), shot()],
        );
        let (settled, _) = drive(&rt, &chat, &fake, "look");
        assert_eq!(words(settled), "Again.");
        assert_eq!(fake.looks(), 1);
        assert_eq!(fake.asks(), 2, "and no third request");
    }

    #[test]
    fn a_failed_capture_does_not_fail_the_request() {
        let (rt, _) = runtime();
        let chat = Chat::default();
        let fake = Fake::new(
            vec![asks_to_look("c1"), said("I couldn't see your screen.")],
            vec![Err(ScreenError::PermissionDenied)],
        );
        let (settled, phases) = drive(&rt, &chat, &fake, "look");
        assert_eq!(words(settled), "I couldn't see your screen.");
        assert_eq!(phases, [Phase::Capturing, Phase::Thinking]);
        assert_eq!(rt.snapshot().phase, Phase::Idle);
    }

    #[test]
    fn stop_during_the_capture_ends_it_drops_the_picture_and_the_next_message_works() {
        let (rt, _) = runtime();
        let chat = Chat::default();
        let mut hanging = Fake::new(vec![asks_to_look("c1")], vec![]);
        hanging.hangs = true;

        let (token, _) = rt.begin_turn().expect("idle");
        run(async {
            let progress = |stage| {
                rt.set_stage(token, stage);
            };
            let turn = send_with(
                &hanging,
                &chat,
                MODEL,
                "look".into(),
                None,
                rt.files(),
                true,
                &progress,
            );
            // The island is showing "Looking at your screen…" when Stop is pressed.
            let outcome = tokio::time::timeout(Duration::from_millis(100), turn).await;
            assert!(outcome.is_err());
            assert_eq!(rt.snapshot().phase, Phase::Capturing);
        });
        // `chat_send` ends the task, which drops the turn; the runtime records the cancel.
        assert_eq!(rt.cancel().snapshot.phase, Phase::Cancelled);

        assert!(
            hanging.cancels.lock().unwrap()[0].load(Ordering::SeqCst),
            "the capture was told to stop"
        );
        assert_eq!(hanging.asks(), 1, "nothing was sent after the capture");
        assert!(
            chat.is_empty(),
            "the stopped request is not in the conversation"
        );
        assert!(
            rt.on_reply(token, "late".into(), None).is_err(),
            "a late reply is dropped"
        );

        let fake = Fake::new(vec![said("Ready.")], vec![]);
        assert_eq!(words(drive(&rt, &chat, &fake, "hello").0), "Ready.");
    }
}
