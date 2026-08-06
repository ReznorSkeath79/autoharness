//! The full-page settings surface.
//!
//! bb's `SettingsView` (github.com/get-bb/bb, MIT, TypeScript/React) is the
//! design reference — a page that owns the whole window, sections you
//! navigate, and search that reaches every row. Only the shape is borrowed;
//! the code is ours. The rows themselves are the same typed
//! [`crate::UtilityOverlayRow`] vocabulary the old 720px dialog used, so
//! keyboard traversal, pointer hits, and VoiceOver all still agree on what is
//! actionable.

use crate::client::{EngineStatus, UiState};
use crate::icon::{Icon, IconName, IconSize};
use crate::navigation::OverlayState;
use crate::theme::{self, Colors, Fill, Ink, Metrics, Radius, Space, Surface, Tone, Typo};
use crate::{
    Shell, UtilityOverlayRow, components, layout, notify, row, row_action, settings, settings_row,
    toolbar, update, update_row, usage,
};
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, ElementId, MouseButton, Role, SharedString,
    StatefulInteractiveElement, div, ease_out_quint, px,
};

/// The settings buckets. Order here is presentation order, and the keyboard
/// traversal order across the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Integrations,
    Limits,
    Notifications,
    Updates,
    Layout,
    Usage,
}

impl SettingsSection {
    pub(crate) const ALL: [Self; 7] = [
        Self::General,
        Self::Integrations,
        Self::Limits,
        Self::Notifications,
        Self::Updates,
        Self::Layout,
        Self::Usage,
    ];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Integrations => "Integrations",
            Self::Limits => "Limits",
            Self::Notifications => "Notifications",
            Self::Updates => "Updates",
            Self::Layout => "Layout",
            Self::Usage => "Usage",
        }
    }
}

/// A leading identity mark on a row. Brand marks exist only for engines whose
/// glyph we actually have; everything else gets an honest monogram rather
/// than invented brand art.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowMark {
    Brand(IconName),
    Monogram(char),
}

/// Which mark identifies an engine, by manifest id or alias. Only marks
/// whose brand identity is verified get one — a wrong logo is worse than a
/// monogram, so the rest stay letters until their real mark is sourced.
pub(crate) fn engine_mark(engine_id: &str) -> RowMark {
    let key = engine_id.trim().to_ascii_lowercase();
    match key.as_str() {
        "claude" | "claude-code" => RowMark::Brand(IconName::BrandClaude),
        "codex" => RowMark::Brand(IconName::BrandOpenAi),
        "copilot" => RowMark::Brand(IconName::BrandCopilot),
        "cursor" => RowMark::Brand(IconName::BrandCursor),
        "gemini" => RowMark::Brand(IconName::BrandGemini),
        "kimi" => RowMark::Brand(IconName::BrandKimi),
        "opencode" => RowMark::Brand(IconName::BrandOpenCode),
        "pi" => RowMark::Brand(IconName::BrandPi),
        _ => RowMark::Monogram(
            key.chars()
                .find(char::is_ascii_alphanumeric)
                .map(|ch| ch.to_ascii_uppercase())
                .unwrap_or('·'),
        ),
    }
}

