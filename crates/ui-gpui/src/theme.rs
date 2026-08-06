//! Design tokens.
//!
//! Ported from diri (<https://github.com/cristicretu/diri>, Apache-2.0) —
//! specifically `crates/diri-ui/src/tokens.rs` and `status.rs`: the same type
//! scale, radii, spacing, semantic colours, and status vocabulary, adapted to
//! AutoHarness's domain (runs, nodes, worktrees rather than sessions).
//!
//! Two ideas from that system are worth stating, because they are why the
//! tokens look the way they do:
//!
//! 1. **Text tone is alpha over one foreground**, not a palette of greys. A
//!    secondary label is the primary colour at 60% — so it stays correct on
//!    any surface, and there is no "which grey was that" question.
//! 2. **Status colour is semantic, and "needs input" outranks everything.**
//!    A run wanting the user is amber, a destructive one red, freshly-finished
//!    work green, and anything settled fades back into the neutral scale.

use gpui::{FontWeight, Rgba};

/// Build a colour from 8-bit channels, the way the ported tokens are written.
pub const fn rgba8(r: u8, g: u8, b: u8, a: u8) -> Rgba {
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    }
}

/// Corner radii, smallest to largest.
pub struct Radius;

impl Radius {
    pub const CHIP: f32 = 5.0;
    pub const BADGE: f32 = 6.0;
    pub const ROW: f32 = 7.0;
    pub const CARD: f32 = 10.0;
    pub const PANEL: f32 = 12.0;
}

/// Type scale. Three sizes only (11 / 13 / 15) separated by weight, which is
/// what keeps a dense tool readable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TypeStyle {
    pub size: f32,
    pub weight: FontWeight,
    pub monospaced: bool,
}

impl TypeStyle {
    const fn new(size: f32, weight: FontWeight, monospaced: bool) -> Self {
        Self {
            size,
            weight,
            monospaced,
        }
    }

    /// Line height for stacked rows of this style.
    pub fn line_height(self) -> f32 {
        self.size * 1.45
    }
}

pub struct Typo;

impl Typo {
    pub const META: TypeStyle = TypeStyle::new(11.0, FontWeight::MEDIUM, false);
    pub const SECTION_HEADER: TypeStyle = TypeStyle::new(11.0, FontWeight::SEMIBOLD, false);
    pub const ROW: TypeStyle = TypeStyle::new(13.0, FontWeight::NORMAL, false);
    pub const ROW_EMPHASIZED: TypeStyle = TypeStyle::new(13.0, FontWeight::MEDIUM, false);
    pub const TITLE: TypeStyle = TypeStyle::new(13.0, FontWeight::SEMIBOLD, false);
    pub const DISPLAY_TITLE: TypeStyle = TypeStyle::new(15.0, FontWeight::SEMIBOLD, false);
    pub const META_MONO: TypeStyle = TypeStyle::new(11.0, FontWeight::MEDIUM, true);
}

pub struct Space;

impl Space {
    pub const INDENT: f32 = 12.0;
    pub const ROW_H: f32 = 8.0;
    pub const INSET: f32 = 10.0;
}

/// The approved cockpit's sidebar is roughly 23.4% of a 1280-wide shell.
pub const SIDEBAR_WIDTH: f32 = 300.0;

pub struct Metrics;

impl Metrics {
    pub const TITLE_BAR: f32 = 48.0;
    pub const TOOLBAR_EDGE_INSET: f32 = 12.0;
    pub const TOOLBAR_CONTROL_SIZE: f32 = 26.0;
    pub const TOOLBAR_CHIP_HEIGHT: f32 = 24.0;
    pub const INSPECTOR_WIDTH: f32 = 379.0;
    pub const ROW_HEIGHT: f32 = 28.0;
    /// macOS traffic lights sit here; nothing else may.
    pub const TRAFFIC_LIGHT_LANE: f32 = 66.0;
}

/// Text emphasis, expressed as alpha over the primary foreground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Full-strength text.
    Primary,
    /// Supporting text: 60% on content, 70% on a sidebar.
    Secondary,
    /// Faint text: 30% on content, 44% on a sidebar.
    Tertiary,
}

/// A surface's colour family. Sidebars sit over denser material, so their
/// supporting tones are firmer or they lose perceived contrast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Content,
    Sidebar,
}

/// Dark appearance only: the shell has no light mode yet, and inventing one
/// without being able to look at it would be guessing.
pub struct Colors;

