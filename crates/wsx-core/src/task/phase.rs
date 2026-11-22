//! A task's phase: the one place that decides what a task does next. The task host feeds it
//! events and runs the effects it returns; `TaskState`, `WaitOn` and the status kind derive from
//! it, so the stored record, the CLI, the TUI and the web read one model.
//! ref: docs/design/lean.md "Transitions"

use serde::{Deserialize, Serialize};

use super::{TaskState, WaitOn};

/// How far a task that waits for another one got: no turn yet, or planned with its build held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Fresh,
    Planned,
}

/// What a running turn is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Turn {
    /// A workflow step (the first turn, or the next step after a result).
    Step,
    /// An owner's or manager's add.
    Free,
    /// The checks failed and the worker fixes it.
    CheckRetry,
    /// A landing's rebase conflicted; `land` lands it once resolved and checked.
    Conflict { land: bool },
}

/// What a throttled task waits for; the host resumes it by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "on")]
pub enum Throttle {
    Disk,
    Load,
    Limit { until_ms: u64 },
}

/// What a throttled task resumes into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Resume {
    Turn { turn: Turn, text: String },
    Checks { land: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum Phase {
    /// Waits for `record.after` to land.
    Parked { stage: Stage },
    Working { turn: Turn },
    /// The checks run, or wait for their slot (`queued`: why); `land` lands the revision when they pass.
    Checking { queued: Option<String>, land: bool },
    Throttled { throttle: Throttle, then: Resume },
    /// The plan waits for the manager's go.
    Plan,
    Blocked { question: String },
    Paused { why: String },
    /// The owner stopped it; a parked task keeps its stage for the next add.
    Stopped { parked: Option<Stage> },
    /// The owner works in the copy.
    Hand,
    Ready { asks: Option<String>, land_stopped: Option<String> },
    Failed { why: String },
    Landed,
    Closed,
}

/// What happened to the task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Add { text: String },
    Stop,
    AfterEnded,
    Land,
    Close,
    Hand,
    HandBack,
    /// The worker's turn ended with its result.
    Result(Outcome),
    ChecksPassed,
    ChecksFailed { why: String },
    ChecksQueued { why: String },
    ChecksStarted,
    Throttled { throttle: Throttle },
    WaitEnded,
    AgentDied { why: String },
    /// The host started again; a turn that ran is gone.
    Restart,
    /// A land that stopped short (a file in the way) or conflicted.
    LandStopped { why: String },
    LandConflict,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The step is done; `last`: no step follows.
    Done { last: bool },
    Plan,
    Blocked { question: String },
    Paused { why: String },
    Asks { question: String },
}

/// What `next` needs besides the phase and the event: facts of the record.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx {
    /// `record.after` is set: the build (or the first turn) waits for that task.
    pub after: bool,
    /// The next step is the build, which a planned task holds while `after` is set.
    pub build_next: bool,
    /// The failed checks may go back to the worker once more.
    pub retries_left: bool,
    /// `record.hold` is set: a land waits for the owner's try.
    pub held: bool,
}

/// What the host does for a transition, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Start a turn of this kind; the host builds its text from the queued adds once.
    StartTurn(Turn),
    /// Queue the text for the next turn (the log keeps it).
    Queue(String),
    /// Kill the running agent and checks; later events of them are stale.
    Quiesce,
    StartChecks { land: bool },
    /// Move the copy onto the default branch tip; a conflict becomes the worker's.
    FollowDefaultBranch,
    Rebase,
    Land,
    Report,
    StopAgent,
    /// The host refuses the request with these words; the phase stays.
    Refuse(&'static str),
}

