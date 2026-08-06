use std::time::{Duration, Instant};

pub const OVERLAY_ENTRY: Duration = Duration::from_millis(160);
pub const TAB_TRANSITION: Duration = Duration::from_millis(190);
pub const SEAM_SLIDE: Duration = Duration::from_millis(140);

const SPRING_STIFFNESS: f32 = 7.0;

/// A normalized, critically damped spring. It is deliberately monotonic so
/// panes never overshoot their final seam while content is being reflowed.
pub fn spring_settle(delta: f32) -> f32 {
    let delta = delta.clamp(0.0, 1.0);
    if delta == 0.0 || delta == 1.0 {
        return delta;
    }
    let response = 1.0 - (1.0 + SPRING_STIFFNESS * delta) * (-SPRING_STIFFNESS * delta).exp();
    let settled = 1.0 - (1.0 + SPRING_STIFFNESS) * (-SPRING_STIFFNESS).exp();
    (response / settled).clamp(0.0, 1.0)
}

pub fn overlay_opacity(delta: f32) -> f32 {
    0.76 + 0.24 * delta.clamp(0.0, 1.0)
}

pub fn tab_opacity(delta: f32) -> f32 {
    0.70 + 0.30 * delta.clamp(0.0, 1.0)
}

pub fn tab_offset(direction: f32, delta: f32, reduce_motion: bool) -> f32 {
    if reduce_motion {
        0.0
    } else {
        direction.signum() * (1.0 - delta.clamp(0.0, 1.0)) * 8.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SeamSlide {
    from: f32,
    started_at: Instant,
}

impl SeamSlide {
    pub fn begin(from: f32, started_at: Instant) -> Self {
        Self { from, started_at }
    }

    pub fn value_at(self, to: f32, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.started_at);
        let delta = (elapsed.as_secs_f32() / SEAM_SLIDE.as_secs_f32()).clamp(0.0, 1.0);
        self.from + (to - self.from) * spring_settle(delta)
    }

    pub fn is_done(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= SEAM_SLIDE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diri_motion_tokens_are_exact_and_the_spring_never_overshoots() {
        assert_eq!(OVERLAY_ENTRY, std::time::Duration::from_millis(160));
        assert_eq!(TAB_TRANSITION, std::time::Duration::from_millis(190));
        assert_eq!(SEAM_SLIDE, std::time::Duration::from_millis(140));
        assert_eq!(spring_settle(0.0), 0.0);
        assert_eq!(spring_settle(1.0), 1.0);
        let mut previous = 0.0;
        for step in 0..=100 {
            let value = spring_settle(step as f32 / 100.0);
            assert!((0.0..=1.0).contains(&value));
            assert!(value >= previous, "spring moved backwards at {step}");
            previous = value;
        }
        assert!(spring_settle(0.5) > 0.8);
    }

    #[test]
    fn reduced_motion_keeps_opacity_feedback_but_removes_spatial_travel() {
        assert_eq!(tab_offset(1.0, 0.0, false), 8.0);
        assert_eq!(tab_offset(-1.0, 0.0, false), -8.0);
        assert_eq!(tab_offset(1.0, 0.0, true), 0.0);
        assert_eq!(tab_opacity(0.0), 0.70);
        assert_eq!(tab_opacity(1.0), 1.0);
        assert_eq!(overlay_opacity(0.0), 0.76);
        assert_eq!(overlay_opacity(1.0), 1.0);
    }

    #[test]
    fn seam_slide_finishes_exactly_at_its_target() {
        let started_at = std::time::Instant::now();
        let slide = SeamSlide::begin(32.0, started_at);
        assert_eq!(slide.value_at(320.0, started_at), 32.0);
        assert_eq!(slide.value_at(320.0, started_at + SEAM_SLIDE), 320.0);
        assert!(slide.is_done(started_at + SEAM_SLIDE));
        assert_eq!(slide.value_at(400.0, started_at + SEAM_SLIDE), 400.0);
    }
}
