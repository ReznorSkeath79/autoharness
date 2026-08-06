//! Choosing what runs the next objective: engine, model and effort, together.
//!
//! These were three controls in two places. The engine sat in the toolbar as a
//! pair of chips, the model and its effort in a popover anchored to the
//! composer, and the toolbar pair could not show a third engine at all once an
//! engine became a manifest id rather than an enum variant. Meanwhile the
//! three are one decision — a model belongs to an engine, an effort belongs to
//! a model — and splitting it across two surfaces meant the answer to "what
//! will run this?" was never in one place.
//!
//! This is that one place. Every combination the daemon reports as runnable is
//! a row, filtered by typing, using the same matcher the command palette uses.

use crate::client::UiState;
use crate::fuzzy;

/// One runnable combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutionChoice {
    pub engine: String,
    /// Empty means the provider's own default model.
    pub model: String,
    pub model_label: String,
    pub description: String,
    /// The effort this row would apply: the model's default, or the current
    /// one when this row is already selected.
    pub effort: String,
    /// Every effort this model accepts, for the chips on the selected row.
    pub efforts: Vec<String>,
    /// Whether this is what the next objective would use right now.
    pub current: bool,
    /// The provider's default model for this engine.
    pub provider_default: bool,
    /// Why this combination cannot be picked, if it cannot.
    pub unavailable: Option<String>,
}

impl ExecutionChoice {
    /// What the row is matched against when filtering. Includes the engine so
    /// typing "claude" narrows to Claude's models without leaving the picker.
    pub fn haystack(&self) -> String {
        let mut text = format!("{} {}", self.engine, self.model_label);
        if !self.effort.is_empty() {
            text.push(' ');
            text.push_str(&self.effort);
        }
        text
    }
}

/// Every combination the daemon reports, current first, unavailable last.
///
/// An engine that is not ready is still listed, with its reason, for the same
/// reason the new-run picker lists them: hiding it turns "why can I not use
/// Claude here" into a question with no answer on screen.
pub(crate) fn choices(state: &UiState) -> Vec<ExecutionChoice> {
    let mut out = Vec::new();
    for engine in &state.engines {
        // A gated engine is one row saying "Coming soon", never a catalog.
        // Expanding its models turned this picker into a forty-row haystack
        // of things that cannot be picked.
        if !crate::client::engine_generally_available(&engine.name) {
            out.push(ExecutionChoice {
                engine: engine.name.clone(),
                model: String::new(),
                model_label: title_case(&engine.name),
                description: "Coming soon".into(),
                effort: String::new(),
                efforts: Vec::new(),
                current: false,
                provider_default: false,
                unavailable: Some("Coming soon".into()),
            });
            continue;
        }
        let unavailable = (!engine.ready).then(|| {
            engine
                .problems
                .first()
                .cloned()
                .unwrap_or_else(|| "not available".into())
        });

        // Only the provider's own models; routed third-party entries in the
        // catalog are not offered (see `client::offered_models`).
        let models = crate::client::offered_models(engine);
        if models.is_empty() {
            // No catalog is not "no models": it means the provider decides.
            // Saying so is better than an empty engine the user cannot pick.
            out.push(ExecutionChoice {
                engine: engine.name.clone(),
                model: String::new(),
                model_label: format!("{} default", title_case(&engine.name)),
                description: engine
                    .model_load_error
                    .clone()
                    .map(|error| format!("catalog unavailable: {error}"))
                    .unwrap_or_else(|| "whatever the installed CLI is set to".into()),
                effort: String::new(),
                efforts: Vec::new(),
                current: state.engine == engine.name && state.model.is_none(),
                provider_default: true,
                unavailable: unavailable.clone(),
            });
            continue;
        }

        for model in models {
            // ONE row per model. Listing every model-and-effort pair turned a
            // catalog of 36 models into 104 rows with "Claude default"
            // repeated five times — a list that long is not a chooser, it is
            // a haystack. Effort is picked on the row that is selected.
            let current =
                state.engine == engine.name && state.model.as_deref().unwrap_or("") == model.id;
            let effort = if current {
                state
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| model.default_reasoning_effort.clone().unwrap_or_default())
            } else {
                model.default_reasoning_effort.clone().unwrap_or_default()
            };
            out.push(ExecutionChoice {
                engine: engine.name.clone(),
                model: model.id.clone(),
                model_label: model.display_name.clone(),
                description: model.description.clone(),
                effort,
                efforts: model.reasoning_efforts.clone(),
                current,
                provider_default: model.is_default,
                unavailable: unavailable.clone(),
            });
        }
    }

    out.sort_by(|a, b| {
        // What is selected now, then what can run, then everything else. A
        // picker that buries the current choice makes you hunt for where you
        // already are.
        b.current
            .cmp(&a.current)
            .then_with(|| a.unavailable.is_some().cmp(&b.unavailable.is_some()))
            .then_with(|| b.provider_default.cmp(&a.provider_default))
            .then_with(|| a.engine.cmp(&b.engine))
            .then_with(|| a.model_label.cmp(&b.model_label))
    });
    out
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// One engine tab at the top of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerTab {
    pub engine: String,
    pub active: bool,
    /// The engine is listed but cannot run; its panel explains why.
    pub unavailable: Option<String>,
}

