//! Cross-engine handoff.
//!
//! A provider session belongs to the provider that made it: a Codex thread id
//! means nothing to Claude. So when a conversation continues on a different
//! engine, resuming is refused — correctly — and the new engine would
//! otherwise start with no idea what the last one did.
//!
//! This composes a brief from the event ledger and gives it to the new engine
//! as the opening of its first turn. The ledger is already the source of truth
//! for a run, so the brief is:
//!
//! - **free** — no model call, so switching engines costs nothing and adds no
//!   latency;
//! - **deterministic** — the same thread produces the same brief, so a replay
//!   reconstructs exactly what the second engine was told;
//! - **always available** — it does not need the previous engine's process to
//!   still be alive, which it usually is not.
//!
//! Asking the outgoing model to summarize itself would read better and cost a
//! turn, tokens, and a live session to ask. It would also be unverifiable: a
//! model describing its own work is exactly the claim a handoff should not
//! take on trust. What is here instead is what actually happened.

use autoharness_store::{Run, Store};

/// The one ceiling on a brief.
///
/// Sections used to be capped at twelve items and lines clipped at three
/// hundred characters, which meant an incoming engine was handed a summary of a
/// summary — and silently, with no sign that anything had been dropped. Turns
/// are now carried whole. What remains is a single total ceiling, because the
/// daemon still must not be able to emit an unbounded prompt, and crossing it
/// is *announced* rather than hidden: whole oldest turns are dropped, newest
/// first kept, and the brief says how many went.
const MAX_BRIEF_CHARS: usize = 100_000;

/// One earlier turn of the thread.
struct Turn {
    engine: String,
    objective: String,
    said: Vec<String>,
    files: Vec<String>,
    checks: Vec<String>,
    commit: Option<String>,
}

/// Build the handoff brief for `run`, or `None` when there is nothing to hand
/// over — a fresh thread, or a continuation on the same engine, which resumes
/// its own session and needs no summary.
///
/// `worktree_carried` says whether the checkout this run is starting in is the
/// one the previous turns worked in. It is not decoration: telling an engine
/// that earlier work is already on disk when the tree is actually empty sends
/// it looking for files that do not exist, and it will report on a state that
/// was never there.
pub(crate) fn brief(store: &Store, run: &Run, worktree_carried: bool) -> Option<String> {
    let parent_id = run.parent_run_id.as_deref()?;
    let chain = ancestry(store, parent_id);
    if chain.is_empty() {
        return None;
    }
    // Same engine throughout: the session resumes and carries its own context.
    if chain.iter().all(|r| r.engine == run.engine) {
        return None;
    }

    let turns: Vec<Turn> = chain.iter().map(|r| turn_of(store, r)).collect();
    let previous: Vec<&str> = {
        let mut seen: Vec<&str> = Vec::new();
        for turn in &turns {
            if !seen.contains(&turn.engine.as_str()) {
                seen.push(&turn.engine);
            }
        }
        seen
    };

    let header = format!(
        "You are continuing work another agent started. It ran as `{}`; you are `{}`. \
         You cannot see its session, so here is what it actually did, taken from the \
         run log rather than from its own account of itself.\n",
        previous.join("`, `"),
        run.engine
    );
    let footer = if worktree_carried {
        "\nYou are in the same worktree those turns worked in, so their committed work is \
         already on disk and on the branch. Verify anything you depend on rather than \
         assuming it — the notes above are what was claimed and what the checks reported, \
         not a guarantee. Then continue with the request that follows.\n"
    } else {
        "\nThe worktree you are in is FRESH: it was created from the base commit and does \
         NOT contain the work described above. Treat that history as context only. Verify \
         the tree before you rely on any of it — the notes above are what was claimed and \
         what the checks reported, not a guarantee, and this time they are not on disk \
         either. Redo whatever the request that follows actually needs.\n"
    };

    Some(assemble(&header, &turns, footer))
}