/// One truthful line about an integration's state. Never claims readiness the
/// daemon did not report.
fn engine_status_line(engine: &EngineStatus) -> String {
    let mut line = if engine.ready {
        "Ready".to_string()
    } else if !engine.installed {
        "Not installed".to_string()
    } else if let Some(problem) = engine.problems.first() {
        // A concrete problem explains more than a generic auth state.
        problem.clone()
    } else if engine.authenticated == Some(false) {
        "Installed · not signed in".to_string()
    } else {
        "Installed".to_string()
    };
    if let Some(version) = engine.version.as_deref() {
        line.push_str(" · ");
        line.push_str(version);
    }
    let offered = crate::client::offered_models(engine).len();
    if engine.ready && offered > 0 {
        line.push_str(&format!(" · {offered} models"));
    }
    line
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub(crate) struct SectionGroup {
    pub section: SettingsSection,
    pub rows: Vec<UtilityOverlayRow>,
}

/// Every section with every row, in page order. The single source both the
/// renderer and the keyboard action list are derived from.
pub(crate) fn groups(state: &UiState, layout: layout::ShellLayout) -> Vec<SectionGroup> {
    use settings::{SettingsAction, SettingsBoolField, SettingsNumberField, on_off};

    let values = &state.settings.values;
    let general = vec![
        settings_row(
            "Default engine",
            values.default_engine.as_str(),
            SettingsAction::CycleEngine,
        ),
        settings_row(
            "Route mode",
            values.default_route_mode.as_str(),
            SettingsAction::CycleRouteMode,
        ),
        settings_row(
            "History scan",
            on_off(values.automatic_history_scan),
            SettingsAction::Toggle(SettingsBoolField::AutomaticHistoryScan),
        ),
        settings_row(
            "Confirm destructive actions",
            on_off(values.confirm_destructive_actions),
            SettingsAction::Toggle(SettingsBoolField::ConfirmDestructiveActions),
        ),
        row("Selected engine", state.engine.clone(), None),
        row(
            "Check command",
            state
                .check_command
                .clone()
                .unwrap_or_else(|| "No check configured".into()),
            None,
        ),
    ];

    let integrations = if state.engines.is_empty() {
        vec![row(
            "Engines",
            "Waiting for the daemon to report engines",
            None,
        )]
    } else {
        // What you can use today first, with its real state; everything
        // gated after it, saying "Coming soon" rather than pretending an
        // install state that cannot be acted on.
        let (available, gated): (Vec<_>, Vec<_>) = state
            .engines
            .iter()
            .partition(|engine| crate::client::engine_generally_available(&engine.name));
        let mut gated: Vec<&EngineStatus> = gated;
        gated.sort_by(|a, b| a.name.cmp(&b.name));
        available
            .into_iter()
            .map(|engine| (engine, engine_status_line(engine)))
            .chain(
                gated
                    .into_iter()
                    .map(|engine| (engine, "Coming soon".to_string())),
            )
            .map(|(engine, status)| {
                let mut engine_row = row(title_case(&engine.name), status, None);
                engine_row.mark = Some(engine_mark(&engine.name));
                engine_row
            })
            .collect()
    };

    let limits = vec![
        settings_row(
            "Active runs",
            values.max_active_runs.to_string(),
            SettingsAction::Increment(SettingsNumberField::MaxActiveRuns),
        ),
        settings_row(
            "Max workers",
            values.max_parallel_workers.to_string(),
            SettingsAction::Increment(SettingsNumberField::MaxParallelWorkers),
        ),
        settings_row(
            "Max graph nodes",
            values.max_graph_nodes.to_string(),
            SettingsAction::Increment(SettingsNumberField::MaxGraphNodes),
        ),
        settings_row(
            "Wall time",
            format!("{} min", values.default_wall_time_minutes),
            SettingsAction::Increment(SettingsNumberField::DefaultWallTimeMinutes),
        ),
        settings_row(
            "Retention",
            format!("{} days", values.retention_days),
            SettingsAction::Increment(SettingsNumberField::RetentionDays),
        ),
    ];

    let notifications = vec![
        settings_row(
            "Notifications",
            on_off(values.notifications_enabled),
            SettingsAction::Toggle(SettingsBoolField::NotificationsEnabled),
        ),
        settings_row(
            "Sounds",
            if notify::sound_supported() {
                on_off(values.sounds_enabled).to_string()
            } else {
                format!(
                    "{} · unavailable on this build",
                    on_off(values.sounds_enabled)
                )
            },
            SettingsAction::Toggle(SettingsBoolField::SoundsEnabled),
        ),
    ];

    let mut updates = vec![
        settings_row(
            "Update checks",
            on_off(values.automatic_update_checks),
            SettingsAction::Toggle(SettingsBoolField::AutomaticUpdateChecks),
        ),
        update_row(
            "Check for updates",
            state.update_status.summary(),
            update::UpdateAction::CheckNow,
        ),
    ];
    if let update::UpdateStatus::Available { version, .. } = &state.update_status {
        updates.push(update_row(
            format!("Install {version}"),
            "Closes AutoHarness, atomically swaps, and relaunches",
            update::UpdateAction::InstallVerified,
        ));
    } else {
        updates.push(row(
            "Update install",
            update::InstallGate::current().reason(),
            None,
        ));
    }

    let layout_rows = vec![
        row(
            "Sidebar pane",
            pane_state(layout.sidebar_open),
            Some(toolbar::ToolbarAction::ToggleSidebar),
        ),
        row(
            "Execution pane",
            pane_state(layout.execution_open),
            Some(toolbar::ToolbarAction::ToggleExecution),
        ),
        row(
            "Inspector pane",
            pane_state(layout.inspector_open),
            Some(toolbar::ToolbarAction::ToggleInspector),
        ),
        row(
            "Reset layout",
            "Local only",
            Some(toolbar::ToolbarAction::ResetLayout),
        ),
    ];

    let usage_rows = usage::usage_rows(state)
        .into_iter()
        .map(|usage| row(usage.label, usage.value, None))
        .collect();

    vec![
        SectionGroup {
            section: SettingsSection::General,
            rows: general,
        },
        SectionGroup {
            section: SettingsSection::Integrations,
            rows: integrations,
        },
        SectionGroup {
            section: SettingsSection::Limits,
            rows: limits,
        },
        SectionGroup {
            section: SettingsSection::Notifications,
            rows: notifications,
        },
        SectionGroup {
            section: SettingsSection::Updates,
            rows: updates,
        },
        SectionGroup {
            section: SettingsSection::Layout,
            rows: layout_rows,
        },
        SectionGroup {
            section: SettingsSection::Usage,
            rows: usage_rows,
        },
    ]
}

fn pane_state(open: bool) -> &'static str {
    if open { "Open" } else { "Closed" }
}

