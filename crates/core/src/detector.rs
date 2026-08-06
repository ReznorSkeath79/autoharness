//! Loop and stall detection, and the recovery ladder (PLAN.md "Loop and stall
//! detection").
//!
//! Pure and deterministic: the detector is fed normalized signals and returns
//! verdicts. No IO, no clock, no randomness — so the false-positive fixtures
//! below are exact, and a run's detector history replays identically.
//!
//! The hard part is not catching loops, it is NOT catching productive work.
//! Red-green-refactor repeats the same test command with the same failing
//! result on purpose; a long build emits nothing for minutes; research reads
//! twenty files without touching one. Every trigger here is therefore gated on
//! an explicit absence of progress, and [`Progress`] resets the counters.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// How far back fingerprints are compared. Long enough to catch an alternating
/// pair with noise between, short enough that ancient history cannot trip a
/// trigger.
const HISTORY: usize = 12;

/// What the run just did, normalized. `exact` distinguishes "same command,
/// different argument"; `normalized` collapses volatile detail (paths, ids,
/// durations) so a genuine repeat is recognizable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub exact: String,
    pub normalized: String,
}

impl Fingerprint {
    pub fn new(exact: impl Into<String>, normalized: impl Into<String>) -> Self {
        Self {
            exact: exact.into(),
            normalized: normalized.into(),
        }
    }

    /// Fingerprint a tool action and its result together. Callers normalize by
    /// stripping volatile detail with [`normalize`].
    pub fn action(name: &str, detail: &str) -> Self {
        Self::new(
            format!("{name}:{detail}"),
            format!("{name}:{}", normalize(detail)),
        )
    }
}

/// Collapse volatile detail so two runs of the same work compare equal:
/// digits become `#`, absolute paths keep only their final component, and
/// whitespace is squeezed. Deliberately crude — this feeds a repeat counter,
/// not a diff.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for word in text.split_whitespace() {
        let word = word.rsplit('/').next().unwrap_or(word);
        if !out.is_empty() && !last_was_space {
            out.push(' ');
        }
        for ch in word.chars() {
            if ch.is_ascii_digit() {
                if !out.ends_with('#') {
                    out.push('#');
                }
            } else {
                out.push(ch.to_ascii_lowercase());
            }
        }
        last_was_space = false;
    }
    out
}

/// One observation fed to the detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Signal {
    /// A tool action and the result it produced.
    Action {
        fingerprint: Fingerprint,
        /// The action could not be performed at all (bad arguments, missing
        /// file, malformed command) — distinct from an action that ran and
        /// reported failure.
        invalid: bool,
    },
    /// An error the run hit, classified (compiler error code, test name,
    /// exception type). Class, not message: messages carry volatile detail.
    Error { class: String },
    /// Something genuinely moved. Resets every counter.
    Progress(Progress),
}

/// Evidence that the run is getting somewhere. Task-specific on purpose: a
/// failing test that changed which assertion it fails on IS progress, and a
/// research task that read a new source IS progress, even though neither
/// touched the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    /// Files changed in the worktree.
    Workspace,
    /// A check's result changed (pass↔fail, or a different failure).
    Checks,
    /// A source not previously inspected was read.
    Source,
    /// An artifact or handoff was produced.
    Artifact,
}

/// Why the detector fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Identical action and normalized result three times.
    RepeatedAction { fingerprint: String, count: u32 },
    /// Identical invalid action three times.
    RepeatedInvalidAction { fingerprint: String, count: u32 },
    /// An alternating action pair twice with no progress delta.
    AlternatingPair { first: String, second: String },
    /// The same error class three times without changed code or checks.
    RepeatedErrorClass { class: String, count: u32 },
    /// Six tool actions without workspace, test, source, or artifact progress.
    NoProgress { actions: u32 },
    /// A node or run exhausted its explicit budget.
    BudgetExhausted { limit: String },
}

