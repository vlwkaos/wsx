//! The task host's decisions as pure rules over plain facts: the host reads the facts, asks the
//! rule and runs its answer. A new case is an arm here, not a branch in the host.
//! ref: docs/design/engine.md "The engine"

/// What the host knows when a worker's check-run has its answer.
pub struct Answer {
    /// The worker's result came without the answer.
    pub settled: bool,
    /// The run's calls got `queued`: no call waits for the answer.
    pub queued: bool,
    /// It failed in a way that is the task's own (not a known failure).
    pub fails: bool,
    /// The run's commit is still the open task's head.
    pub current: bool,
    /// `wsx task stop` ran and no add came since.
    pub stopped: bool,
    /// The worker waits on its run (`waits_on`).
    pub awaits_run: bool,
    /// A worker turn runs now (a keep-warm ping is not one).
    pub worker_turn: bool,
}

/// Where a check-run's answer goes. ref: docs/tasks.md "Check-run"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerTo {
    /// The calls that wait got it.
    Caller,
    /// It joins the next turn and starts none.
    Notice,
    /// It waits for the running turn's result, which decides again.
    Hold,
    /// The worker's next turn, after the checks while they run; at a result, instead of its settle.
    Turn,
    /// A pass: its proof joins a done result.
    Proof,
    /// A failure on a tree the task has left: logged only.
    Drop,
}

/// When the run ends, and again at the result of a turn that ran meanwhile.
pub fn answer_to(a: &Answer) -> AnswerTo {
    // ^ The worker waits on it, or its result has not come: it gets the answer, pass or failure.
    let for_worker = a.awaits_run || !a.settled;
    if a.stopped {
        AnswerTo::Notice
    } else if !a.settled && !a.queued {
        AnswerTo::Caller
    } else if for_worker && a.worker_turn {
        AnswerTo::Hold
    } else if for_worker || (a.fails && a.current) {
        AnswerTo::Turn
    } else if a.fails {
        AnswerTo::Drop
    } else {
        AnswerTo::Proof
    }
}
