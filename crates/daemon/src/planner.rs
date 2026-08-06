//! The structured planning call (PLAN.md: "deterministic repository/task facts
//! plus one structured planning call to the selected engine").
//!
//! **One** call, and only when the facts have not already settled the route:
//! an atomic task with a check is a direct run and asking a model about it
//! would spend the user's tokens to be told what we already know.
//!
//! The model's answer is untrusted input. It is parsed leniently — models wrap
//! JSON in prose and fences — and then handed to `graph::compile`, which is
//! what actually decides whether anything may run. A plan that will not parse
//! is not an error: it means no proposal, and the router falls back.

use std::path::Path;

use autoharness_core::{EngineKind, GraphProposal};
use autoharness_engines::{EngineEvent, SessionSpec};
use serde::Deserialize;

/// The planning prompt. It asks for one JSON object and nothing else, names
/// the V1 limits so the model does not waste a turn proposing something the
/// compiler will refuse, and demands disjoint file scopes explicitly.
const PROMPT: &str = r#"Plan how to accomplish this objective. Reply with ONE JSON object and no other text.

{
  "confidence": 0.0 to 1.0,
  "integration_strategy": "how the branches are combined",
  "nodes": [
    {
      "id": "short_snake_case",
      "role": "editor" | "reader" | "verifier" | "integration",
      "objective": "what this node does",
      "file_scope": ["glob/**"],
      "acceptance_checks": ["shell command that must pass"]
    }
  ],
  "edges": [["from_id", "to_id"]]
}

Rules the plan must satisfy, or it will be rejected:
- At most 8 nodes, at most 4 running at once, at most 3 levels deep.
- Acyclic, and every branch must converge on exactly ONE final node.
- Two editing nodes may never share a file scope.
- Every editing node must lead to a verifier or integration node.
- Every editing node must declare a file scope and an acceptance check.

If the work is genuinely sequential and does not decompose, reply with
confidence below 0.5. Do not invent parallelism that is not there.

OBJECTIVE:
"#;

/// What the model is asked to return.
#[derive(Debug, Deserialize)]
struct RawPlan {
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    integration_strategy: String,
    #[serde(default)]
    nodes: Vec<RawNode>,
    #[serde(default)]
    edges: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    id: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    objective: String,
    #[serde(default)]
    file_scope: Vec<String>,
    #[serde(default)]
    acceptance_checks: Vec<String>,
}

