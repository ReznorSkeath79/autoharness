//! PTY-backed agent sessions: process ownership, output logging, headless
//! terminal emulation, and manifest-driven status detection.
//!
//! Transplanted from diri's `diri-engine` (Apache-2.0; see NOTICE). What
//! carried across is everything that is domain-free — the parts that know
//! about file descriptors and terminals rather than about runs. diri's wire
//! types were replaced with [`types`], and nothing here knows what an
//! AutoHarness run is: the bridge from a session's status to a run's events
//! lives in `autoharness-engines`, in one place, on purpose.
//!
//! Why this exists at all: Codex and Claude speak structured JSON, but most
//! coding agents do not. An agent that only paints a terminal can still be
//! driven, watched, and answered — but only by something that owns a real
//! pseudo-terminal and reads what was actually painted.

pub mod agent;
pub mod detect;
pub mod log;
pub mod manifests;
pub mod pty;
pub mod screen;
pub mod status;
pub mod types;

pub use types::{
    ExitInfo, ExitReason, NeedsInputDetail, NeedsInputKind, NeedsInputSource, RiskHint,
    SessionStatus,
};