impl Trigger {
    /// Short stable identifier for the ledger and the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            Trigger::RepeatedAction { .. } => "repeated_action",
            Trigger::RepeatedInvalidAction { .. } => "repeated_invalid_action",
            Trigger::AlternatingPair { .. } => "alternating_pair",
            Trigger::RepeatedErrorClass { .. } => "repeated_error_class",
            Trigger::NoProgress { .. } => "no_progress",
            Trigger::BudgetExhausted { .. } => "budget_exhausted",
        }
    }

    /// One sentence the user can act on.
    pub fn describe(&self) -> String {
        match self {
            Trigger::RepeatedAction { fingerprint, count } => {
                format!("repeated the same action with the same result {count}x: {fingerprint}")
            }
            Trigger::RepeatedInvalidAction { fingerprint, count } => {
                format!("repeated an action that cannot run {count}x: {fingerprint}")
            }
            Trigger::AlternatingPair { first, second } => {
                format!("alternating between two actions with no progress: {first} / {second}")
            }
            Trigger::RepeatedErrorClass { class, count } => {
                format!("hit the same error {count}x without changing code or checks: {class}")
            }
            Trigger::NoProgress { actions } => {
                format!(
                    "{actions} tool actions with no workspace, check, source, or artifact progress"
                )
            }
            Trigger::BudgetExhausted { limit } => format!("exhausted its budget: {limit}"),
        }
    }
}

/// Thresholds, so a promoted policy version can tune them (PLAN.md
/// "Self-evolution" allows detector thresholds; it does not allow sandbox,
/// capability, or external-write changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectorThresholds {
    pub repeated_action: u32,
    pub repeated_invalid_action: u32,
    pub alternating_cycles: u32,
    pub repeated_error_class: u32,
    pub actions_without_progress: u32,
}

impl Default for DetectorThresholds {
    fn default() -> Self {
        // PLAN.md "High-confidence triggers".
        Self {
            repeated_action: 3,
            repeated_invalid_action: 3,
            alternating_cycles: 2,
            repeated_error_class: 3,
            actions_without_progress: 6,
        }
    }
}

/// The recovery ladder. Each rung is attempted once per run; exhausting the
/// ladder blocks the node with the evidence that got it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    /// One constrained nudge prohibiting the repeated action and demanding new
    /// evidence.
    Nudge,
    /// One replan using a critic and the detector's evidence.
    Replan,
    /// One restart from the latest verified checkpoint.
    RestartFromCheckpoint,
    /// Out of moves: surface the exact evidence and stop.
    Blocked,
}

impl Recovery {
    pub fn as_str(self) -> &'static str {
        match self {
            Recovery::Nudge => "nudge",
            Recovery::Replan => "replan",
            Recovery::RestartFromCheckpoint => "restart_from_checkpoint",
            Recovery::Blocked => "blocked",
        }
    }

    /// The next rung. `Blocked` is terminal.
    pub fn next(self) -> Recovery {
        match self {
            Recovery::Nudge => Recovery::Replan,
            Recovery::Replan => Recovery::RestartFromCheckpoint,
            Recovery::RestartFromCheckpoint | Recovery::Blocked => Recovery::Blocked,
        }
    }
}

/// Detector state for one run or node.
#[derive(Debug, Clone)]
pub struct Detector {
    thresholds: DetectorThresholds,
    actions: VecDeque<Fingerprint>,
    invalid: VecDeque<String>,
    errors: VecDeque<String>,
    actions_since_progress: u32,
    /// Next rung to attempt. Advances only when a trigger is acted on.
    next_recovery: Recovery,
    /// Triggers already reported, so one stuck pattern does not re-fire on
    /// every subsequent action.
    reported: Vec<String>,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new(DetectorThresholds::default())
    }
}

impl Detector {
    pub fn new(thresholds: DetectorThresholds) -> Self {
        Self {
            thresholds,
            actions: VecDeque::new(),
            invalid: VecDeque::new(),
            errors: VecDeque::new(),
            actions_since_progress: 0,
            next_recovery: Recovery::Nudge,
            reported: Vec::new(),
        }
    }

    /// Rung that will be attempted for the next trigger.
    pub fn pending_recovery(&self) -> Recovery {
        self.next_recovery
    }

    pub fn actions_since_progress(&self) -> u32 {
        self.actions_since_progress
    }