/// Everything the composer picker renders, already resolved: which engine
/// tab is active, that engine's models (query-filtered), the reasoning
/// levels of the model the effort section applies to, and how many engines
/// are gated behind "coming soon".
///
/// The shape follows bb's ModelReasoningPicker — provider tabs, a Model
/// section, a Reasoning section, one popover — with diri's density. One
/// gated engine is one number in a footer, never sixteen rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerView {
    pub tabs: Vec<PickerTab>,
    pub active_engine: String,
    /// Why the active engine cannot run, when it cannot. Its models still
    /// list — choosing one is how you get sent to fix sign-in — but the
    /// reason leads the panel.
    pub active_unavailable: Option<String>,
    pub models: Vec<ExecutionChoice>,
    /// The choice whose reasoning levels the effort section shows: the
    /// current selection when it belongs to the active engine, else the
    /// active engine's default model.
    pub effort_target: Option<ExecutionChoice>,
    pub coming_soon: usize,
}

pub(crate) fn picker_view(state: &UiState, tab: Option<&str>, query: &str) -> PickerView {
    let all = choices(state);
    let coming_soon = state
        .engines
        .iter()
        .filter(|engine| !crate::client::engine_generally_available(&engine.name))
        .count();

    let available: Vec<&crate::client::EngineStatus> = state
        .engines
        .iter()
        .filter(|engine| crate::client::engine_generally_available(&engine.name))
        .collect();
    // The active tab: an explicit pick, else the engine of the current
    // selection, else the first available engine.
    let active_engine = tab
        .map(str::to_string)
        .filter(|name| available.iter().any(|engine| &engine.name == name))
        .or_else(|| {
            available
                .iter()
                .find(|engine| engine.name == state.engine)
                .map(|engine| engine.name.clone())
        })
        .or_else(|| available.first().map(|engine| engine.name.clone()))
        .unwrap_or_default();

    let tabs = available
        .iter()
        .map(|engine| PickerTab {
            engine: engine.name.clone(),
            active: engine.name == active_engine,
            unavailable: (!engine.ready).then(|| {
                engine
                    .problems
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "not available".into())
            }),
        })
        .collect();

    let mut models: Vec<ExecutionChoice> = all
        .into_iter()
        .filter(|choice| choice.engine == active_engine && choice.unavailable_is_signin_only())
        .collect();
    if !query.trim().is_empty() {
        let mut scored: Vec<(u32, usize, ExecutionChoice)> = models
            .into_iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                fuzzy::score(query, &choice.haystack()).map(|score| (score, index, choice))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        models = scored.into_iter().map(|(_, _, choice)| choice).collect();
    }

    let active_unavailable = models.first().and_then(|choice| choice.unavailable.clone());
    let effort_target = models
        .iter()
        .find(|choice| choice.current)
        .or_else(|| models.iter().find(|choice| choice.provider_default))
        .or_else(|| models.first())
        .cloned();

    PickerView {
        tabs,
        active_engine,
        active_unavailable,
        models,
        effort_target,
        coming_soon,
    }
}