/// The one transition function. Pure: the host persists the phase and runs the effects.
pub fn next(phase: &Phase, ctx: Ctx, event: Event) -> (Phase, Vec<Effect>) {
    use Effect::*;
    use Phase as P;
    let stay = |effects: Vec<Effect>| (phase.clone(), effects);
    match (phase, event) {
        (P::Landed | P::Closed, Event::Close) => (P::Closed, vec![]),
        (P::Landed | P::Closed, _) => stay(vec![Refuse("the task is over")]),
        (_, Event::Close) => (P::Closed, vec![Quiesce, StopAgent]),

        // A parked task: adds wait, the wait's end starts it; a restart keeps it.
        (P::Parked { .. }, Event::Add { text }) => stay(vec![Queue(text)]),
        (P::Parked { stage }, Event::AfterEnded) => (
            P::Working {
                turn: match stage {
                    Stage::Fresh => Turn::Step,
                    Stage::Planned => Turn::Step,
                },
            },
            vec![FollowDefaultBranch, StartTurn(Turn::Step)],
        ),
        (P::Parked { stage }, Event::Stop) => (
            P::Stopped {
                parked: Some(*stage),
            },
            vec![],
        ),
        (P::Parked { .. }, Event::Restart) => stay(vec![]),

        // Stopped: nothing runs until an add; a wait that ends meanwhile only opens the gate.
        (P::Stopped { parked }, Event::Add { text }) => match (parked, ctx.after) {
            (Some(stage), true) => (P::Parked { stage: *stage }, vec![Queue(text)]),
            (Some(_), false) => (
                P::Working { turn: Turn::Step },
                vec![Queue(text), FollowDefaultBranch, StartTurn(Turn::Step)],
            ),
            (None, _) => (
                P::Working { turn: Turn::Free },
                vec![Queue(text), StartTurn(Turn::Free)],
            ),
        },
        (P::Stopped { .. }, Event::AfterEnded | Event::Restart | Event::Stop) => stay(vec![]),

        // Every other open phase but Hand stops the same way.
        (P::Hand, Event::Stop) => stay(vec![Refuse("the owner works on it by hand")]),
        (_, Event::Stop) => (P::Stopped { parked: None }, vec![Quiesce]),

        // A running turn.
        (P::Working { .. }, Event::Add { text }) => stay(vec![Queue(text)]),
        (P::Working { .. }, Event::Result(outcome)) => match outcome {
            Outcome::Done { last: true } => (
                P::Checking {
                    queued: None,
                    land: matches!(phase, P::Working { turn: Turn::Conflict { land: true } }),
                },
                vec![StartChecks {
                    land: matches!(phase, P::Working { turn: Turn::Conflict { land: true } }),
                }],
            ),
            Outcome::Done { last: false } if ctx.after && ctx.build_next => (
                P::Parked {
                    stage: Stage::Planned,
                },
                vec![],
            ),
            Outcome::Done { last: false } => {
                (P::Working { turn: Turn::Step }, vec![StartTurn(Turn::Step)])
            }
            Outcome::Plan => (P::Plan, vec![Report]),
            Outcome::Blocked { question } => (P::Blocked { question }, vec![Report]),
            Outcome::Paused { why } => (P::Paused { why }, vec![Report]),
            Outcome::Asks { question } => (
                P::Ready {
                    asks: Some(question),
                    land_stopped: None,
                },
                vec![Report],
            ),
        },
        (P::Working { turn }, Event::Throttled { throttle }) => (
            P::Throttled {
                throttle,
                then: Resume::Turn {
                    turn: *turn,
                    text: String::new(),
                },
            },
            vec![],
        ),
        (P::Working { .. }, Event::AgentDied { why }) => (P::Failed { why }, vec![Report]),
        (P::Working { .. }, Event::Restart) => (
            P::Blocked {
                question: "the host restarted during the turn".into(),
            },
            vec![Report],
        ),

        // Throttled: the host resumes it; a restart re-arms the wait.
        (P::Throttled { .. }, Event::Add { text }) => stay(vec![Queue(text)]),
        (P::Throttled { then, .. }, Event::WaitEnded) => match then {
            Resume::Turn { turn, .. } => (P::Working { turn: *turn }, vec![StartTurn(*turn)]),
            Resume::Checks { land } => (
                P::Checking {
                    queued: None,
                    land: *land,
                },
                vec![StartChecks { land: *land }],
            ),
        },
        (P::Throttled { .. }, Event::Restart) => stay(vec![]),

        // Checks.
        (P::Checking { land, .. }, Event::ChecksQueued { why }) => (
            P::Checking {
                queued: Some(why),
                land: *land,
            },
            vec![],
        ),
        (P::Checking { land, .. }, Event::ChecksStarted) => (
            P::Checking {
                queued: None,
                land: *land,
            },
            vec![],
        ),
        (P::Checking { land: true, .. }, Event::ChecksPassed) => (P::Landed, vec![Land, Report]),
        (P::Checking { land: false, .. }, Event::ChecksPassed) => (
            P::Ready {
                asks: None,
                land_stopped: None,
            },
            vec![Report],
        ),
        (P::Checking { .. }, Event::ChecksFailed { .. }) if ctx.retries_left => (
            P::Working {
                turn: Turn::CheckRetry,
            },
            vec![StartTurn(Turn::CheckRetry)],
        ),
        (P::Checking { .. }, Event::ChecksFailed { why }) => (P::Failed { why }, vec![Report]),
        (P::Checking { land, .. }, Event::Restart) => (
            P::Checking {
                queued: None,
                land: *land,
            },
            vec![StartChecks { land: *land }],
        ),
        (P::Checking { .. }, Event::Add { text }) => stay(vec![Queue(text)]),

        // Land: the one place `hold` and `after` stop it.
        (P::Ready { .. }, Event::Land) if ctx.held => {
            stay(vec![Refuse("held for the owner's try: release the hold first")])
        }
        (P::Ready { .. }, Event::Land) if ctx.after => {
            stay(vec![Refuse("it waits for another task to land first")])
        }
        (P::Ready { .. }, Event::Land) => stay(vec![Rebase]),
        (P::Ready { .. }, Event::LandConflict) => (
            P::Working {
                turn: Turn::Conflict { land: true },
            },
            vec![StartTurn(Turn::Conflict { land: true })],
        ),
        (P::Ready { asks, .. }, Event::LandStopped { why }) => (
            P::Ready {
                asks: asks.clone(),
                land_stopped: Some(why),
            },
            vec![Report],
        ),
        (P::Ready { .. }, Event::ChecksStarted) => (
            P::Checking {
                queued: None,
                land: true,
            },
            vec![],
        ),

        // The owner's or manager's answer moves a waiting task on.
        (
            P::Ready { .. } | P::Plan | P::Blocked { .. } | P::Paused { .. } | P::Failed { .. },
            Event::Add { text },
        ) => (
            P::Working { turn: Turn::Free },
            vec![Queue(text), StartTurn(Turn::Free)],
        ),
        (
            P::Ready { .. } | P::Plan | P::Blocked { .. } | P::Paused { .. } | P::Failed { .. },
            Event::Hand,
        ) => (P::Hand, vec![Quiesce]),
        (P::Hand, Event::HandBack) => (
            P::Checking {
                queued: None,
                land: false,
            },
            vec![StartChecks { land: false }],
        ),
        (P::Hand, Event::Add { .. }) => stay(vec![Refuse("the owner works on it by hand")]),

        // A restart leaves every other phase as it was stored.
        (_, Event::Restart) => stay(vec![]),
        (_, Event::AfterEnded) => stay(vec![]),
        _ => stay(vec![Refuse("not now")]),
    }
}