/// A planning call that never answers must not wedge the run. Past this, the
/// run proceeds with no proposal, which is a safe route, not a failure.
const PLANNING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Ask the engine for a plan, bounded by [`PLANNING_TIMEOUT`]. Returns the
/// proposal and the model's own confidence, or `None` when there is no usable
/// answer — never an error, because a missing plan is a routing input, not a
/// failure.
pub(crate) async fn propose(
    engines: &autoharness_engines::EngineRegistry,
    engine: EngineKind,
    working_dir: &Path,
    data_dir: &Path,
    objective: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Option<(GraphProposal, f32)> {
    match tokio::time::timeout(
        PLANNING_TIMEOUT,
        propose_inner(
            engines,
            engine,
            working_dir,
            data_dir,
            objective,
            model,
            reasoning_effort,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("planning call timed out; routing without a proposal");
            None
        }
    }
}

async fn propose_inner(
    engines: &autoharness_engines::EngineRegistry,
    engine: EngineKind,
    working_dir: &Path,
    data_dir: &Path,
    objective: &str,
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Option<(GraphProposal, f32)> {
    let mut adapter = engines.create(engine)?;
    let spec = SessionSpec {
        working_dir: working_dir.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        // Planning is one shot and is never resumed, so a throwaway home is
        // right here — it keeps proposal chatter out of the thread's own
        // transcript.
        session_key: autoharness_core::new_id(),
        model: model.map(str::to_string),
        reasoning_effort: reasoning_effort.map(str::to_string),
    };
    adapter.start_session(&spec).await.ok()?;
    adapter
        .send_turn(&format!("{PROMPT}{objective}"))
        .await
        .ok()?;

    let mut text = String::new();
    loop {
        match adapter.next_event().await {
            Ok(Some(EngineEvent::Text { text: chunk })) => text.push_str(&chunk),
            Ok(Some(EngineEvent::Completed { .. })) | Ok(None) => break,
            Ok(Some(EngineEvent::Failed { .. })) => break,
            Ok(Some(_)) => {}
            Err(_) => break,
        }
    }
    // The planning session is done; it must not linger holding a process.
    let _ = adapter.cancel().await;

    parse(&text)
}

/// Pull a plan out of whatever the model said. Models wrap JSON in prose and
/// code fences; the object is located by brace matching rather than by hoping
/// the whole reply is valid JSON.
fn parse(text: &str) -> Option<(GraphProposal, f32)> {
    let json = extract_object(text)?;
    let raw: RawPlan = serde_json::from_str(&json).ok()?;
    if raw.nodes.is_empty() {
        return None;
    }
    let proposal = GraphProposal {
        nodes: raw
            .nodes
            .into_iter()
            .map(|n| autoharness_core::ProposedNode {
                id: n.id,
                role: n.role,
                objective: n.objective,
                file_scope: n.file_scope,
                acceptance_checks: n.acceptance_checks,
            })
            .collect(),
        edges: raw
            .edges
            .into_iter()
            .filter(|e| e.len() == 2)
            .map(|e| (e[0].clone(), e[1].clone()))
            .collect(),
        integration_strategy: raw.integration_strategy,
    };
    Some((proposal, raw.confidence.clamp(0.0, 1.0)))
}

/// The first balanced `{...}` run in the text, ignoring braces inside strings.
fn extract_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        let _ = bytes;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + offset + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_json_reply_parses() {
        let (proposal, confidence) = parse(
            r#"{"confidence":0.9,"integration_strategy":"staging",
                "nodes":[{"id":"a","role":"editor","objective":"x",
                          "file_scope":["src/a/**"],"acceptance_checks":["true"]}],
                "edges":[]}"#,
        )
        .unwrap();
        assert_eq!(proposal.nodes.len(), 1);
        assert_eq!(confidence, 0.9);
    }

    /// Models wrap JSON in prose and fences. That must not lose the plan.
    #[test]
    fn json_wrapped_in_prose_and_fences_still_parses() {
        let reply = r#"Sure! Here is the plan:

```json
{
  "confidence": 0.8,
  "integration_strategy": "staging worktree",
  "nodes": [
    {"id": "a", "role": "editor", "objective": "edit a",
     "file_scope": ["src/a/**"], "acceptance_checks": ["cargo test"]},
    {"id": "m", "role": "integration", "objective": "merge",
     "file_scope": ["src/**"], "acceptance_checks": ["cargo test"]}
  ],
  "edges": [["a", "m"]]
}
```

Let me know if you want changes."#;
        let (proposal, confidence) = parse(reply).unwrap();
        assert_eq!(proposal.nodes.len(), 2);
        assert_eq!(proposal.edges, vec![("a".into(), "m".into())]);
        assert_eq!(confidence, 0.8);
    }

    /// Braces inside strings must not end the object early.
    #[test]
    fn braces_inside_strings_do_not_confuse_extraction() {
        let reply = r#"{"confidence":0.7,"integration_strategy":"use {braces} here",
            "nodes":[{"id":"a","role":"editor","objective":"say \"hi\" {ok}",
                      "file_scope":["src/**"],"acceptance_checks":["true"]}],
            "edges":[]}"#;
        let (proposal, _) = parse(reply).unwrap();
        assert_eq!(proposal.integration_strategy, "use {braces} here");
        assert_eq!(proposal.nodes.len(), 1);
    }

    #[test]
    fn a_reply_with_no_usable_plan_is_no_proposal_not_an_error() {
        assert!(parse("I don't think this decomposes.").is_none());
        assert!(parse("{ not json }").is_none());
        assert!(parse(r#"{"confidence":0.9,"nodes":[]}"#).is_none());
        assert!(parse("").is_none());
    }

    /// A model that claims impossible confidence is clamped, not trusted.
    #[test]
    fn confidence_is_clamped_to_a_real_probability() {
        let (_, high) = parse(
            r#"{"confidence":9.5,"nodes":[{"id":"a","role":"editor","objective":"x",
                "file_scope":["a/**"],"acceptance_checks":["true"]}],"edges":[]}"#,
        )
        .unwrap();
        assert_eq!(high, 1.0);
        let (_, low) = parse(
            r#"{"confidence":-3,"nodes":[{"id":"a","role":"editor","objective":"x",
                "file_scope":["a/**"],"acceptance_checks":["true"]}],"edges":[]}"#,
        )
        .unwrap();
        assert_eq!(low, 0.0);
    }

    /// Malformed edges are dropped rather than poisoning the proposal; the
    /// compiler would refuse the graph anyway, but silently mangling an edge
    /// into a self-loop would be worse than dropping it.
    #[test]
    fn malformed_edges_are_dropped() {
        let (proposal, _) = parse(
            r#"{"confidence":0.9,"nodes":[{"id":"a","role":"editor","objective":"x",
                "file_scope":["a/**"],"acceptance_checks":["true"]}],
                "edges":[["a"],["a","b"],["a","b","c"]]}"#,
        )
        .unwrap();
        assert_eq!(proposal.edges, vec![("a".into(), "b".into())]);
    }
}