/// Lay the brief out and hold it to [`MAX_BRIEF_CHARS`].
///
/// Turns are rendered whole. If they do not all fit, the OLDEST are dropped —
/// the incoming engine's next action depends most on the most recent turn — and
/// the brief states how many were dropped, so a reader can never mistake a
/// clipped history for the whole one. A single turn too large to fit alone is
/// truncated with the same announcement rather than dropped entirely, because
/// some of the last turn beats none of it.
fn assemble(header: &str, turns: &[Turn], footer: &str) -> String {
    /// Room held back for whichever notice the layout ends up needing. Both are
    /// short and bounded; reserving up front is what keeps the announcement
    /// itself from pushing the brief over the ceiling it announces.
    const NOTICE_ALLOWANCE: usize = 256;
    const TRUNCATED_TURN: &str = "\n[turn truncated at the brief size ceiling]\n";

    let rendered: Vec<String> = turns
        .iter()
        .enumerate()
        .map(|(index, turn)| render_turn(index + 1, turn))
        .collect();

    let fixed = count(header) + count(footer);
    let budget = MAX_BRIEF_CHARS
        .saturating_sub(fixed)
        .saturating_sub(NOTICE_ALLOWANCE);

    // Newest first: the incoming engine's next action depends most on the last
    // turn, so that is the one that must survive a tight budget.
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0;
    for body in rendered.iter().rev() {
        let size = count(body);
        if used + size > budget {
            break;
        }
        used += size;
        kept.push(body);
    }
    kept.reverse();

    let dropped = rendered.len() - kept.len();
    let mut out = String::with_capacity(fixed + used + NOTICE_ALLOWANCE);
    out.push_str(header);

    if dropped > 0 {
        // Said before the turns, so the omission reads as part of the record
        // rather than being discovered at the bottom.
        out.push_str(&format!(
            "\n[{dropped} earlier turn(s) omitted: this thread's full log is larger than one \
             brief can carry. What follows is the most recent {} of {} turns.]\n",
            kept.len(),
            rendered.len()
        ));
    }

    if kept.is_empty() {
        // Even the newest turn alone is over budget. Carry as much of it as
        // fits and mark the cut: some of the last turn beats none of it.
        if let Some(last) = rendered.last() {
            out.push_str(&take_chars(
                last,
                budget.saturating_sub(count(TRUNCATED_TURN)),
            ));
            out.push_str(TRUNCATED_TURN);
        }
    } else {
        for body in kept {
            out.push_str(body);
        }
    }

    out.push_str(footer);
    debug_assert!(count(&out) <= MAX_BRIEF_CHARS);
    out
}

/// One turn, verbatim. Nothing here is clipped: what the ledger recorded is
/// what the next engine reads.
fn render_turn(number: usize, turn: &Turn) -> String {
    let mut out = format!(
        "\n## Turn {number} (`{}`)\nAsked: {}\n",
        turn.engine,
        one_line(&turn.objective)
    );
    if !turn.said.is_empty() {
        out.push_str("Reported:\n");
        for line in &turn.said {
            out.push_str(&format!("- {}\n", one_line(line)));
        }
    }
    if !turn.files.is_empty() {
        out.push_str(&format!("Changed: {}\n", turn.files.join(", ")));
    }
    for check in &turn.checks {
        out.push_str(&format!("Check: {check}\n"));
    }
    if let Some(commit) = &turn.commit {
        out.push_str(&format!("Committed: {commit}\n"));
    }
    out
}

fn count(text: &str) -> usize {
    text.chars().count()
}

fn take_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The thread's runs from the root down to `run_id`, oldest first.
///
/// The walk used to stop after 64 ancestors. That bounded the loop, but it also
/// meant a long thread silently handed over only its recent tail — with nothing
/// in the brief to say so. Termination is now a cycle check, which is what the
/// guard was actually for; how much of the result survives is decided in one
/// place, [`assemble`], which announces whatever it drops.
fn ancestry(store: &Store, run_id: &str) -> Vec<Run> {
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = Some(run_id.to_string());
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break; // A malformed chain that loops must not spin.
        }
        let Ok(run) = store.get_run(&id) else { break };
        cursor = run.parent_run_id.clone();
        chain.push(run);
    }
    chain.reverse();
    chain
}

/// Everything the ledger recorded for one run, condensed.
fn turn_of(store: &Store, run: &Run) -> Turn {
    let mut turn = Turn {
        engine: run.engine.clone(),
        objective: run.objective.clone(),
        said: Vec::new(),
        files: Vec::new(),
        checks: Vec::new(),
        commit: None,
    };
    let Ok(events) = store.events_since(0, Some(&run.id)) else {
        return turn;
    };
    for event in events {
        let payload = &event.payload;
        let text = |key: &str| {
            payload
                .get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        match event.kind.as_str() {
            "engine.text" => {
                let said = text("text");
                if !said.trim().is_empty() {
                    turn.said.push(said);
                }
            }
            "engine.file_change" => {
                let path = text("path");
                if !path.is_empty() && !turn.files.contains(&path) {
                    turn.files.push(path);
                }
            }
            "run.check" => {
                let passed = payload
                    .get("passed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                turn.checks.push(format!(
                    "`{}` {}",
                    text("command"),
                    if passed { "passed" } else { "FAILED" }
                ));
            }
            "run.commit" => {
                if let Some(commit) = payload.get("commit").and_then(serde_json::Value::as_str) {
                    turn.commit = Some(commit.to_string());
                }
            }
            _ => {}
        }
    }
    turn
}

