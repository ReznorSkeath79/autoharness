//! Session vocabulary for the PTY engine.
//!
//! These are the types the status reducer speaks. They came across from diri's
//! `diri-proto::model`, where they were wire types shared with a Swift daemon;
//! here they are plain domain types with no wire obligations, so the wire
//! encodings and the `DateMillis` newtype did not travel with them.
//!
//! Nothing in this crate is AutoHarness-specific on purpose. A PTY session's
//! status is not a run's status: the adapter that bridges the two lives in
//! `autoharness-engines`, and keeping the translation in one visible place is
//! what stops two vocabularies from leaking into each other.

use std::time::SystemTime;

/// Why a child stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    Exited,
    Signaled,
    /// The supervising process went away, not the child's own doing.
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitInfo {
    pub reason: ExitReason,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// What an agent is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedsInputKind {
    /// It wants permission to do something.
    Permission,
    /// It asked a question.
    Question,
}

/// How we learned the agent needs input. Screen scraping is the only source
/// this crate produces; the others exist because an agent that reports out of
/// band (a hook, a notify callback) is more trustworthy than a screen read,
/// and the reducer weighs them differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NeedsInputSource {
    ScreenScrape,
    Hook,
    Notify,
}

/// How loudly to ask. A destructive command must not look like a file write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskHint {
    Destructive,
    Network,
    FileWrite,
    Neutral,
}

/// Everything known about a pending prompt. `summary` and `prompt_excerpt`
/// have been through redaction: they are read off a terminal and shown to the
/// user, so they can contain anything the agent happened to print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsInputDetail {
    pub kind: NeedsInputKind,
    pub source: NeedsInputSource,
    pub tool_name: Option<String>,
    pub summary: String,
    pub prompt_excerpt: Option<String>,
    pub options: Option<Vec<String>>,
    pub risk_hint: RiskHint,
    pub occurred_at: SystemTime,
}

/// The canonical answer to "what is this session doing".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    Starting,
    Idle,
    Working,
    NeedsInput(NeedsInputKind),
    Exited(ExitInfo),
    /// Running, but nothing readable has arrived for long enough that claiming
    /// any of the above would be a guess.
    Unknown,
}