impl ExecutionChoice {
    /// Gated engines carry "Coming soon"; those never reach the tabbed
    /// picker's model list. A sign-in problem is not gating — the row still
    /// lists so the reason is on screen.
    fn unavailable_is_signin_only(&self) -> bool {
        self.unavailable.as_deref() != Some("Coming soon")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{EngineModelView, EngineStatus, UiState};

    fn engine(name: &str, ready: bool, models: Vec<EngineModelView>) -> EngineStatus {
        EngineStatus {
            name: name.into(),
            ready,
            installed: true,
            authenticated: Some(ready),
            version: None,
            problems: if ready {
                Vec::new()
            } else {
                vec![format!("{name} is not signed in")]
            },
            models,
            model_load_error: None,
        }
    }

    fn model(id: &str, label: &str, efforts: &[&str], default: bool) -> EngineModelView {
        EngineModelView {
            id: id.into(),
            display_name: label.into(),
            description: String::new(),
            reasoning_efforts: efforts.iter().map(|e| (*e).to_string()).collect(),
            default_reasoning_effort: efforts.first().map(|e| (*e).to_string()),
            is_default: default,
        }
    }

    fn state() -> UiState {
        UiState {
            engine: "codex".into(),
            engines: vec![
                engine(
                    "codex",
                    true,
                    vec![model("gpt-5.6-sol", "GPT-5.6-Sol", &["low", "high"], true)],
                ),
                engine(
                    "claude",
                    true,
                    vec![
                        model("opus", "Opus", &["high", "max"], false),
                        model("sonnet", "Sonnet", &["low", "high"], true),
                    ],
                ),
            ],
            ..UiState::default()
        }
    }

    /// The choice is engine AND model AND effort, in one list.
    ///
    /// These used to be three controls in two places — engine chips in the
    /// toolbar, model and effort in a composer popover — so the answer to
    /// "what will run this?" was never in one place. The toolbar pair also
    /// could not show a third engine at all.
    #[test]
    fn every_runnable_combination_is_one_row() {
        let rows = choices(&state());
        // One row per MODEL: codex has 1, claude has 2. Listing every
        // model-and-effort pair turned a 36-model catalog into 104 rows with
        // "Claude default" five times over, which is a haystack, not a chooser.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| row.engine == "codex"));
        assert!(
            rows.iter()
                .any(|row| row.engine == "claude" && row.model_label == "Opus"),
            "an engine the toolbar pair could show is not the limit"
        );
        // A model appears once, carrying the efforts it accepts.
        let opus = rows
            .iter()
            .find(|row| row.model_label == "Opus")
            .expect("opus is listed");
        assert_eq!(opus.efforts, vec!["high".to_string(), "max".to_string()]);
        assert_eq!(opus.effort, "high", "its default is preselected");
    }

    /// Where you already are is the first thing you see.
    #[test]
    fn the_current_choice_sorts_first() {
        let mut state = state();
        state.model = Some("gpt-5.6-sol".into());
        state.reasoning_effort = Some("high".into());

        let rows = choices(&state);
        assert!(rows[0].current, "{:?}", rows[0]);
        assert_eq!(rows[0].engine, "codex");
        // The selected row keeps the effort in use, not the model default.
        assert_eq!(rows[0].effort, "high");
        assert_eq!(rows.iter().filter(|row| row.current).count(), 1);
    }

    /// An engine that cannot run is listed with its reason rather than hidden.
    /// Hiding it turns "why can I not use Claude here" into a question with no
    /// answer on screen — and it sorts below what does work.
    #[test]
    fn an_unavailable_engine_is_explained_not_hidden() {
        let mut state = state();
        state.engines[1] = engine(
            "claude",
            false,
            vec![model("opus", "Opus", &["high"], true)],
        );

        let rows = choices(&state);
        let opus = rows
            .iter()
            .find(|row| row.model_label == "Opus")
            .expect("still listed");
        assert_eq!(opus.unavailable.as_deref(), Some("claude is not signed in"));
        let first_unavailable = rows.iter().position(|row| row.unavailable.is_some());
        let last_available = rows.iter().rposition(|row| row.unavailable.is_none());
        assert!(last_available < first_unavailable, "what works comes first");
    }

    /// An engine with no catalog offers the provider default rather than
    /// nothing. Discovery is allowed to fail without making the engine
    /// unusable, and an empty engine looks broken.
    #[test]
    fn an_engine_without_a_catalog_still_offers_its_default() {
        let mut state = state();
        state.engines[1] = engine("claude", true, Vec::new());

        let rows = choices(&state);
        let claude = rows
            .iter()
            .find(|row| row.engine == "claude")
            .expect("claude is offered");
        assert!(claude.model.is_empty(), "the provider decides");
        assert!(claude.model_label.contains("default"));
        assert!(claude.unavailable.is_none());
    }

    /// A gated engine never floods the picker with its catalog: it is one
    /// "Coming soon" row, listed after everything that can actually run.
    #[test]
    fn a_gated_engine_is_one_coming_soon_row_not_a_catalog() {
        let mut state = state();
        state.engines.push(engine(
            "opencode",
            true,
            vec![
                model("deepseek-v4", "DeepSeek V4 Pro", &["low", "high"], true),
                model("qwen3.8", "Qwen3.8 Max", &["low"], false),
                model("minimax-m3", "MiniMax M3", &["low"], false),
            ],
        ));

        let rows = choices(&state);
        let gated: Vec<_> = rows.iter().filter(|row| row.engine == "opencode").collect();
        assert_eq!(gated.len(), 1, "the catalog stays out of the picker");
        assert_eq!(gated[0].unavailable.as_deref(), Some("Coming soon"));
        assert_eq!(gated[0].description, "Coming soon");
        assert!(
            rows.iter().rposition(|row| row.unavailable.is_none())
                < rows.iter().position(|row| row.engine == "opencode"),
            "coming soon sorts after what works"
        );
    }

    /// The tabbed picker: available engines are tabs, the active tab lists
    /// only its own models, and gated engines are one count — never rows.
    #[test]
    fn the_picker_view_tabs_available_engines_and_counts_the_gated() {
        let mut state = state();
        state.engines.push(engine(
            "opencode",
            true,
            vec![model("deepseek-v4", "DeepSeek V4 Pro", &["low"], true)],
        ));
        state.engines.push(engine("grok", true, Vec::new()));

        let view = picker_view(&state, None, "");
        assert_eq!(
            view.tabs
                .iter()
                .map(|tab| tab.engine.as_str())
                .collect::<Vec<_>>(),
            vec!["codex", "claude"],
            "gated engines are not tabs"
        );
        assert_eq!(view.active_engine, "codex", "follows the current selection");
        assert!(view.models.iter().all(|choice| choice.engine == "codex"));
        assert_eq!(view.coming_soon, 2);

        // An explicit tab pick browses that engine without committing it.
        let claude = picker_view(&state, Some("claude"), "");
        assert_eq!(claude.active_engine, "claude");
        assert_eq!(claude.models.len(), 2);
        assert!(claude.models.iter().all(|choice| choice.engine == "claude"));

        // A gated engine cannot become the active tab.
        let gated = picker_view(&state, Some("opencode"), "");
        assert_eq!(gated.active_engine, "codex");
    }

    /// The reasoning section belongs to the model in use on the active tab,
    /// or that tab's default model when nothing there is selected.
    #[test]
    fn the_effort_section_targets_the_right_model() {
        let mut state = state();
        state.model = Some("gpt-5.6-sol".into());
        let view = picker_view(&state, None, "");
        let target = view.effort_target.expect("current model targeted");
        assert!(target.current);
        assert_eq!(target.model, "gpt-5.6-sol");

        // Browsing the other tab: its default model's efforts, uncommitted.
        let claude = picker_view(&state, Some("claude"), "");
        let target = claude.effort_target.expect("default model targeted");
        assert!(!target.current);
        assert_eq!(target.model, "sonnet", "the provider default leads");
    }

    /// A signed-out engine's tab leads with the reason and still lists its
    /// models — hiding them turns "why can I not use Claude" into a question
    /// with no answer on screen.
    #[test]
    fn a_signed_out_tab_explains_itself_and_still_lists_models() {
        let mut state = state();
        state.engines[1] = engine(
            "claude",
            false,
            vec![model("opus", "Opus", &["high"], true)],
        );

        let view = picker_view(&state, Some("claude"), "");
        assert_eq!(
            view.active_unavailable.as_deref(),
            Some("claude is not signed in")
        );
        assert_eq!(view.models.len(), 1);
    }

    /// Typing narrows across engine and model together, so reaching Claude's
    /// Opus never means leaving the picker to change the engine first.
    #[test]
    fn typing_narrows_across_engine_and_model() {
        let state = state();
        let opus = picker_view(&state, Some("claude"), "opus");
        assert_eq!(opus.models.len(), 1);
        assert_eq!(opus.models[0].model_label, "Opus");

        // An empty query keeps the natural order, so reopening looks the same.
        let blank = picker_view(&state, Some("claude"), "   ");
        assert_eq!(blank.models, picker_view(&state, Some("claude"), "").models);
        assert!(
            picker_view(&state, Some("claude"), "zzzz")
                .models
                .is_empty()
        );
    }
}