/// What the page shows for a section pick and a search query.
///
/// A non-empty query searches EVERY section — that is what a settings search
/// is for — so the section filter only applies while the query is empty.
pub(crate) fn visible_groups(
    state: &UiState,
    layout: layout::ShellLayout,
    section: Option<SettingsSection>,
    query: &str,
) -> Vec<SectionGroup> {
    let query = query.trim().to_lowercase();
    groups(state, layout)
        .into_iter()
        .filter(|group| {
            if query.is_empty() {
                section.is_none() || section == Some(group.section)
            } else {
                true
            }
        })
        .map(|group| {
            let section_hit = group.section.title().to_lowercase().contains(&query);
            let rows = if query.is_empty() || section_hit {
                group.rows
            } else {
                group
                    .rows
                    .into_iter()
                    .filter(|row| {
                        row.label.to_lowercase().contains(&query)
                            || row.value.to_lowercase().contains(&query)
                    })
                    .collect()
            };
            SectionGroup {
                section: group.section,
                rows,
            }
        })
        .filter(|group| !group.rows.is_empty())
        .collect()
}

/// The flat row list in exactly the order the page renders it, so keyboard
/// traversal and pointer targets can never disagree.
pub(crate) fn visible_rows(
    state: &UiState,
    layout: layout::ShellLayout,
    section: Option<SettingsSection>,
    query: &str,
) -> Vec<UtilityOverlayRow> {
    visible_groups(state, layout, section, query)
        .into_iter()
        .flat_map(|group| group.rows)
        .collect()
}