impl Colors {
    /// Window background.
    pub const BACKGROUND: Rgba = rgba8(0, 0, 0, 0xff);
    /// Persistent panel material.
    pub const SURFACE: Rgba = rgba8(0, 0, 0, 0xff);
    /// Floating and selected material.
    pub const RAISED: Rgba = rgba8(10, 10, 10, 0xff);

    /// Foreground at a tone, for a surface.
    pub fn text(surface: Surface, tone: Tone) -> Rgba {
        let alpha = match (surface, tone) {
            (_, Tone::Primary) => 1.00,
            (Surface::Content, Tone::Secondary) => 0.60,
            (Surface::Content, Tone::Tertiary) => 0.30,
            (Surface::Sidebar, Tone::Secondary) => 0.70,
            (Surface::Sidebar, Tone::Tertiary) => 0.44,
        };
        white(alpha)
    }

    /// Hairline separating floating material from what is beneath it.
    pub const fn stroke() -> Rgba {
        rgba8(0xff, 0xff, 0xff, 20) // 8%
    }
}

/// Foreground white at an alpha in 0..=1.
pub fn white(alpha: f32) -> Rgba {
    Rgba {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: alpha.clamp(0.0, 1.0),
    }
}

/// Row background fills. All are the foreground at a low alpha, so a row
/// tints rather than changing colour.
pub struct Fill;

impl Fill {
    pub const HOVER: f32 = 0.06;
    pub const SUBTLE: f32 = 0.06;
    pub const MULTI_SELECTED: f32 = 0.08;
    pub const SELECTED: f32 = 0.10;

    pub fn selected(on: bool) -> Rgba {
        white(if on { Self::SELECTED } else { 0.0 })
    }

    pub fn subtle() -> Rgba {
        white(Self::SUBTLE)
    }
}

/// Semantic status colours.
pub struct Ink;

impl Ink {
    pub const ATTENTION: Rgba = rgba8(245, 166, 35, 0xff);
    pub const DANGER: Rgba = rgba8(245, 69, 58, 0xff);
    pub const FRESH: Rgba = rgba8(52, 199, 89, 0xff);
    pub const GENERIC_WORKING: Rgba = rgba8(138, 143, 152, 0xff);
    /// Claude's brand clay.
    pub const CLAY: Rgba = rgba8(217, 119, 87, 0xff);
    /// Gemini's brand blue, used only on its integration mark.
    pub const GEMINI_BLUE: Rgba = rgba8(78, 130, 238, 0xff);
    /// Codex's overprint teal.
    pub const TEAL: Rgba = rgba8(46, 204, 189, 0xff);
    /// Diff additions and removals. Same green/red family as the status ink,
    /// dimmed so a large patch does not vibrate.
    pub const ADDED: Rgba = rgba8(126, 211, 133, 0xff);
    pub const REMOVED: Rgba = rgba8(232, 118, 110, 0xff);
    /// Tint behind a changed line.
    pub const ADDED_BG: Rgba = rgba8(52, 199, 89, 28);
    pub const REMOVED_BG: Rgba = rgba8(245, 69, 58, 28);

    /// The colour a working run is drawn in, by engine.
    pub fn working(engine: &str) -> Rgba {
        match engine {
            "claude" => Self::CLAY,
            "codex" => Self::TEAL,
            _ => Self::GENERIC_WORKING,
        }
    }
}

/// Motion constants, ported from diri's `Motion`.
pub struct Motion;

impl Motion {
    pub const BREATHE: f64 = 2.6;
    pub const SWEEP_REV: f64 = 2.4;
    pub const PING_PERIOD: f64 = 1.8;
    pub const PING_PERIOD_RISK: f64 = 1.2;
    pub const TICK_HZ: u64 = 10;
}

/// Every periodic value evaluated from one absolute clock sample.
///
/// Ported from diri, including the detail that makes it work: the sample is
/// the **wall clock**, not a per-element timer. Every mark on screen therefore
/// shares a phase without any shared state, so a column of pulsing marks beats
/// together instead of shimmering out of step.
///
/// diri computes this but renders static marks, so a glyph never schedules a
/// frame. This shell already repaints at `TICK_HZ` to follow the daemon, so
/// the motion is free here — and a "needs you" mark that pulses is the whole
/// point of having a "needs you" state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimationPhase {
    pub breathe_scale: f32,
    pub needs_input_pulse: f32,
    pub needs_input_opacity: f32,
    pub sweep_turns: f32,
}

