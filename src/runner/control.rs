//! Target-level desired state + action planning primitives.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
    Restarted,
}

impl DesiredState {
    pub fn is_fulfilled(&self, status: &TargetStatus) -> bool {
        match self {
            DesiredState::Running => status.is_running,
            DesiredState::Stopped => !status.is_running,
            DesiredState::Restarted => false,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
            DesiredState::Restarted => "restarted",
        }
    }

    pub fn make_plan(&self, target: &str) -> Action {
        match self {
            DesiredState::Running => Action::Start {
                on_success: Box::new(Action::Idle(ActionResult::Ok)),
                on_failure: Box::new(Action::Idle(ActionResult::Error(format!(
                    "failed to start {target}"
                )))),
            },
            DesiredState::Stopped => Action::Stop {
                on_success: Box::new(Action::Idle(ActionResult::Ok)),
                on_failure: Box::new(Action::Idle(ActionResult::Error(format!(
                    "failed to stop {target}"
                )))),
            },
            DesiredState::Restarted => Action::Stop {
                on_success: Box::new(Action::SetDesiredState {
                    desired: DesiredState::Running,
                    next: Box::new(Action::Idle(ActionResult::NotStarted)),
                }),
                on_failure: Box::new(Action::Idle(ActionResult::Error(format!(
                    "failed to stop {target}"
                )))),
            },
        }
    }
}

impl fmt::Display for DesiredState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Start {
        on_success: Box<Action>,
        on_failure: Box<Action>,
    },
    Stop {
        on_success: Box<Action>,
        on_failure: Box<Action>,
    },
    SetDesiredState {
        desired: DesiredState,
        next: Box<Action>,
    },
    Idle(ActionResult),
}

impl Action {
    pub fn idle(result: ActionResult) -> Self {
        Action::Idle(result)
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Action::Idle(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Action::Start {
                on_success,
                on_failure,
            } => format!(
                "Start(success: {}, failure: {})",
                on_success.describe(),
                on_failure.describe()
            ),
            Action::Stop {
                on_success,
                on_failure,
            } => format!(
                "Stop(success: {}, failure: {})",
                on_success.describe(),
                on_failure.describe()
            ),
            Action::SetDesiredState { desired, next } => {
                format!("SetDesiredState({}, next: {})", desired, next.describe())
            }
            Action::Idle(result) => format!("Idle({result})"),
        }
    }
}

impl Default for Action {
    fn default() -> Self {
        Action::Idle(ActionResult::NotStarted)
    }
}

#[derive(Debug, Clone)]
pub enum ActionResult {
    NotStarted,
    Ok,
    Error(String),
}

impl fmt::Display for ActionResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionResult::NotStarted => write!(f, "not started"),
            ActionResult::Ok => write!(f, "ok"),
            ActionResult::Error(message) => write!(f, "error: {message}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TargetStatus {
    pub is_running: bool,
}

#[derive(Debug, Clone)]
pub struct TargetControl {
    pub desired_state: DesiredState,
    pub action_plan: Action,
    pub watch_request: bool,
    pub kind: TargetKind,
}

impl TargetControl {
    pub fn new() -> Self {
        Self {
            desired_state: DesiredState::Stopped,
            action_plan: Action::default(),
            watch_request: false,
            kind: TargetKind::Unknown,
        }
    }

    pub fn set_watch_request(&mut self, watch: bool) {
        self.watch_request = watch;
    }

    pub fn request_desired_state(&mut self, desired: DesiredState) {
        self.desired_state = desired;
        self.action_plan = Action::Idle(ActionResult::NotStarted);
    }

    pub fn settle_desired_state(&mut self, desired: DesiredState) {
        self.desired_state = desired;
        self.action_plan = Action::Idle(ActionResult::Ok);
    }

    pub fn schedule_plan_rebuild(&mut self) {
        self.action_plan = Action::Idle(ActionResult::NotStarted);
    }

    pub fn set_kind(&mut self, kind: TargetKind) {
        self.kind = kind;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Unknown,
    Service,
    Job,
}
