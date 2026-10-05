// The assistant's turn, as an explicit state machine.
//
//   idle ──► thinking ──► awaiting_approval ──► executing ──► completed
//              │               │                    │    └──► failed
//              │               └──► cancelled       └───────► cancelled
//              └──► failed / cancelled
//
// A request is sent to the model (`thinking`). If the model proposes an action it is
// parsed, judged by the policy and, unless the policy lets it through, shown to the
// person (`awaiting_approval`). Only an explicit approval moves it to `executing`,
// where the executor carries it out and reports how it went.
//
// Nothing here talks to the network or the operating system: the model's answer
// arrives as data, and the system is reached only through the executor's
// `Launcher`. That is what lets every transition below be tested with fakes.
//
// Cancellation is a first-class transition. While thinking it ends the request (the
// caller supplies the means to stop it); while waiting it withdraws the proposal; and
// while executing it sets a flag the executor looks at immediately before it asks
// the system to do anything. A reply that arrives after a cancel is stale and is
// dropped, so nothing the person cancelled can come back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;

use crate::actions::{self, Action, ActionView, PolicyConfig, Verdict};
use crate::claude::Chat;
use crate::executor::{Executor, Files, Launcher, Outcome};

// ── Data crossing the boundaries ──────────────────────────────────────────────

/// The model's request to use a tool: its id (every one must be answered in the
/// next message) and its input, which is untrusted.
#[derive(Debug, Clone)]
pub struct RawProposal {
    pub tool_use_id: String,
    pub input: Value,
}

/// The answer to one tool use, for the next message to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub text: String,
}

/// Puts a resolution into the conversation, where the next request will carry it.
pub fn settle(chat: &Chat, result: Option<ToolResult>) {
    if let Some(result) = result {
        chat.add_tool_result(result);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Thinking,
    AwaitingApproval,
    Executing,
    Completed,
    Failed,
    Cancelled,
}

/// A proposal as the island shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProposalView {
    pub id: u64,
    #[serde(flatten)]
    pub action: ActionView,
}

/// Everything the island needs to draw, and nothing it could act on without asking
/// Rust again.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub phase: Phase,
    pub proposal: Option<ProposalView>,
    pub message: Option<String>,
}

/// A turn that has begun. A reply is only accepted for the turn it was asked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnToken(u64);

#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

#[derive(Debug, PartialEq, Eq)]
pub struct Stale;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NoSuchProposal,
    WrongPhase,
}

impl Error {
    pub fn message(&self) -> &'static str {
        match self {
            Error::NoSuchProposal => "That request is no longer waiting for an answer.",
            Error::WrongPhase => "Coucou isn't waiting for an answer right now.",
        }
    }
}

/// The go-ahead to carry out one action. Only the runtime makes these.
#[derive(Debug)]
pub struct Approved {
    pub id: u64,
    action: Action,
    cancel: Arc<AtomicBool>,
}

/// What came of the model's reply.
#[derive(Debug)]
pub enum Settled {
    /// Just words.
    Reply { text: String },
    /// An action to show the person.
    Proposed {
        text: String,
        proposal: ProposalView,
    },
    /// The policy let a low-risk action through without asking; carry it out.
    AutoApproved {
        text: String,
        proposal: ProposalView,
        approved: Approved,
    },
    /// The proposal was not valid or not allowed. Nothing will run.
    Refused {
        text: String,
        reason: String,
        result: ToolResult,
    },
}

pub struct CancelReport {
    pub snapshot: Snapshot,
    /// The tool use this resolves, if a proposal was withdrawn.
    pub result: Option<ToolResult>,
}

// ── The machine ───────────────────────────────────────────────────────────────

struct Pending {
    id: u64,
    action: Action,
    view: ActionView,
    tool_use_id: String,
    cancel: Arc<AtomicBool>,
}

struct Inner {
    phase: Phase,
    turn: u64,
    next_proposal: u64,
    pending: Option<Pending>,
    message: Option<String>,
    /// How to stop the request in flight. Supplied by whoever started it.
    abort: Option<Box<dyn FnOnce() + Send>>,
}

pub struct Runtime<L: Launcher> {
    state: Mutex<Inner>,
    executor: Executor<L>,
    files: Files,
    policy: PolicyConfig,
}

const DECLINED: &str = "The user declined, so nothing was done.";
const CANCELLED: &str = "The action was cancelled and nothing was done.";
const SUPERSEDED: &str = "The user sent a new message instead, so the action was not performed.";

impl<L: Launcher> Runtime<L> {
    pub fn new(launcher: L, files: Files, policy: PolicyConfig) -> Self {
        Runtime {
            state: Mutex::new(Inner {
                phase: Phase::Idle,
                turn: 0,
                next_proposal: 0,
                pending: None,
                message: None,
                abort: None,
            }),
            executor: Executor::new(launcher),
            files,
            policy,
        }
    }

    pub fn files(&self) -> &Files {
        &self.files
    }

    pub fn snapshot(&self) -> Snapshot {
        snapshot_of(&self.state.lock().unwrap())
    }