impl AnimationPhase {
    pub fn at(seconds: f64) -> Self {
        let wave = |period: f64| (seconds * std::f64::consts::TAU / period).sin() as f32;
        let needs_input_pulse = 0.5 + 0.5 * wave(Motion::PING_PERIOD);
        Self {
            breathe_scale: 1.0 + 0.055 * wave(Motion::BREATHE),
            needs_input_pulse,
            // A second 0.5→1 map, so the dimmest frame is still legible.
            needs_input_opacity: 0.5 + 0.5 * needs_input_pulse,
            sweep_turns: (seconds / Motion::SWEEP_REV).rem_euclid(1.0) as f32,
        }
    }

    /// The current phase.
    pub fn now() -> Self {
        Self::at(wall_clock_seconds())
    }
}

/// Absolute wall-clock sample shared by every glyph.
pub fn wall_clock_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// What a run is doing, in the terms a person actually cares about. Ported
/// from diri's `StatusState`: the question a glance must answer is "does this
/// want me?", so wanting-the-user is its own state and outranks progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Working,
    /// Blocked on the user. `destructive` means the ask carries risk.
    NeedsInput {
        destructive: bool,
    },
    /// Finished and not yet looked at.
    DoneUnseen,
    /// Settled; the user has seen it.
    IdleSeen,
    /// Never started, or ended without a result.
    None,
}

impl Status {
    /// Map a run state from the daemon.
    pub fn of_run(state: &str) -> Status {
        match state {
            "running" | "working" => Status::Working,
            "awaiting_approval" => Status::NeedsInput { destructive: false },
            "needs-you" => Status::NeedsInput { destructive: false },
            // A blocked run stopped on evidence the user must judge.
            "blocked" | "paused" => Status::NeedsInput { destructive: true },
            "succeeded" | "completed" => Status::DoneUnseen,
            "failed" | "cancelled" => Status::IdleSeen,
            _ => Status::None,
        }
    }

    /// Map a graph node state.
    pub fn of_node(state: &str) -> Status {
        match state {
            "running" | "verifying" => Status::Working,
            "blocked" => Status::NeedsInput { destructive: true },
            "succeeded" => Status::DoneUnseen,
            "failed" => Status::NeedsInput { destructive: true },
            "cancelled" => Status::IdleSeen,
            _ => Status::None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Status::Working => "Working",
            Status::NeedsInput { destructive: false } => "Needs you",
            Status::NeedsInput { destructive: true } => "Needs you · check it",
            Status::DoneUnseen => "Done",
            Status::IdleSeen => "Ended",
            Status::None => "Idle",
        }
    }

    /// The mark's colour. `engine` tints a working run with its brand.
    pub fn color(self, engine: &str) -> Rgba {
        match self {
            Status::Working => Ink::working(engine),
            Status::NeedsInput { destructive: false } => Ink::ATTENTION,
            Status::NeedsInput { destructive: true } => Ink::DANGER,
            Status::DoneUnseen => Ink::FRESH,
            Status::IdleSeen => white(0.42),
            Status::None => white(0.28),
        }
    }

    /// Whether the mark is filled. A settled run reads as an outline, so the
    /// eye lands on the runs that are live or waiting.
    pub fn filled(self) -> bool {
        !matches!(self, Status::IdleSeen | Status::None)
    }

    /// Opacity for this frame. Only the states that want something from the
    /// user move; work in progress breathes faintly, everything settled is
    /// perfectly still. Motion is a request for attention, so anything that
    /// moves without needing you is noise.
    pub fn opacity(self, phase: AnimationPhase) -> f32 {
        match self {
            Status::NeedsInput { .. } => phase.needs_input_opacity,
            Status::Working => 0.82 + 0.18 * phase.breathe_scale.clamp(0.9, 1.1),
            _ => 1.0,
        }
    }

    /// Size multiplier for this frame.
    pub fn scale(self, phase: AnimationPhase) -> f32 {
        match self {
            Status::Working => phase.breathe_scale,
            Status::NeedsInput { .. } => 0.94 + 0.12 * phase.needs_input_pulse,
            _ => 1.0,
        }
    }

