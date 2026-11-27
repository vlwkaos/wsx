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

/// What the host knows when a worker's result settles. ref: docs/tasks.md "Workflow"
pub struct Settle<'a> {
    /// The result's `status`: `waits_on`, `blocked`, `done` or another.
    pub status: &'a str,
    /// The turn wrote the task's plan.
    pub plan: bool,
    /// The task's step is not its last.
    pub more_steps: bool,
    /// The next step is the build that waits for the `--after` task to land.
    pub build_waits: bool,
    /// The worker was lent the simplify skill and never loaded it, and was not asked yet.
    pub simplify_unrun: bool,
    /// The result has human_verify items the worker proves, within the check retries.
    pub verify_unproven: bool,
    /// The checks of an earlier result still run.
    pub checks_out: bool,
}

/// Where a settled result goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleTo {
    /// The worker waits on its check-run: the run's answer is its next turn.
    CheckRun,
    /// Only the worker's own word blocks a task: it waits on the owner.
    Blocked,
    /// The lead reads the plan and continues the task with an add.
    Plan,
    /// The next step's turn starts, with any queued message.
    Step,
    /// The next step is the build: it waits for the `--after` task.
    BuildWaits,
    /// Back to the worker once, to run the simplify skill.
    Simplify,
    /// Back to the worker, to prove its human_verify items.
    Verify,
    /// The running checks of an earlier result answer this one.
    JoinChecks,
    /// The checks start.
    Checks,
}

/// The first outcome that holds, in this order.
pub fn settle_to(s: &Settle) -> SettleTo {
    let done = s.status == "done";
    match () {
        _ if s.status == "waits_on" => SettleTo::CheckRun,
        _ if s.status == "blocked" => SettleTo::Blocked,
        _ if s.plan => SettleTo::Plan,
        _ if done && s.more_steps && s.build_waits => SettleTo::BuildWaits,
        _ if done && s.more_steps => SettleTo::Step,
        _ if done && s.simplify_unrun => SettleTo::Simplify,
        _ if done && s.verify_unproven => SettleTo::Verify,
        _ if s.checks_out => SettleTo::JoinChecks,
        _ => SettleTo::Checks,
    }
}