fn mark_badge(mark: RowMark) -> AnyElement {
    const BADGE: f32 = 24.0;
    let (badge_fill, tint) = match mark {
        RowMark::Brand(IconName::BrandClaude) => (
            gpui::Rgba {
                a: 0.14,
                ..Ink::CLAY
            },
            Ink::CLAY,
        ),
        RowMark::Brand(IconName::BrandGemini) => (theme::white(Fill::SUBTLE), Ink::GEMINI_BLUE),
        _ => (theme::white(Fill::SUBTLE), theme::white(0.85)),
    };
    let badge = div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(BADGE))
        .rounded(px(Radius::BADGE))
        .bg(badge_fill);
    match mark {
        RowMark::Brand(icon) => badge
            .child(Icon::new(icon, IconSize::REGULAR, tint))
            .into_any_element(),
        RowMark::Monogram(letter) => badge
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .font_weight(Typo::TITLE.weight)
                    .text_color(Colors::text(Surface::Content, Tone::Secondary))
                    .child(SharedString::from(letter.to_string())),
            )
            .into_any_element(),
    }
}

/// The whole settings page: header with back, section navigation, and search;
/// then the visible sections as cards.
/// The page's transient navigation state, owned by [`Shell`].
pub(crate) struct PageNav<'a> {
    pub section: Option<SettingsSection>,
    pub nav_open: bool,
    /// The raw filter text, and the same text with the caret for display.
    pub query: &'a str,
    pub query_display: &'a str,
}