    /// idle (or finished) → thinking. A proposal still waiting is withdrawn: the
    /// person has moved on, and the second request must not run the first's action.
    pub fn begin_turn(&self) -> Result<(TurnToken, Option<ToolResult>), Busy> {
        let mut s = self.state.lock().unwrap();
        let mut superseded = None;
        match s.phase {
            Phase::Thinking | Phase::Executing => return Err(Busy),
            Phase::AwaitingApproval => {
                superseded = s.pending.take().map(|p| ToolResult {
                    tool_use_id: p.tool_use_id,
                    text: SUPERSEDED.into(),
                });
            }
            _ => {}
        }
        s.turn += 1;
        s.phase = Phase::Thinking;
        s.message = None;
        s.pending = None;
        s.abort = None;
        Ok((TurnToken(s.turn), superseded))
    }

    /// Registers how to stop this turn's request. If the turn was already cancelled
    /// by the time this is called, the request is stopped at once.
    pub fn attach_abort(&self, token: TurnToken, abort: impl FnOnce() + Send + 'static) {
        let mut s = self.state.lock().unwrap();
        if s.turn == token.0 && s.phase == Phase::Thinking {
            s.abort = Some(Box::new(abort));
        } else {
            drop(s);
            abort();
        }
    }

    /// The model answered. thinking → idle / awaiting_approval / executing / failed.
    /// `Err(Stale)` if the turn was cancelled meanwhile: the reply is dropped.
    pub fn on_reply(
        &self,
        token: TurnToken,
        text: String,
        proposal: Option<RawProposal>,
    ) -> Result<Settled, Stale> {
        let mut s = self.state.lock().unwrap();
        if s.turn != token.0 || s.phase != Phase::Thinking {
            return Err(Stale);
        }
        s.abort = None;
        let Some(proposal) = proposal else {
            s.phase = Phase::Idle;
            return Ok(Settled::Reply { text });
        };

        let refused = |s: &mut Inner, text: String, reason: String, tool_use_id: String| {
            s.phase = Phase::Failed;
            s.message = Some(reason.clone());
            let result = ToolResult {
                tool_use_id,
                text: format!("Coucou did not perform this and nothing was done: {reason}"),
            };
            Ok(Settled::Refused {
                text,
                reason,
                result,
            })
        };

        // The model's input is untrusted: it becomes an action only by passing the
        // same parser the executor uses, and only the policy can let it run.
        let (action, verdict) = match actions::assess(&proposal.input, &self.policy) {
            Ok(pair) => pair,
            Err(Verdict::Refuse(reason)) => {
                return refused(&mut s, text, reason, proposal.tool_use_id)
            }
            Err(_) => {
                return refused(
                    &mut s,
                    text,
                    "The proposed action was not valid.".into(),
                    proposal.tool_use_id,
                )
            }
        };
        if let Verdict::Refuse(reason) = verdict {
            return refused(&mut s, text, reason, proposal.tool_use_id);
        }

        s.next_proposal += 1;
        let id = s.next_proposal;
        let view = action.view(&|file| self.files.name_of(file));
        let cancel = Arc::new(AtomicBool::new(false));
        let shown = ProposalView {
            id,
            action: view.clone(),
        };
        s.pending = Some(Pending {
            id,
            action: action.clone(),
            view,
            tool_use_id: proposal.tool_use_id,
            cancel: cancel.clone(),
        });
        if verdict == Verdict::Allow {
            s.phase = Phase::Executing;
            Ok(Settled::AutoApproved {
                text,
                proposal: shown,
                approved: Approved { id, action, cancel },
            })
        } else {
            s.phase = Phase::AwaitingApproval;
            Ok(Settled::Proposed {
                text,
                proposal: shown,
            })
        }
    }

    /// The request itself failed (network, key, rate limit). thinking → failed.
    pub fn fail_turn(&self, token: TurnToken, message: String) {
        let mut s = self.state.lock().unwrap();
        if s.turn == token.0 && s.phase == Phase::Thinking {
            s.phase = Phase::Failed;
            s.message = Some(message);
            s.abort = None;
        }
    }

    /// awaiting_approval → executing. Only the proposal currently shown, only once.
    pub fn approve(&self, id: u64) -> Result<Approved, Error> {
        let mut s = self.state.lock().unwrap();
        let pending = Self::awaiting(&s, id)?;
        let approved = Approved {
            id,
            action: pending.action.clone(),
            cancel: pending.cancel.clone(),
        };
        s.phase = Phase::Executing;
        Ok(approved)
    }

    /// awaiting_approval → cancelled.
    pub fn deny(&self, id: u64) -> Result<Option<ToolResult>, Error> {
        let mut s = self.state.lock().unwrap();
        Self::awaiting(&s, id)?;
        let pending = s.pending.take().expect("checked above");
        s.phase = Phase::Cancelled;
        s.message = Some("Declined.".into());
        Ok(Some(ToolResult {
            tool_use_id: pending.tool_use_id,
            text: DECLINED.into(),
        }))
    }