    /// Feed one signal. Returns a trigger the first time a pattern crosses its
    /// threshold; the same pattern does not fire again until progress resets it.
    pub fn observe(&mut self, signal: Signal) -> Option<Trigger> {
        match signal {
            Signal::Progress(_) => {
                self.reset_after_progress();
                None
            }
            Signal::Error { class } => {
                push_bounded(&mut self.errors, class.clone());
                let count = self.errors.iter().filter(|e| **e == class).count() as u32;
                self.fire_once(
                    count >= self.thresholds.repeated_error_class,
                    Trigger::RepeatedErrorClass { class, count },
                )
            }
            Signal::Action {
                fingerprint,
                invalid,
            } => {
                self.actions_since_progress += 1;
                if invalid {
                    push_bounded(&mut self.invalid, fingerprint.exact.clone());
                    let count = self
                        .invalid
                        .iter()
                        .filter(|f| **f == fingerprint.exact)
                        .count() as u32;
                    if let Some(trigger) = self.fire_once(
                        count >= self.thresholds.repeated_invalid_action,
                        Trigger::RepeatedInvalidAction {
                            fingerprint: fingerprint.exact.clone(),
                            count,
                        },
                    ) {
                        return Some(trigger);
                    }
                }
                push_bounded(&mut self.actions, fingerprint.clone());

                let repeats = self
                    .actions
                    .iter()
                    .filter(|f| f.normalized == fingerprint.normalized)
                    .count() as u32;
                if let Some(trigger) = self.fire_once(
                    repeats >= self.thresholds.repeated_action,
                    Trigger::RepeatedAction {
                        fingerprint: fingerprint.normalized.clone(),
                        count: repeats,
                    },
                ) {
                    return Some(trigger);
                }

                if let Some((first, second)) = self.alternating_pair()
                    && let Some(trigger) = self.fire_once(
                        true,
                        Trigger::AlternatingPair {
                            first: first.clone(),
                            second: second.clone(),
                        },
                    )
                {
                    return Some(trigger);
                }

                self.fire_once(
                    self.actions_since_progress >= self.thresholds.actions_without_progress,
                    Trigger::NoProgress {
                        actions: self.actions_since_progress,
                    },
                )
            }
        }
    }

    /// A budget ceiling was hit. Always fires: a budget is a hard limit, not a
    /// heuristic.
    pub fn budget_exhausted(&mut self, limit: impl Into<String>) -> Trigger {
        Trigger::BudgetExhausted {
            limit: limit.into(),
        }
    }

    /// Consume the next rung of the ladder for a trigger that is being acted
    /// on. Returns the rung to attempt now.
    pub fn take_recovery(&mut self) -> Recovery {
        let current = self.next_recovery;
        self.next_recovery = current.next();
        current
    }

    /// Real progress clears the loop evidence — but NOT the recovery ladder.
    /// A run that needed a nudge, recovered, and got stuck again has already
    /// spent that rung.
    fn reset_after_progress(&mut self) {
        self.actions.clear();
        self.invalid.clear();
        self.errors.clear();
        self.actions_since_progress = 0;
        self.reported.clear();
    }

    /// `A B A B`: the last four actions alternate between two distinct
    /// normalized fingerprints.
    fn alternating_pair(&self) -> Option<(String, String)> {
        let cycles = self.thresholds.alternating_cycles as usize;
        let needed = cycles * 2;
        if self.actions.len() < needed {
            return None;
        }
        let tail: Vec<&str> = self
            .actions
            .iter()
            .rev()
            .take(needed)
            .map(|f| f.normalized.as_str())
            .collect();
        let (a, b) = (tail[0], tail[1]);
        if a == b {
            return None;
        }
        for (i, item) in tail.iter().enumerate() {
            let expected = if i % 2 == 0 { a } else { b };
            if *item != expected {
                return None;
            }
        }
        Some((a.to_string(), b.to_string()))
    }