pub(crate) fn view(
    overlay: &OverlayState,
    nav: PageNav<'_>,
    state: &UiState,
    layout: layout::ShellLayout,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let PageNav {
        section,
        nav_open,
        query,
        query_display,
    } = nav;
    let groups = visible_groups(state, layout, section, query);
    let mut selectable_index = 0usize;
    let nav_label = match section {
        Some(section) => section.title(),
        None => "All sections",
    };

    let header = div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(Space::ROW_H))
        .h(px(Metrics::TITLE_BAR))
        .px(px(Metrics::TOOLBAR_EDGE_INSET))
        .border_b_1()
        .border_color(Colors::stroke())
        .child(
            components::compact_control("")
                .id("settings-page-back")
                .role(Role::Button)
                .aria_label("Back")
                .hover(|control| control.bg(theme::white(Fill::HOVER)))
                .cursor_pointer()
                .on_click(cx.listener(|shell, _, _, cx| {
                    shell.dismiss_utility_overlay(cx);
                }))
                .child(Icon::new(
                    IconName::ArrowLeft,
                    IconSize::REGULAR,
                    Colors::text(Surface::Content, Tone::Secondary),
                )),
        )
        .child(
            div()
                .text_size(px(Typo::DISPLAY_TITLE.size))
                .font_weight(Typo::DISPLAY_TITLE.weight)
                .child("Settings"),
        )
        .child(
            // The section dropdown anchors its menu to this wrapper.
            div()
                .relative()
                .child(
                    components::compact_control(nav_label)
                        .id("settings-page-section-nav")
                        .role(Role::Button)
                        .aria_label(format!("Sections: {nav_label}"))
                        .hover(|control| control.bg(theme::white(Fill::HOVER)))
                        .cursor_pointer()
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.settings_nav_open = !shell.settings_nav_open;
                            cx.notify();
                        }))
                        .child(Icon::new(
                            IconName::ChevronDown,
                            IconSize::COMPACT,
                            Colors::text(Surface::Content, Tone::Tertiary),
                        )),
                )
                .when(nav_open, |wrapper| {
                    wrapper.child(
                        div()
                            .absolute()
                            .top(px(28.0))
                            .left(px(0.0))
                            .w(px(180.0))
                            .flex()
                            .flex_col()
                            .py(px(4.0))
                            .bg(Colors::RAISED)
                            .border_1()
                            .border_color(Colors::stroke())
                            .rounded(px(Radius::CARD))
                            .shadow_lg()
                            .child(section_menu_item(
                                "All sections",
                                section.is_none(),
                                None,
                                cx,
                            ))
                            .children(SettingsSection::ALL.into_iter().map(|entry| {
                                section_menu_item(
                                    entry.title(),
                                    section == Some(entry),
                                    Some(entry),
                                    cx,
                                )
                            })),
                    )
                }),
        )
        .child(div().flex_1())
        .child(
            div()
                .id("settings-page-search")
                .role(Role::SearchInput)
                .aria_label("Search settings")
                .aria_placeholder("Search settings")
                .aria_value(query.to_string())
                .flex()
                .items_center()
                .gap(px(6.0))
                .w(px(260.0))
                .h(px(Metrics::TOOLBAR_CHIP_HEIGHT))
                .px(px(Space::ROW_H))
                .rounded(px(Radius::ROW))
                .bg(theme::white(Fill::SUBTLE))
                .border_1()
                .border_color(Colors::stroke())
                .child(Icon::new(
                    IconName::Search,
                    IconSize::COMPACT,
                    Colors::text(Surface::Content, Tone::Tertiary),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(Typo::ROW.size))
                        .when(query.is_empty(), |field| {
                            field.text_color(Colors::text(Surface::Content, Tone::Tertiary))
                        })
                        .when(!query.is_empty(), |field| {
                            field.text_color(Colors::text(Surface::Content, Tone::Primary))
                        })
                        .child(SharedString::from(if query.is_empty() {
                            "Search settings".to_string()
                        } else {
                            query_display.to_string()
                        })),
                ),
        );

    let status_line = if state.settings.loading {
        Some(("Loading persisted settings…".to_string(), false))
    } else {
        state
            .settings
            .error
            .as_ref()
            .map(|error| (error.clone(), true))
    };

    let body = div().flex_1().min_h(px(0.0)).flex().justify_center().child(
        div()
            .id("settings-page-scroll")
            .w_full()
            .max_w(px(760.0))
            .overflow_y_scroll()
            .px(px(Space::INDENT * 2.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .children(status_line.map(|(text, is_error)| {
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(if is_error {
                        Ink::DANGER
                    } else {
                        Colors::text(Surface::Content, Tone::Tertiary)
                    })
                    .child(SharedString::from(text))
            }))
            .when(groups.is_empty(), |body| {
                body.child(
                    div()
                        .py(px(24.0))
                        .text_size(px(Typo::ROW.size))
                        .text_color(Colors::text(Surface::Content, Tone::Secondary))
                        .child(SharedString::from(format!(
                            "Nothing matches \"{}\"",
                            query.trim()
                        ))),
                )
            })
            .children(groups.into_iter().map(|group| {
                let card_rows = group.rows.into_iter().enumerate().map(|(row_index, row)| {
                    let action = row_action(&row);
                    let settings_action = row.settings_action;
                    let actionable = action.is_some();
                    let selected = actionable && {
                        let is_selected = selectable_index == overlay.selected;
                        selectable_index += 1;
                        is_selected
                    };
                    let accessible_label = format!("{}: {}", row.label, row.value);
                    div()
                        .id(ElementId::Name(
                            format!("settings-row-{}-{row_index}", group.section.title()).into(),
                        ))
                        .role(if actionable {
                            Role::Button
                        } else {
                            Role::Label
                        })
                        .aria_label(accessible_label)
                        .when(selected, |row| row.aria_active_descendant())
                        .flex()
                        .items_center()
                        .gap(px(Space::ROW_H))
                        .px(px(Space::INDENT))
                        .h(px(40.0))
                        .border_b_1()
                        .border_color(theme::white(0.05))
                        .when(selected, |row| row.bg(Fill::selected(true)))
                        .when_some(action, |row, action| {
                            row.cursor_pointer()
                                .hover(|row| row.bg(theme::white(Fill::HOVER)))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.apply_utility_overlay_action(action.clone());
                                    cx.notify();
                                }))
                        })
                        .when_some(settings_action, |row, action| {
                            // Right click walks a numeric setting back
                            // down, mirroring Left arrow.
                            row.on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |shell, _, _, cx| {
                                    shell.settings_action(action.reversed());
                                    cx.notify();
                                }),
                            )
                        })
                        .children(row.mark.map(mark_badge))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(Typo::ROW.size))
                                .text_color(Colors::text(Surface::Content, Tone::Primary))
                                .child(SharedString::from(row.label)),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .min_w(px(0.0))
                                .text_size(px(Typo::ROW.size))
                                .text_color(Colors::text(Surface::Content, Tone::Secondary))
                                .child(SharedString::from(row.value)),
                        )
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(Typo::SECTION_HEADER.size))
                            .font_weight(Typo::SECTION_HEADER.weight)
                            .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                            .child(group.section.title()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .bg(Colors::RAISED)
                            .border_1()
                            .border_color(Colors::stroke())
                            .rounded(px(Radius::CARD))
                            .overflow_hidden()
                            .children(card_rows),
                    )
            })),
    );

    div()
        .id("settings-page")
        .role(Role::Dialog)
        .aria_label("Settings")
        .aria_description(
            "Type to search, use arrows to move, Enter to activate, Escape to go back",
        )
        .absolute()
        .inset_0()
        .bg(Colors::BACKGROUND)
        .child(
            // The background lands instantly so the cockpit never bleeds
            // through; only the content settles in, diri's entry recipe.
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(header)
                .child(body)
                .with_animation(
                    ElementId::Name("settings-page-entry".into()),
                    Animation::new(crate::motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
                    |content, delta| {
                        content
                            .relative()
                            .top(px((1.0 - delta) * 8.0))
                            .opacity(crate::motion::overlay_opacity(delta))
                    },
                ),
        )
        .into_any_element()
}