    fn awaiting(s: &Inner, id: u64) -> Result<&Pending, Error> {
        if s.phase != Phase::AwaitingApproval {
            return Err(Error::WrongPhase);
        }
        match &s.pending {
            Some(pending) if pending.id == id => Ok(pending),
            _ => Err(Error::NoSuchProposal),
        }
    }

    /// Carries out an approved action. Blocking: run it off the UI thread. A cancel
    /// that arrives before the system is asked to do anything stops it.
    pub fn execute(&self, approved: &Approved) -> Outcome {
        self.executor.execute(&approved.action, &self.files, &|| {
            approved.cancel.load(Ordering::SeqCst)
        })
    }

    /// executing → completed / failed / cancelled.
    pub fn finish(&self, id: u64, outcome: Outcome) -> (Snapshot, Option<ToolResult>) {
        let mut s = self.state.lock().unwrap();
        if s.phase != Phase::Executing || s.pending.as_ref().is_none_or(|p| p.id != id) {
            return (snapshot_of(&s), None);
        }
        let pending = s.pending.take().expect("checked above");
        let (phase, message, text) = match outcome {
            Outcome::Done(message) => (
                Phase::Completed,
                message.clone(),
                format!("The user approved and Coucou did it: {message}"),
            ),
            Outcome::Failed(message) => (
                Phase::Failed,
                message.clone(),
                format!("The user approved but it could not be done: {message}"),
            ),
            Outcome::Cancelled => (
                Phase::Cancelled,
                "Cancelled before it started.".to_string(),
                CANCELLED.to_string(),
            ),
        };
        s.phase = phase;
        s.message = Some(message);
        let result = ToolResult {
            tool_use_id: pending.tool_use_id,
            text,
        };
        (snapshot_of(&s), Some(result))
    }

    /// The person cancels, whatever is going on.
    pub fn cancel(&self) -> CancelReport {
        let mut s = self.state.lock().unwrap();
        let mut result = None;
        let mut abort = None;
        match s.phase {
            Phase::Thinking => {
                s.phase = Phase::Cancelled;
                s.message = Some("Cancelled.".into());
                abort = s.abort.take();
            }
            Phase::AwaitingApproval => {
                if let Some(pending) = s.pending.take() {
                    result = Some(ToolResult {
                        tool_use_id: pending.tool_use_id,
                        text: CANCELLED.into(),
                    });
                }
                s.phase = Phase::Cancelled;
                s.message = Some("Cancelled.".into());
            }
            Phase::Executing => {
                // The action may already be starting. The flag is read right before
                // the system is called; the outcome decides what was true.
                if let Some(pending) = &s.pending {
                    pending.cancel.store(true, Ordering::SeqCst);
                }
            }
            _ => {}
        }
        let snapshot = snapshot_of(&s);
        drop(s);
        if let Some(abort) = abort {
            abort();
        }
        CancelReport { snapshot, result }
    }

    /// A new conversation: anything in flight is cancelled and the files forgotten.
    pub fn reset(&self) {
        let abort = {
            let mut s = self.state.lock().unwrap();
            if let Some(pending) = &s.pending {
                pending.cancel.store(true, Ordering::SeqCst);
            }
            s.turn += 1; // whatever was asked before is now stale
            s.pending = None;
            s.phase = Phase::Idle;
            s.message = None;
            s.abort.take()
        };
        if let Some(abort) = abort {
            abort();
        }
        self.files.clear();
    }
}