    /// Report a trigger only the first time this pattern crosses.
    fn fire_once(&mut self, crossed: bool, trigger: Trigger) -> Option<Trigger> {
        if !crossed {
            return None;
        }
        let key = match &trigger {
            Trigger::RepeatedAction { fingerprint, .. } => format!("action:{fingerprint}"),
            Trigger::RepeatedInvalidAction { fingerprint, .. } => format!("invalid:{fingerprint}"),
            Trigger::AlternatingPair { first, second } => format!("alt:{first}|{second}"),
            Trigger::RepeatedErrorClass { class, .. } => format!("error:{class}"),
            Trigger::NoProgress { .. } => "no_progress".to_string(),
            Trigger::BudgetExhausted { limit } => format!("budget:{limit}"),
        };
        if self.reported.contains(&key) {
            return None;
        }
        self.reported.push(key);
        Some(trigger)
    }
}

fn push_bounded<T>(queue: &mut VecDeque<T>, item: T) {
    if queue.len() == HISTORY {
        queue.pop_front();
    }
    queue.push_back(item);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(name: &str, detail: &str) -> Signal {
        Signal::Action {
            fingerprint: Fingerprint::action(name, detail),
            invalid: false,
        }
    }

    fn invalid(name: &str, detail: &str) -> Signal {
        Signal::Action {
            fingerprint: Fingerprint::action(name, detail),
            invalid: true,
        }
    }

    #[test]
    fn normalize_collapses_volatile_detail() {
        assert_eq!(normalize("/a/b/Foo.rs line 42"), "foo.rs line #");
        assert_eq!(normalize("run   test   1234"), "run test #");
        // Two runs of the same work compare equal.
        assert_eq!(
            normalize("/tmp/wt-91af/src/lib.rs:88"),
            normalize("/tmp/wt-22cd/src/lib.rs:12")
        );
        // Genuinely different work does not.
        assert_ne!(normalize("src/lib.rs"), normalize("src/main.rs"));
    }

    // ---- high-confidence triggers ----

    #[test]
    fn identical_action_and_result_three_times_fires() {
        let mut d = Detector::default();
        assert!(d.observe(action("shell", "cargo test")).is_none());
        assert!(d.observe(action("shell", "cargo test")).is_none());
        let trigger = d.observe(action("shell", "cargo test")).unwrap();
        assert_eq!(trigger.kind(), "repeated_action");
        // Does not re-fire on every subsequent identical action.
        assert!(d.observe(action("shell", "cargo test")).is_none());
    }

    #[test]
    fn identical_invalid_action_three_times_fires() {
        let mut d = Detector::default();
        d.observe(invalid("edit", "missing.rs"));
        d.observe(invalid("edit", "missing.rs"));
        let trigger = d.observe(invalid("edit", "missing.rs")).unwrap();
        assert_eq!(trigger.kind(), "repeated_invalid_action");
    }

    #[test]
    fn alternating_pair_twice_fires() {
        let mut d = Detector::default();
        d.observe(action("edit", "a.rs"));
        d.observe(action("shell", "build"));
        d.observe(action("edit", "a.rs"));
        let trigger = d.observe(action("shell", "build")).unwrap();
        assert_eq!(trigger.kind(), "alternating_pair");
    }

    #[test]
    fn same_error_class_three_times_fires() {
        let mut d = Detector::default();
        d.observe(Signal::Error {
            class: "E0308".into(),
        });
        d.observe(Signal::Error {
            class: "E0308".into(),
        });
        let trigger = d
            .observe(Signal::Error {
                class: "E0308".into(),
            })
            .unwrap();
        assert_eq!(trigger.kind(), "repeated_error_class");
    }

    #[test]
    fn six_actions_without_progress_fires() {
        let mut d = Detector::default();
        // Genuinely distinct actions — note `file1/file2` would NOT be, since
        // normalization collapses digits, and the repeated-action trigger
        // would fire first.
        for name in ["alpha", "beta", "gamma", "delta", "epsilon"] {
            assert!(d.observe(action("read", name)).is_none());
        }
        let trigger = d.observe(action("read", "zeta")).unwrap();
        assert_eq!(trigger.kind(), "no_progress");
        assert_eq!(d.actions_since_progress(), 6);
    }

    // ---- false-positive fixtures (PLAN.md: productive work must not trip) ----

    #[test]
    fn tdd_red_green_refactor_is_not_a_loop() {
        let mut d = Detector::default();
        // The same test command, over and over, is the WHOLE point of TDD.
        // Each cycle changes the workspace and the check result.
        for _ in 0..6 {
            assert!(d.observe(action("shell", "cargo test")).is_none());
            assert!(d.observe(Signal::Progress(Progress::Workspace)).is_none());
            assert!(d.observe(action("shell", "cargo test")).is_none());
            assert!(d.observe(Signal::Progress(Progress::Checks)).is_none());
        }
    }

    #[test]
    fn research_reading_many_sources_is_not_a_loop() {
        let mut d = Detector::default();
        // Many reads, nothing edited. Each new source is progress.
        for name in [
            "intro", "design", "api", "faq", "errors", "limits", "auth", "schema", "events",
            "ledger", "sandbox", "router",
        ] {
            assert!(
                d.observe(action("read", name)).is_none(),
                "reading a new source must never trip the detector"
            );
            assert!(d.observe(Signal::Progress(Progress::Source)).is_none());
        }
    }

    #[test]
    fn a_long_build_is_not_a_stall() {
        let mut d = Detector::default();
        // One action, a long silence, then a result. The detector is fed
        // signals, not time, so silence cannot trip it.
        assert!(
            d.observe(action("shell", "cargo build --release"))
                .is_none()
        );
        assert!(d.observe(Signal::Progress(Progress::Checks)).is_none());
        assert_eq!(d.actions_since_progress(), 0);
    }

    #[test]
    fn flaky_infrastructure_retries_are_not_a_loop_while_the_error_changes() {
        let mut d = Detector::default();
        // A flaky suite fails differently each time; the error class differs,
        // so the repeated-error trigger stays quiet.
        for class in ["timeout_a", "timeout_b", "timeout_c", "timeout_d"] {
            assert!(
                d.observe(Signal::Error {
                    class: class.into()
                })
                .is_none()
            );
        }
    }

    #[test]
    fn deterministic_reproduction_with_progress_between_runs_is_not_a_loop() {
        let mut d = Detector::default();
        // Reproducing a bug repeatedly is fine as long as something advances.
        for _ in 0..5 {
            assert!(d.observe(action("shell", "./repro.sh")).is_none());
            assert!(d.observe(Signal::Progress(Progress::Artifact)).is_none());
        }
    }

    // ---- recovery ladder ----

    #[test]
    fn recovery_ladder_escalates_in_order_then_blocks() {
        let mut d = Detector::default();
        assert_eq!(d.pending_recovery(), Recovery::Nudge);
        assert_eq!(d.take_recovery(), Recovery::Nudge);
        assert_eq!(d.take_recovery(), Recovery::Replan);
        assert_eq!(d.take_recovery(), Recovery::RestartFromCheckpoint);
        assert_eq!(d.take_recovery(), Recovery::Blocked);
        // Terminal: never wraps around to try again.
        assert_eq!(d.take_recovery(), Recovery::Blocked);
    }

    #[test]
    fn progress_clears_loop_evidence_but_not_the_spent_ladder() {
        let mut d = Detector::default();
        d.observe(action("shell", "x"));
        d.observe(action("shell", "x"));
        assert!(d.observe(action("shell", "x")).is_some());
        assert_eq!(d.take_recovery(), Recovery::Nudge);

        // Recovered: the same pattern can be detected afresh...
        d.observe(Signal::Progress(Progress::Workspace));
        d.observe(action("shell", "x"));
        d.observe(action("shell", "x"));
        assert!(d.observe(action("shell", "x")).is_some());
        // ...but the ladder does not rewind.
        assert_eq!(d.take_recovery(), Recovery::Replan);
    }

    #[test]
    fn budget_exhaustion_always_fires() {
        let mut d = Detector::default();
        let trigger = d.budget_exhausted("max_turns=200");
        assert_eq!(trigger.kind(), "budget_exhausted");
        assert!(trigger.describe().contains("max_turns"));
    }

    #[test]
    fn thresholds_are_tunable_by_policy() {
        let mut d = Detector::new(DetectorThresholds {
            repeated_action: 2,
            ..DetectorThresholds::default()
        });
        d.observe(action("shell", "x"));
        assert!(d.observe(action("shell", "x")).is_some());
    }
}
