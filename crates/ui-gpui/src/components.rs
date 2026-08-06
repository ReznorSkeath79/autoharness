//! Shared GPUI recipes for the shell chrome.

use gpui::prelude::*;
use gpui::{Div, IntoElement, SharedString, div, px};

use crate::theme::{self, Colors, Fill, Metrics, Radius, Space, Status, Surface, Tone, Typo};

/// A pane's section header.
pub fn section(label: &str) -> impl IntoElement + use<> {
    div()
        .px(px(Space::INDENT))
        .pt(px(Space::INSET))
        .pb(px(Space::ROW_H))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(SharedString::from(label.to_uppercase()))
}

/// Persistent shell panel material.
pub fn panel() -> Div {
    div().flex().flex_col().bg(Colors::SURFACE).rounded(px(0.0))
}

/// The one recipe every floating surface uses: popovers, pickers, sheets.
///
/// Adapted from diri's shared floating-surface recipe. Each overlay here used
/// to inline its own radius, border, fill and shadow, which is how two menus
/// end up a pixel apart and a third ends up a different shade of black. One
/// recipe means a new surface cannot be subtly misaligned with the others, and
/// changing the material changes all of them.
pub fn floating_surface() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(Radius::PANEL))
        .border_1()
        .border_color(Colors::stroke())
        .bg(Colors::RAISED)
        .shadow_lg()
}

/// Dense toolbar control chip.
pub fn compact_control(label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(Metrics::TOOLBAR_CHIP_HEIGHT))
        .min_w(px(Metrics::TOOLBAR_CONTROL_SIZE))
        .px(px(Space::ROW_H))
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle())
        .border_1()
        .border_color(Colors::stroke())
        .text_size(px(Typo::META.size))
        .font_weight(Typo::META.weight)
        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
        .child(label.into())
}

/// diri's status mark: filled for live or fresh work, a ring once settled, so
/// the eye lands on what still wants attention.
pub fn status_mark(status: Status, engine: &str) -> impl IntoElement + use<> {
    // One wall-clock sample per frame, shared by every mark, so a column of
    // pulsing marks beats together instead of shimmering out of step.
    let phase = theme::AnimationPhase::now();
    let mut color = status.color(engine);
    color.a *= status.opacity(phase);
    let size = 8.0 * status.scale(phase);

    let dot = div().size(px(size)).rounded_full();
    div()
        .flex()
        .flex_none()
        .w(px(14.0))
        .justify_center()
        .items_center()
        .child(if status.filled() {
            dot.bg(color)
        } else {
            dot.border_1().border_color(color)
        })
}