fn snapshot_of(s: &Inner) -> Snapshot {
    let proposal = match (&s.phase, &s.pending) {
        (Phase::AwaitingApproval | Phase::Executing, Some(p)) => Some(ProposalView {
            id: p.id,
            action: p.view.clone(),
        }),
        _ => None,
    };
    Snapshot {
        phase: s.phase,
        proposal,
        message: s.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::AppId;
    use serde_json::json;
    use std::io;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    /// Records what the system was asked to do; does none of it.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<String>>,
        fail: AtomicBool,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, what: String) -> io::Result<()> {
            self.calls.lock().unwrap().push(what);
            if self.fail.load(Ordering::SeqCst) {
                Err(io::Error::other(r"C:\private\place: denied"))
            } else {
                Ok(())
            }
        }
    }

    impl Launcher for &'static Fake {
        fn launch_app(&self, app: AppId) -> io::Result<()> {
            self.record(format!("app:{}", app.id()))
        }
        fn open_url(&self, url: &str) -> io::Result<()> {
            self.record(format!("url:{url}"))
        }
        fn open_path(&self, path: &Path) -> io::Result<()> {
            self.record(format!("path:{}", path.display()))
        }
    }

    /// A runtime over a leaked fake (a test process, so the leak is harmless) with a
    /// throwaway inbox.
    fn runtime_with(policy: PolicyConfig) -> (Runtime<&'static Fake>, &'static Fake) {
        let fake: &'static Fake = Box::leak(Box::default());
        let root = std::env::temp_dir().join(format!(
            "coucou-assistant-{}-{}",
            std::process::id(),
            next()
        ));
        std::fs::create_dir_all(&root).unwrap();
        (Runtime::new(fake, Files::new(root), policy), fake)
    }

    fn runtime() -> (Runtime<&'static Fake>, &'static Fake) {
        runtime_with(PolicyConfig::default())
    }

    fn next() -> usize {
        static N: AtomicUsize = AtomicUsize::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn notepad() -> Option<RawProposal> {
        Some(RawProposal {
            tool_use_id: "toolu_1".into(),
            input: json!({ "type": "open_app", "app": "notepad" }),
        })
    }

    fn proposed(settled: Result<Settled, Stale>) -> ProposalView {
        match settled {
            Ok(Settled::Proposed { proposal, .. }) => proposal,
            other => panic!("expected a proposal, got {other:?}"),
        }
    }

    // ── The happy paths ──────────────────────────────────────────────────────

    #[test]
    fn idle_to_thinking_to_a_reply_with_no_action() {
        let (rt, fake) = runtime();
        assert_eq!(rt.snapshot().phase, Phase::Idle);
        let (token, superseded) = rt.begin_turn().unwrap();
        assert_eq!(superseded, None);
        assert_eq!(rt.snapshot().phase, Phase::Thinking);
        let settled = rt.on_reply(token, "Hello.".into(), None).unwrap();
        assert!(matches!(settled, Settled::Reply { ref text } if text == "Hello."));
        assert_eq!(
            rt.snapshot(),
            Snapshot {
                phase: Phase::Idle,
                proposal: None,
                message: None
            }
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn thinking_to_a_proposal_that_waits_for_the_person() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, "Sure.".into(), notepad()));
        assert_eq!(shown.action.title, "Open Notepad");
        let snapshot = rt.snapshot();
        assert_eq!(snapshot.phase, Phase::AwaitingApproval);
        assert_eq!(snapshot.proposal, Some(shown));
        assert!(
            fake.calls().is_empty(),
            "nothing happens before an approval"
        );
    }

    #[test]
    fn approval_runs_the_action_and_completes() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));

        let approved = rt.approve(shown.id).unwrap();
        assert_eq!(rt.snapshot().phase, Phase::Executing);
        let outcome = rt.execute(&approved);
        assert_eq!(outcome, Outcome::Done("Opened Notepad.".into()));
        let (snapshot, result) = rt.finish(shown.id, outcome);

        assert_eq!(snapshot.phase, Phase::Completed);
        assert_eq!(snapshot.message.as_deref(), Some("Opened Notepad."));
        assert_eq!(snapshot.proposal, None);
        assert_eq!(
            result.unwrap(),
            ToolResult {
                tool_use_id: "toolu_1".into(),
                text: "The user approved and Coucou did it: Opened Notepad.".into()
            }
        );
        assert_eq!(fake.calls(), vec!["app:notepad".to_string()]);
    }

    #[test]
    fn a_failed_action_is_reported_as_failed_without_system_details() {
        let (rt, fake) = runtime();
        fake.fail.store(true, Ordering::SeqCst);
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        let approved = rt.approve(shown.id).unwrap();
        let (snapshot, result) = rt.finish(shown.id, rt.execute(&approved));
        assert_eq!(snapshot.phase, Phase::Failed);
        assert_eq!(
            snapshot.message.as_deref(),
            Some("Notepad couldn't be started.")
        );
        let text = result.unwrap().text;
        assert!(
            text.contains("could not be done") && !text.contains("private"),
            "{text}"
        );
    }

    #[test]
    fn denial_cancels_the_proposal_and_runs_nothing() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        let result = rt.deny(shown.id).unwrap().unwrap();
        assert_eq!(result.text, DECLINED);
        assert_eq!(result.tool_use_id, "toolu_1");
        assert_eq!(
            rt.snapshot(),
            Snapshot {
                phase: Phase::Cancelled,
                proposal: None,
                message: Some("Declined.".into())
            }
        );
        // It cannot be approved afterwards.
        assert_eq!(rt.approve(shown.id).unwrap_err(), Error::WrongPhase);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_proposal_can_be_answered_only_once_and_only_by_its_own_id() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        assert_eq!(rt.approve(shown.id + 1).unwrap_err(), Error::NoSuchProposal);
        assert_eq!(rt.deny(shown.id + 1).unwrap_err(), Error::NoSuchProposal);
        let approved = rt.approve(shown.id).unwrap();
        // A second approval, or a denial, of the same proposal is refused.
        assert_eq!(rt.approve(shown.id).unwrap_err(), Error::WrongPhase);
        assert_eq!(rt.deny(shown.id).unwrap_err(), Error::WrongPhase);
        rt.finish(shown.id, rt.execute(&approved));
        assert_eq!(rt.approve(shown.id).unwrap_err(), Error::WrongPhase);
        assert_eq!(fake.calls().len(), 1, "it ran exactly once");
    }

    // ── Cancellation ─────────────────────────────────────────────────────────

    #[test]
    fn cancelling_while_thinking_stops_the_request_and_drops_a_late_reply() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        rt.attach_abort(token, move || flag.store(true, Ordering::SeqCst));

        let report = rt.cancel();
        assert_eq!(
            report.snapshot,
            Snapshot {
                phase: Phase::Cancelled,
                proposal: None,
                message: Some("Cancelled.".into())
            }
        );
        assert!(
            stopped.load(Ordering::SeqCst),
            "the request in flight was stopped"
        );

        // A reply that was already on its way arrives anyway: it is ignored.
        assert_eq!(
            rt.on_reply(token, "too late".into(), notepad())
                .unwrap_err(),
            Stale
        );
        assert_eq!(rt.snapshot().phase, Phase::Cancelled);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn an_abort_attached_after_the_cancel_runs_at_once() {
        let (rt, _) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        rt.cancel();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        rt.attach_abort(token, move || flag.store(true, Ordering::SeqCst));
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[test]
    fn cancelling_while_waiting_withdraws_the_proposal() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        let report = rt.cancel();
        assert_eq!(report.snapshot.phase, Phase::Cancelled);
        assert_eq!(report.result.unwrap().text, CANCELLED);
        assert_eq!(
            rt.approve(shown.id).unwrap_err(),
            Error::WrongPhase,
            "it can no longer be approved"
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn cancelling_after_approval_but_before_the_launch_stops_it() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        let approved = rt.approve(shown.id).unwrap();

        let report = rt.cancel();
        assert_eq!(
            report.snapshot.phase,
            Phase::Executing,
            "still running until the executor answers"
        );

        let outcome = rt.execute(&approved);
        assert_eq!(outcome, Outcome::Cancelled);
        let (snapshot, result) = rt.finish(shown.id, outcome);
        assert_eq!(snapshot.phase, Phase::Cancelled);
        assert_eq!(result.unwrap().text, CANCELLED);
        assert!(fake.calls().is_empty(), "the system was never asked");
    }

    #[test]
    fn cancel_does_nothing_when_nothing_is_going_on() {
        let (rt, _) = runtime();
        let report = rt.cancel();
        assert_eq!(report.snapshot.phase, Phase::Idle);
        assert!(report.result.is_none());
    }

    #[test]
    fn a_new_message_withdraws_a_waiting_proposal() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        let (_, superseded) = rt.begin_turn().unwrap();
        assert_eq!(
            superseded.unwrap(),
            ToolResult {
                tool_use_id: "toolu_1".into(),
                text: SUPERSEDED.into()
            }
        );
        assert_eq!(rt.approve(shown.id).unwrap_err(), Error::WrongPhase);
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn only_one_turn_at_a_time() {
        let (rt, _) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        assert_eq!(rt.begin_turn().unwrap_err(), Busy, "still thinking");
        let shown = proposed(rt.on_reply(token, String::new(), notepad()));
        rt.approve(shown.id).unwrap();
        assert_eq!(rt.begin_turn().unwrap_err(), Busy, "still executing");
    }

    #[test]
    fn a_turn_can_begin_again_after_each_way_of_ending() {
        let (rt, _) = runtime();
        for end in 0..4 {
            let (token, _) = rt.begin_turn().unwrap();
            match end {
                0 => drop(rt.on_reply(token, "x".into(), None)),
                1 => rt.fail_turn(token, "No connection.".into()),
                2 => drop(rt.cancel()),
                _ => {
                    let shown = proposed(rt.on_reply(token, String::new(), notepad()));
                    drop(rt.deny(shown.id));
                }
            }
            assert!(rt.begin_turn().is_ok(), "after way {end}");
            rt.cancel();
        }
    }

    #[test]
    fn a_failed_request_is_a_failed_turn() {
        let (rt, _) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        rt.fail_turn(token, "No connection.".into());
        assert_eq!(
            rt.snapshot(),
            Snapshot {
                phase: Phase::Failed,
                proposal: None,
                message: Some("No connection.".into())
            }
        );
        // A stale failure (the turn moved on) changes nothing.
        let (_, _) = rt.begin_turn().unwrap();
        rt.fail_turn(token, "late".into());
        assert_eq!(rt.snapshot().phase, Phase::Thinking);
    }

    #[test]
    fn a_new_conversation_cancels_everything_in_flight() {
        let (rt, fake) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        rt.attach_abort(token, move || flag.store(true, Ordering::SeqCst));
        rt.reset();
        assert!(stopped.load(Ordering::SeqCst));
        assert_eq!(rt.snapshot().phase, Phase::Idle);
        assert_eq!(
            rt.on_reply(token, "late".into(), notepad()).unwrap_err(),
            Stale
        );
        assert!(fake.calls().is_empty());
    }

    // ── Policy ───────────────────────────────────────────────────────────────

    #[test]
    fn auto_approval_runs_a_low_risk_action_but_still_asks_for_a_riskier_one() {
        let (rt, fake) = runtime_with(PolicyConfig {
            auto_approve_low_risk: true,
        });
        let (token, _) = rt.begin_turn().unwrap();
        match rt.on_reply(token, String::new(), notepad()).unwrap() {
            Settled::AutoApproved {
                approved, proposal, ..
            } => {
                assert_eq!(rt.snapshot().phase, Phase::Executing);
                let (snapshot, _) = rt.finish(proposal.id, rt.execute(&approved));
                assert_eq!(snapshot.phase, Phase::Completed);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(fake.calls(), vec!["app:notepad".to_string()]);

        let (token, _) = rt.begin_turn().unwrap();
        let local = RawProposal {
            tool_use_id: "t2".into(),
            input: json!({ "type": "open_url", "url": "http://localhost:8080/" }),
        };
        let shown = proposed(rt.on_reply(token, String::new(), Some(local)));
        assert_eq!(shown.action.title, "Open localhost in your browser");
        assert_eq!(rt.snapshot().phase, Phase::AwaitingApproval);
        assert_eq!(fake.calls().len(), 1, "the riskier one waited");
    }

    // ── Model output cannot get past validation ─────────────────────────────

    #[test]
    fn model_output_that_is_not_a_valid_action_is_refused_and_runs_nothing() {
        for policy in [
            PolicyConfig::default(),
            PolicyConfig {
                auto_approve_low_risk: true,
            },
        ] {
            let (rt, fake) = runtime_with(policy);
            for input in [
                // Commands and executables.
                json!({ "type": "run_command", "command": "calc.exe" }),
                json!({ "type": "open_app", "app": "cmd" }),
                json!({ "type": "open_app", "app": "powershell" }),
                json!({ "type": "open_app", "app": "C:\\Windows\\System32\\cmd.exe" }),
                json!({ "type": "open_app", "app": "notepad.exe /c calc" }),
                json!({ "type": "open_app", "app": "notepad", "args": "C:\\Windows\\win.ini" }),
                // Command injection through a link, a scheme, a path.
                json!({ "type": "open_url", "url": "https://example.com\" & calc.exe & \"" }),
                json!({ "type": "open_url", "url": "javascript:alert(1)" }),
                json!({ "type": "open_url", "url": "file:///C:/Windows/System32/cmd.exe" }),
                json!({ "type": "open_url", "url": "ms-msdt:/id PCWDiagnostic /skip force /param \"IT_RebrowseForFile=x\"" }),
                json!({ "type": "open_url", "url": "https://user:password@example.com/" }),
                // Path traversal and paths where an id belongs.
                json!({ "type": "open_file", "fileId": "../../Windows/win.ini" }),
                json!({ "type": "open_file", "fileId": "C:\\Windows\\win.ini" }),
                json!({ "type": "open_file", "fileId": "f1", "path": "C:\\Windows\\win.ini" }),
                // Malformed shapes.
                json!({ "type": 5 }),
                json!({}),
                json!(null),
                json!("open_app notepad"),
                json!([{ "type": "open_app", "app": "notepad" }]),
            ] {
                let (token, _) = rt.begin_turn().unwrap();
                let proposal = Some(RawProposal {
                    tool_use_id: "toolu_x".into(),
                    input: input.clone(),
                });
                match rt.on_reply(token, "ok".into(), proposal).unwrap() {
                    Settled::Refused { reason, result, .. } => {
                        assert!(!reason.is_empty());
                        assert_eq!(
                            result.tool_use_id, "toolu_x",
                            "the tool use is still answered"
                        );
                        assert!(result.text.contains("nothing was done"));
                    }
                    other => panic!("{input} was not refused: {other:?}"),
                }
                assert_eq!(rt.snapshot().phase, Phase::Failed, "{input}");
                assert_eq!(rt.snapshot().proposal, None);
            }
            assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        }
    }

    #[test]
    fn the_refusal_does_not_repeat_the_models_text_back() {
        let (rt, _) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        let sneaky = json!({ "type": "calc.exe\n&& del /q *.*" });
        let Settled::Refused { reason, result, .. } = rt
            .on_reply(
                token,
                String::new(),
                Some(RawProposal {
                    tool_use_id: "t".into(),
                    input: sneaky,
                }),
            )
            .unwrap()
        else {
            panic!("expected a refusal")
        };
        // A few harmless letters of the name may show; the shell syntax never does.
        for text in [reason, result.text] {
            assert!(
                !text.contains(['&', '\n', '*', '/', '|', '>', '<']),
                "{text}"
            );
        }
    }

    #[test]
    fn an_attached_file_is_proposed_by_id_and_a_bad_id_is_refused() {
        let (rt, fake) = runtime();
        let note = rt.files().register_for_test("notes.txt");
        let (token, _) = rt.begin_turn().unwrap();
        let input = json!({ "type": "open_file", "fileId": note.as_str() });
        let shown = proposed(rt.on_reply(
            token,
            String::new(),
            Some(RawProposal {
                tool_use_id: "t".into(),
                input,
            }),
        ));
        assert_eq!(shown.action.title, "Open notes.txt");
        let approved = rt.approve(shown.id).unwrap();
        let (snapshot, _) = rt.finish(shown.id, rt.execute(&approved));
        assert_eq!(snapshot.phase, Phase::Completed);
        assert_eq!(fake.calls().len(), 1);

        // An id that was never issued parses fine and then fails when used.
        let (token, _) = rt.begin_turn().unwrap();
        let input = json!({ "type": "open_file", "fileId": "f404" });
        let shown = proposed(rt.on_reply(
            token,
            String::new(),
            Some(RawProposal {
                tool_use_id: "t".into(),
                input,
            }),
        ));
        let approved = rt.approve(shown.id).unwrap();
        let (snapshot, _) = rt.finish(shown.id, rt.execute(&approved));
        assert_eq!(
            (snapshot.phase, snapshot.message.as_deref()),
            (Phase::Failed, Some("That file isn't available any more."))
        );
        assert_eq!(fake.calls().len(), 1);
    }

    // ── End to end, with a fake model and a fake system ──────────────────────
    //
    // One full turn the way `chat_send` does it: begin, build the message (with the
    // answers owed from before), take the model's answer as the API would send it,
    // and let the runtime judge it. Then the person answers, the executor acts, and
    // the next request is checked to be one the API would accept.

    use crate::claude::{interpret, request_body, tool_result_blocks};

    /// A user message, then the model's reply (given as content blocks).
    fn turn(rt: &Runtime<&'static Fake>, chat: &Chat, user: &str, blocks: Vec<Value>) -> Settled {
        let (token, superseded) = rt.begin_turn().unwrap();
        settle(chat, superseded);
        let mut content = tool_result_blocks(&chat.take_tool_results());
        content.push(json!({ "type": "text", "text": user }));
        chat.push(json!({ "role": "user", "content": content }));
        chat.push(json!({ "role": "assistant", "content": blocks.clone() }));
        let found = interpret(&blocks);
        for result in found.unanswered {
            chat.add_tool_result(result);
        }
        let settled = rt.on_reply(token, found.text, found.proposal).unwrap();
        if let Settled::Refused { result, .. } = &settled {
            settle(chat, Some(result.clone()));
        }
        settled
    }

    fn model_proposes(id: &str, input: Value) -> Vec<Value> {
        vec![
            json!({ "type": "text", "text": "Sure." }),
            json!({ "type": "tool_use", "id": id, "name": "propose_action", "input": input }),
        ]
    }

    /// What the API enforces: every tool use is answered in the very next message,
    /// and the answers come before anything else in it.
    fn assert_the_next_request_is_well_formed(chat: &Chat, next_user_text: &str) -> Value {
        let mut content = tool_result_blocks(&chat.take_tool_results());
        content.push(json!({ "type": "text", "text": next_user_text }));
        chat.push(json!({ "role": "user", "content": content }));
        let body = request_body("claude-opus-5", chat.snapshot());
        let messages = body["messages"].as_array().unwrap();
        for (i, message) in messages.iter().enumerate() {
            if message["role"] != "assistant" {
                continue;
            }
            let uses: Vec<&str> = message["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| b["id"].as_str().unwrap())
                .collect();
            if uses.is_empty() {
                continue;
            }
            let next = messages
                .get(i + 1)
                .unwrap_or_else(|| panic!("tool use {uses:?} with no message after it"));
            let blocks = next["content"].as_array().unwrap();
            let answered: Vec<&str> = blocks
                .iter()
                .take_while(|b| b["type"] == "tool_result")
                .map(|b| b["tool_use_id"].as_str().unwrap())
                .collect();
            for id in &uses {
                assert!(
                    answered.contains(id),
                    "tool use {id} is not answered first in the next message: {blocks:?}"
                );
            }
        }
        body
    }

    #[test]
    fn end_to_end_open_notepad_is_proposed_approved_executed_and_reported() {
        let (rt, fake) = runtime();
        let chat = Chat::default();

        // "Open Notepad." → the model proposes → the island would show "Open Notepad?"
        let settled = turn(
            &rt,
            &chat,
            "Open Notepad.",
            model_proposes("toolu_01", json!({ "type": "open_app", "app": "notepad" })),
        );
        let Settled::Proposed { text, proposal } = settled else {
            panic!("expected a proposal")
        };
        assert_eq!(text, "Sure.");
        assert_eq!(proposal.action.title, "Open Notepad");
        assert_eq!(rt.snapshot().phase, Phase::AwaitingApproval);
        assert!(fake.calls().is_empty(), "nothing happened yet");

        // The person approves; the executor launches the allowlisted program.
        let approved = rt.approve(proposal.id).unwrap();
        let (snapshot, result) = rt.finish(proposal.id, rt.execute(&approved));
        settle(&chat, result);
        assert_eq!(
            (snapshot.phase, snapshot.message.as_deref()),
            (Phase::Completed, Some("Opened Notepad."))
        );
        assert_eq!(fake.calls(), vec!["app:notepad".to_string()]);

        // The model is told how it went, in a request the API will accept.
        let body = assert_the_next_request_is_well_formed(&chat, "Thanks.");
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["tool_use_id"], "toolu_01");
        assert!(last["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Opened Notepad."));
        assert_eq!(last["content"][1]["text"], "Thanks.");
    }

    #[test]
    fn end_to_end_a_denied_proposal_runs_nothing_and_the_model_is_told() {
        let (rt, fake) = runtime();
        let chat = Chat::default();
        let Settled::Proposed { proposal, .. } = turn(
            &rt,
            &chat,
            "Open example.com",
            model_proposes(
                "toolu_02",
                json!({ "type": "open_url", "url": "https://example.com/" }),
            ),
        ) else {
            panic!()
        };
        assert_eq!(proposal.action.title, "Open example.com in your browser");
        settle(&chat, rt.deny(proposal.id).unwrap());
        assert_eq!(rt.snapshot().phase, Phase::Cancelled);
        let body = assert_the_next_request_is_well_formed(&chat, "Never mind.");
        assert!(body["messages"].to_string().contains("declined"));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn end_to_end_moving_on_without_answering_withdraws_the_proposal_and_keeps_the_conversation_valid(
    ) {
        let (rt, fake) = runtime();
        let chat = Chat::default();
        let Settled::Proposed { proposal, .. } = turn(
            &rt,
            &chat,
            "Open Notepad.",
            model_proposes("toolu_03", json!({ "type": "open_app", "app": "notepad" })),
        ) else {
            panic!()
        };
        // The person types something else instead of answering the card.
        let settled = turn(
            &rt,
            &chat,
            "Actually, what time is it in Tokyo?",
            vec![json!({ "type": "text", "text": "It's evening there." })],
        );
        assert!(matches!(settled, Settled::Reply { .. }));
        assert_eq!(
            rt.approve(proposal.id).unwrap_err(),
            Error::WrongPhase,
            "the old card can no longer be approved"
        );
        assert_the_next_request_is_well_formed(&chat, "ok");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn end_to_end_an_invalid_proposal_is_refused_and_still_answered() {
        let (rt, fake) = runtime();
        let chat = Chat::default();
        let settled = turn(
            &rt,
            &chat,
            "Run calc",
            model_proposes(
                "toolu_04",
                json!({ "type": "run_command", "command": "calc.exe" }),
            ),
        );
        assert!(matches!(settled, Settled::Refused { .. }));
        assert_eq!(rt.snapshot().phase, Phase::Failed);
        let body = assert_the_next_request_is_well_formed(&chat, "hmm");
        assert!(body["messages"].to_string().contains("nothing was done"));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn end_to_end_extra_tool_calls_are_answered_without_being_acted_on() {
        let (rt, fake) = runtime();
        let chat = Chat::default();
        let mut blocks =
            model_proposes("toolu_05", json!({ "type": "open_app", "app": "notepad" }));
        blocks.push(json!({ "type": "tool_use", "id": "toolu_06", "name": "propose_action", "input": { "type": "open_app", "app": "calculator" } }));
        blocks.push(json!({ "type": "tool_use", "id": "toolu_07", "name": "bash", "input": { "command": "dir" } }));
        let Settled::Proposed { proposal, .. } = turn(&rt, &chat, "open both", blocks) else {
            panic!()
        };
        assert_eq!(proposal.action.title, "Open Notepad", "only the first");
        settle(&chat, rt.deny(proposal.id).unwrap());
        // All three tool uses are answered, whatever happened to each.
        assert_the_next_request_is_well_formed(&chat, "ok");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn end_to_end_a_failed_launch_is_reported_to_the_model_and_the_person() {
        let (rt, fake) = runtime();
        fake.fail.store(true, Ordering::SeqCst);
        let chat = Chat::default();
        let Settled::Proposed { proposal, .. } = turn(
            &rt,
            &chat,
            "Open Notepad.",
            model_proposes("toolu_08", json!({ "type": "open_app", "app": "notepad" })),
        ) else {
            panic!()
        };
        let approved = rt.approve(proposal.id).unwrap();
        let (snapshot, result) = rt.finish(proposal.id, rt.execute(&approved));
        settle(&chat, result);
        assert_eq!(snapshot.phase, Phase::Failed);
        let body = assert_the_next_request_is_well_formed(&chat, "ok");
        let text = body["messages"].to_string();
        assert!(
            text.contains("could not be done") && !text.contains("private"),
            "{text}"
        );
    }

    #[test]
    fn a_snapshot_serialises_as_the_island_expects() {
        let (rt, _) = runtime();
        let (token, _) = rt.begin_turn().unwrap();
        proposed(rt.on_reply(token, String::new(), notepad()));
        let json = serde_json::to_value(rt.snapshot()).unwrap();
        assert_eq!(
            json,
            json!({
                "phase": "awaiting_approval",
                "proposal": { "id": 1, "kind": "open_app", "title": "Open Notepad", "target": null, "risk": "low" },
                "message": null
            })
        );
    }
}