impl Phase {
    /// The stored state every reader knows.
    pub fn state(&self) -> TaskState {
        match self {
            Self::Working { .. } => TaskState::Working,
            Self::Checking { .. } => TaskState::Checking,
            Self::Ready { .. } => TaskState::Ready,
            Self::Failed { .. } => TaskState::Failed,
            Self::Landed => TaskState::Landed,
            Self::Closed => TaskState::Closed,
            Self::Parked { .. }
            | Self::Throttled { .. }
            | Self::Plan
            | Self::Blocked { .. }
            | Self::Paused { .. }
            | Self::Stopped { .. }
            | Self::Hand => TaskState::Waiting,
        }
    }

    /// What a waiting task waits on, as `record.waiting.on` says it.
    pub fn wait_on(&self) -> Option<WaitOn> {
        Some(match self {
            Self::Parked { .. } | Self::Blocked { .. } | Self::Stopped { .. } => WaitOn::Owner,
            Self::Paused { .. } => WaitOn::Paused,
            Self::Plan => WaitOn::Plan,
            Self::Hand => WaitOn::Hand,
            Self::Throttled { throttle, .. } => match throttle {
                Throttle::Disk => WaitOn::Disk,
                Throttle::Load => WaitOn::Load,
                Throttle::Limit { .. } => WaitOn::Limit,
            },
            Self::Checking {
                queued: Some(_), ..
            } => WaitOn::CheckSlot,
            Self::Ready { asks: Some(_), .. } => WaitOn::Asks,
            Self::Ready {
                land_stopped: Some(_),
                ..
            } => WaitOn::LandStopped,
            _ => return None,
        })
    }
}