/// Flatten a value onto one line without shortening it.
///
/// The list format is one item per line, so an embedded newline would turn one
/// reported line into several bullet-less ones. Collapsing whitespace keeps the
/// shape readable; no characters are dropped.
fn one_line(text: &str) -> String {
    text.trim().replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with_thread() -> (Store, Run, Run) {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/handoff").unwrap();
        let first = store
            .create_run_full(&project.id, "codex", "Add a parser", None, None)
            .unwrap();
        store
            .append_event(
                Some(&first.id),
                "engine.text",
                json!({ "text": "Added the parser and its tests." }),
            )
            .unwrap();
        store
            .append_event(
                Some(&first.id),
                "engine.file_change",
                json!({ "path": "src/parser.rs" }),
            )
            .unwrap();
        store
            .append_event(
                Some(&first.id),
                "run.check",
                json!({ "command": "cargo test", "passed": true }),
            )
            .unwrap();
        store
            .append_event(
                Some(&first.id),
                "run.commit",
                json!({ "commit": "abc1234" }),
            )
            .unwrap();

        let second = store
            .create_run_full(
                &project.id,
                "claude",
                "Now handle escapes",
                None,
                Some(&first.id),
            )
            .unwrap();
        (store, first, second)
    }

    /// The point of the whole module: a different engine is told what happened.
    #[test]
    fn switching_engines_produces_a_brief_of_the_real_work() {
        let (store, _first, second) = store_with_thread();
        let brief = brief(&store, &second, true).expect("a cross-engine turn needs a brief");

        // Who it was, and who it now is.
        assert!(brief.contains("`codex`"), "{brief}");
        assert!(brief.contains("`claude`"), "{brief}");
        // What was asked, done, changed, checked, committed.
        assert!(brief.contains("Add a parser"));
        assert!(brief.contains("Added the parser and its tests."));
        assert!(brief.contains("src/parser.rs"));
        assert!(brief.contains("cargo test"));
        assert!(brief.contains("passed"));
        assert!(brief.contains("abc1234"));
        // And it says not to take the previous agent's word for it.
        assert!(brief.contains("Verify"));
    }

    /// The brief must describe the tree the engine is actually standing in.
    /// Telling it that earlier work is on disk when the worktree was recreated
    /// from the base commit sends it hunting for files that do not exist.
    #[test]
    fn a_fresh_worktree_is_never_described_as_carrying_the_work() {
        let (store, _first, second) = store_with_thread();

        let carried = brief(&store, &second, true).unwrap();
        assert!(carried.contains("same worktree"), "{carried}");
        assert!(!carried.contains("FRESH"), "{carried}");

        let fresh = brief(&store, &second, false).unwrap();
        assert!(fresh.contains("FRESH"), "{fresh}");
        assert!(!fresh.contains("same worktree"), "{fresh}");
        // Both variants still refuse to let the incoming engine take the
        // previous one's account on trust.
        assert!(fresh.contains("Verify"), "{fresh}");
    }

    /// Continuing on the same engine resumes the real session, so a summary
    /// would be strictly worse than the context the model already has.
    #[test]
    fn continuing_on_the_same_engine_needs_no_brief() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/same").unwrap();
        let first = store
            .create_run_full(&project.id, "codex", "one", None, None)
            .unwrap();
        let second = store
            .create_run_full(&project.id, "codex", "two", None, Some(&first.id))
            .unwrap();
        assert!(brief(&store, &second, true).is_none());
    }

    #[test]
    fn a_fresh_thread_has_nothing_to_hand_over() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/fresh").unwrap();
        let run = store
            .create_run_full(&project.id, "codex", "one", None, None)
            .unwrap();
        assert!(brief(&store, &run, true).is_none());
    }

    /// Every earlier turn is carried, in order, so the second engine sees the
    /// whole conversation rather than only the last exchange.
    #[test]
    fn the_brief_covers_every_earlier_turn_in_order() {
        let (store, first, second) = store_with_thread();
        let third = store
            .create_run_full(
                &second.project_id,
                "codex",
                "and now unicode",
                None,
                Some(&second.id),
            )
            .unwrap();
        store
            .append_event(
                Some(&second.id),
                "engine.text",
                json!({ "text": "Escapes handled." }),
            )
            .unwrap();

        let brief = brief(&store, &third, true).expect("engines differ across the thread");
        let turn_one = brief.find("Add a parser").expect("first turn present");
        let turn_two = brief
            .find("Now handle escapes")
            .expect("second turn present");
        assert!(turn_one < turn_two, "turns must read oldest first");
        assert!(brief.contains("Escapes handled."));
        let _ = first;
    }

    /// Turns are carried whole. A long reported line and a long objective must
    /// reach the next engine intact — the previous behavior clipped both at 300
    /// characters and dropped everything past the twelfth item, so the incoming
    /// engine silently received a summary of a summary.
    #[test]
    fn a_turn_is_handed_over_verbatim() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/verbatim").unwrap();
        let long_objective = format!("rewrite {} now", "x".repeat(4_000));
        let long_line = format!("I changed {} and it works", "y".repeat(4_000));
        let first = store
            .create_run_full(&project.id, "codex", &long_objective, None, None)
            .unwrap();
        store
            .append_event(Some(&first.id), "engine.text", json!({ "text": long_line }))
            .unwrap();
        // Well past the twelve-item cap that used to apply.
        for index in 0..40 {
            store
                .append_event(
                    Some(&first.id),
                    "engine.file_change",
                    json!({ "path": format!("src/file{index}.rs") }),
                )
                .unwrap();
        }
        let second = store
            .create_run_full(&project.id, "claude", "continue", None, Some(&first.id))
            .unwrap();

        let brief = brief(&store, &second, true).unwrap();
        assert!(brief.contains(&long_objective), "the ask is carried whole");
        assert!(brief.contains(&long_line), "the report is carried whole");
        assert!(!brief.contains('…'), "nothing is clipped mid-sentence");
        assert!(brief.contains("src/file0.rs"));
        assert!(
            brief.contains("src/file39.rs"),
            "every changed file is carried, not the first twelve"
        );
    }

    /// A brief is prepended to a real prompt, so it must stay bounded no
    /// matter how chatty the previous engine was — and when the ceiling does
    /// bite, it has to SAY so rather than quietly hand over a partial history.
    #[test]
    fn a_chatty_predecessor_cannot_blow_up_the_brief() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/chatty").unwrap();
        let first = store
            .create_run_full(&project.id, "codex", "x".repeat(5_000).as_str(), None, None)
            .unwrap();
        for i in 0..200 {
            store
                .append_event(
                    Some(&first.id),
                    "engine.text",
                    json!({ "text": format!("line {i} {}", "y".repeat(2_000)) }),
                )
                .unwrap();
            store
                .append_event(
                    Some(&first.id),
                    "engine.file_change",
                    json!({ "path": format!("src/file{i}.rs") }),
                )
                .unwrap();
        }
        let second = store
            .create_run_full(&project.id, "claude", "continue", None, Some(&first.id))
            .unwrap();
        let brief = brief(&store, &second, true).unwrap();
        assert!(
            brief.chars().count() <= MAX_BRIEF_CHARS,
            "brief must stay under the ceiling, was {} chars",
            brief.chars().count()
        );
        // One enormous turn cannot be dropped whole — some of the last turn is
        // worth more than none of it — but the cut has to be visible.
        assert!(brief.contains("truncated"), "an elided brief says so");
    }

    /// A deep chain must terminate rather than walking forever. The guard is
    /// what stops a malformed parent link from spinning the daemon.
    #[test]
    fn a_very_deep_chain_terminates() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/deep").unwrap();
        let mut parent: Option<String> = None;
        let mut last = None;
        for i in 0..200 {
            let engine = if i % 2 == 0 { "codex" } else { "claude" };
            let run = store
                .create_run_full(
                    &project.id,
                    engine,
                    &format!("turn {i}"),
                    None,
                    parent.as_deref(),
                )
                .unwrap();
            parent = Some(run.id.clone());
            last = Some(run);
        }
        let brief = brief(&store, &last.unwrap(), true).expect("engines alternate");
        assert!(
            brief.chars().count() <= MAX_BRIEF_CHARS,
            "was {} chars",
            brief.chars().count()
        );
        // A 200-turn chain fits comfortably, so nothing is dropped. The brief
        // describes the turns BEFORE this one, so the newest it carries is 198.
        assert!(
            brief.contains("turn 198"),
            "the newest earlier turn is kept"
        );
        assert!(
            brief.contains("turn 0"),
            "and a chain this size drops nothing"
        );
        assert!(!brief.contains("omitted"));
    }
}