    /// diri's `AttentionDot` rule: quiet states render nothing at all, so a
    /// rollup only ever shows what is actually asking for something.
    pub fn rollup_dot(self) -> Option<(f32, Rgba)> {
        match self {
            Status::NeedsInput { destructive: false } => Some((6.0, Ink::ATTENTION)),
            Status::NeedsInput { destructive: true } => Some((6.0, Ink::DANGER)),
            Status::DoneUnseen => Some((6.0, Ink::FRESH)),
            Status::Working => Some((5.0, white(0.54))),
            Status::IdleSeen | Status::None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_uses_true_black_with_only_two_elevations() {
        assert_eq!(Colors::BACKGROUND, rgba8(0, 0, 0, 0xff));
        assert_eq!(Colors::SURFACE, rgba8(0, 0, 0, 0xff));
        assert_eq!(Colors::RAISED, rgba8(10, 10, 10, 0xff));
        const {
            assert!(Colors::BACKGROUND.r <= Colors::SURFACE.r);
            assert!(Colors::SURFACE.r < Colors::RAISED.r);
        }
    }

    #[test]
    fn cockpit_controls_keep_the_approved_density() {
        assert_eq!(Metrics::TOOLBAR_CONTROL_SIZE, 26.0);
        assert_eq!(Metrics::TOOLBAR_CHIP_HEIGHT, 24.0);
        assert_eq!(Metrics::INSPECTOR_WIDTH, 379.0);
    }

    /// The one thing this vocabulary must get right: a run that wants the user
    /// never looks like one that is merely busy or already finished.
    #[test]
    fn needing_the_user_is_visually_distinct_from_every_other_state() {
        let attention = Status::NeedsInput { destructive: false }.color("codex");
        let danger = Status::NeedsInput { destructive: true }.color("codex");
        for other in [
            Status::Working.color("codex"),
            Status::DoneUnseen.color("codex"),
            Status::IdleSeen.color("codex"),
            Status::None.color("codex"),
        ] {
            assert_ne!(attention, other);
            assert_ne!(danger, other);
        }
        assert_ne!(attention, danger);
    }

    #[test]
    fn a_working_run_is_tinted_by_its_engine() {
        assert_eq!(Status::Working.color("claude"), Ink::CLAY);
        assert_eq!(Status::Working.color("codex"), Ink::TEAL);
        assert_eq!(Status::Working.color("unknown"), Ink::GENERIC_WORKING);
        // Engine tint applies only to work in progress.
        assert_eq!(
            Status::DoneUnseen.color("claude"),
            Status::DoneUnseen.color("codex")
        );
    }

    #[test]
    fn run_states_map_onto_the_status_vocabulary() {
        assert_eq!(Status::of_run("running"), Status::Working);
        assert_eq!(
            Status::of_run("awaiting_approval"),
            Status::NeedsInput { destructive: false }
        );
        assert_eq!(
            Status::of_run("blocked"),
            Status::NeedsInput { destructive: true }
        );
        assert_eq!(Status::of_run("succeeded"), Status::DoneUnseen);
        assert_eq!(Status::of_run("nonsense"), Status::None);
        // Settled runs read as outlines so live ones draw the eye.
        assert!(Status::Working.filled());
        assert!(!Status::IdleSeen.filled());
    }

    #[test]
    fn text_tone_is_alpha_over_one_foreground() {
        // Same hue, different weight — not a palette of greys.
        let primary = Colors::text(Surface::Content, Tone::Primary);
        let secondary = Colors::text(Surface::Content, Tone::Secondary);
        assert_eq!((primary.r, primary.g), (secondary.r, secondary.g));
        assert!(secondary.a < primary.a);
        // A sidebar's supporting text is firmer than content's.
        assert!(
            Colors::text(Surface::Sidebar, Tone::Secondary).a
                > Colors::text(Surface::Content, Tone::Secondary).a
        );
    }

    #[test]
    fn the_type_scale_separates_by_weight_not_by_size_sprawl() {
        let sizes: std::collections::BTreeSet<u32> = [
            Typo::META,
            Typo::SECTION_HEADER,
            Typo::ROW,
            Typo::ROW_EMPHASIZED,
            Typo::TITLE,
            Typo::DISPLAY_TITLE,
        ]
        .iter()
        .map(|t| t.size as u32)
        .collect();
        assert_eq!(sizes.len(), 3, "three sizes: {sizes:?}");
        // Same size, different weight, different role.
        assert_eq!(Typo::ROW.size, Typo::TITLE.size);
        const { assert!(Typo::TITLE.weight.0 > Typo::ROW.weight.0) };
    }
}