fn section_menu_item(
    label: &'static str,
    current: bool,
    section: Option<SettingsSection>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    div()
        .id(ElementId::Name(format!("settings-section-{label}").into()))
        .role(Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .gap(px(Space::ROW_H))
        .px(px(Space::INDENT))
        .py(px(5.0))
        .cursor_pointer()
        .hover(|item| item.bg(theme::white(Fill::HOVER)))
        .when(current, |item| item.bg(Fill::selected(true)))
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.settings_section = section;
            shell.settings_nav_open = false;
            if let Some(overlay) = &mut shell.utility_overlay {
                overlay.selected = 0;
            }
            cx.notify();
        }))
        .child(
            div()
                .text_size(px(Typo::ROW.size))
                .text_color(Colors::text(Surface::Content, Tone::Primary))
                .child(label),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(name: &str, ready: bool, installed: bool) -> EngineStatus {
        EngineStatus {
            name: name.into(),
            ready,
            installed,
            authenticated: Some(ready),
            version: ready.then(|| "1.2.3".into()),
            problems: Vec::new(),
            models: Vec::new(),
            model_load_error: None,
        }
    }

    #[test]
    fn known_engines_get_brand_marks_and_the_rest_get_monograms() {
        assert_eq!(engine_mark("claude"), RowMark::Brand(IconName::BrandClaude));
        assert_eq!(
            engine_mark("claude-code"),
            RowMark::Brand(IconName::BrandClaude)
        );
        assert_eq!(engine_mark("codex"), RowMark::Brand(IconName::BrandOpenAi));
        assert_eq!(engine_mark("cursor"), RowMark::Brand(IconName::BrandCursor));
        assert_eq!(engine_mark("gemini"), RowMark::Brand(IconName::BrandGemini));
        assert_eq!(engine_mark("kimi"), RowMark::Brand(IconName::BrandKimi));
        assert_eq!(
            engine_mark("opencode"),
            RowMark::Brand(IconName::BrandOpenCode)
        );
        assert_eq!(engine_mark("pi"), RowMark::Brand(IconName::BrandPi));
        assert_eq!(
            engine_mark("copilot"),
            RowMark::Brand(IconName::BrandCopilot)
        );
        // No verified mark yet: an honest letter, never a wrong logo.
        assert_eq!(engine_mark("grok"), RowMark::Monogram('G'));
        assert_eq!(engine_mark("droid"), RowMark::Monogram('D'));
        assert_eq!(engine_mark(""), RowMark::Monogram('·'));
    }

    #[test]
    fn every_registered_engine_appears_in_integrations_with_a_mark() {
        let mut state = crate::preview::populated();
        state.engines = vec![
            engine("pi", false, false),
            engine("claude", true, true),
            engine("grok", false, true),
        ];
        let groups = groups(&state, layout::ShellLayout::default());
        let integrations = groups
            .iter()
            .find(|group| group.section == SettingsSection::Integrations)
            .expect("integrations section exists");

        assert_eq!(integrations.rows.len(), 3);
        assert!(integrations.rows.iter().all(|row| row.mark.is_some()));
        // Available engines lead with their real state; gated ones follow
        // alphabetically and say only "Coming soon" — an install state you
        // cannot act on is noise.
        let claude = &integrations.rows[0];
        assert_eq!(claude.label, "Claude");
        assert!(claude.value.starts_with("Ready"));
        assert!(claude.value.contains("1.2.3"));
        assert_eq!(integrations.rows[1].label, "Grok");
        assert_eq!(integrations.rows[1].value, "Coming soon");
        assert_eq!(integrations.rows[2].label, "Pi");
        assert_eq!(integrations.rows[2].value, "Coming soon");
    }

    #[test]
    fn integration_status_never_reports_readiness_the_daemon_did_not() {
        let mut signed_out = engine("cursor", false, true);
        signed_out.authenticated = Some(false);
        assert_eq!(engine_status_line(&signed_out), "Installed · not signed in");

        let mut broken = engine("droid", false, true);
        broken.problems = vec!["binary exits 1".into()];
        assert_eq!(engine_status_line(&broken), "binary exits 1");
    }

    #[test]
    fn search_reaches_every_section_and_ignores_the_section_filter() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();

        // "retention" lives in Limits; searching while General is picked must
        // still find it.
        let found = visible_rows(&state, layout, Some(SettingsSection::General), "retention");
        assert!(found.iter().any(|row| row.label == "Retention"));

        // A section-title hit surfaces the whole section.
        let by_title = visible_groups(&state, layout, None, "integrations");
        assert_eq!(by_title.len(), 1);
        assert_eq!(by_title[0].section, SettingsSection::Integrations);

        // Nonsense matches nothing, and says so with an empty list rather
        // than a stale page.
        assert!(visible_rows(&state, layout, None, "zzzznothing").is_empty());
    }

    #[test]
    fn the_section_filter_shows_exactly_that_section_when_not_searching() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();
        let groups = visible_groups(&state, layout, Some(SettingsSection::Limits), "");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].section, SettingsSection::Limits);
        assert_eq!(groups[0].rows.len(), 5);
    }

    #[test]
    fn every_persisted_setting_is_reachable_from_the_full_page() {
        use crate::settings::{SettingsAction, SettingsBoolField, SettingsNumberField};

        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();
        let rows = visible_rows(&state, layout, None, "");
        let actions = crate::utility_overlay_keyboard_actions(&rows);

        for expected in [
            crate::UtilityOverlayAction::Settings(SettingsAction::CycleEngine),
            crate::UtilityOverlayAction::Settings(SettingsAction::CycleRouteMode),
            crate::UtilityOverlayAction::Settings(SettingsAction::Increment(
                SettingsNumberField::MaxActiveRuns,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Increment(
                SettingsNumberField::MaxParallelWorkers,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Increment(
                SettingsNumberField::MaxGraphNodes,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Increment(
                SettingsNumberField::DefaultWallTimeMinutes,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Increment(
                SettingsNumberField::RetentionDays,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Toggle(
                SettingsBoolField::AutomaticHistoryScan,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Toggle(
                SettingsBoolField::NotificationsEnabled,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Toggle(
                SettingsBoolField::SoundsEnabled,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Toggle(
                SettingsBoolField::AutomaticUpdateChecks,
            )),
            crate::UtilityOverlayAction::Settings(SettingsAction::Toggle(
                SettingsBoolField::ConfirmDestructiveActions,
            )),
            crate::UtilityOverlayAction::Update(update::UpdateAction::CheckNow),
        ] {
            assert!(
                actions.contains(&expected),
                "{expected:?} must be reachable"
            );
        }
    }
}
