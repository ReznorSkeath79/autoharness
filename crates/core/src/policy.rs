//! Policy evolution guards (PLAN.md "Self-evolution").
//!
//! **V1 observes and proposes; it never self-promotes.** This module is the
//! boundary that makes that safe: a candidate policy may only touch a fixed
//! allow-list of tuning knobs, and any attempt to reach a capability,
//! sandbox, external-write, retention, evaluator, approval, or promotion
//! setting is rejected outright.
//!
//! The guard is a deny-list AND an allow-list on purpose. An allow-list alone
//! would silently permit a newly added field; a deny-list alone would permit
//! a renamed one. Requiring both means a new knob has to be added here
//! deliberately, by a human, before any candidate can move it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Fields a candidate policy may change (PLAN.md: "Candidate policies may
/// change bounded routing thresholds, approved graph-template selection,
/// detector thresholds, retry allocation, and coordinator prompt fragments").
pub const MUTABLE_FIELDS: &[&str] = &[
    "router_thresholds",
    "approved_graph_templates",
    "detector_thresholds",
    "recovery_budgets",
    "coordinator_prompt_fragments",
];

/// Substrings that mark a field as untouchable no matter where it appears.
/// PLAN.md: "Candidates may not change sandbox rules, capabilities,
/// external-write policy, event retention, evaluators, maximum budgets,
/// approval requirements, or promotion logic."
const FORBIDDEN_MARKERS: &[&str] = &[
    "sandbox",
    "capab",
    "external_write",
    "externalwrite",
    "retention",
    "evaluator",
    "approval",
    "approve",
    "promot",
    "seatbelt",
    "proxy",
    "credential",
    "keychain",
    "token",
    "egress",
];

/// Ceilings a candidate may lower but never raise. "Maximum budgets" are a
/// capability boundary: a policy that could raise them could spend without
/// limit.
const CAPPED_BUDGET_FIELDS: &[&str] = &[
    "max_turns",
    "max_tool_calls",
    "max_retries",
    "max_concurrent_workers",
    "max_graph_nodes",
    "wall_time_secs",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyViolation {
    /// A field outside the mutable allow-list.
    ImmutableField(String),
    /// A field whose name marks it as a capability boundary.
    ForbiddenField(String),
    /// A ceiling raised above the promoted policy's value.
    RaisedCeiling { field: String, from: f64, to: f64 },
    /// The candidate is not a JSON object.
    NotAnObject,
}

impl PolicyViolation {
    pub fn describe(&self) -> String {
        match self {
            PolicyViolation::ImmutableField(f) => {
                format!("'{f}' is not a tunable policy field")
            }
            PolicyViolation::ForbiddenField(f) => {
                format!("'{f}' controls a capability boundary and can never be changed by a policy")
            }
            PolicyViolation::RaisedCeiling { field, from, to } => {
                format!(
                    "'{field}' would rise from {from} to {to}; a policy may only lower a ceiling"
                )
            }
            PolicyViolation::NotAnObject => "a policy must be a JSON object".into(),
        }
    }
}

/// Check a candidate policy against the promoted one. `Ok(())` means the
/// candidate is safe to evaluate and offer for promotion — never that it
/// should be promoted, which only a human decides.
pub fn validate_candidate(candidate: &Value, current: &Value) -> Result<(), Vec<PolicyViolation>> {
    let Some(fields) = candidate.as_object() else {
        return Err(vec![PolicyViolation::NotAnObject]);
    };
    let mut violations = Vec::new();

    for (key, value) in fields {
        // The exact allow-list wins first. The substring markers below are a
        // net for names nobody enumerated, and one of the allowed fields
        // ("approved_graph_templates") legitimately contains "approve" — a
        // marker check running first would refuse a field PLAN.md permits.
        if !MUTABLE_FIELDS.contains(&key.as_str()) {
            let lower = key.to_lowercase();
            if FORBIDDEN_MARKERS.iter().any(|m| lower.contains(m)) {
                violations.push(PolicyViolation::ForbiddenField(key.clone()));
            } else {
                violations.push(PolicyViolation::ImmutableField(key.clone()));
            }
            continue;
        }
        // Nested keys are guarded too: a forbidden name cannot hide inside an
        // allowed section.
        collect_forbidden(value, key, &mut violations);
        // Ceilings may fall, never rise.
        check_ceilings(value, current.get(key), &mut violations);
    }

    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations)
    }
}

fn collect_forbidden(value: &Value, path: &str, out: &mut Vec<PolicyViolation>) {
    let Some(object) = value.as_object() else {
        return;
    };
    for (key, nested) in object {
        let lower = key.to_lowercase();
        let full = format!("{path}.{key}");
        if FORBIDDEN_MARKERS.iter().any(|m| lower.contains(m)) {
            out.push(PolicyViolation::ForbiddenField(full));
            continue;
        }
        collect_forbidden(nested, &full, out);
    }
}

fn check_ceilings(candidate: &Value, current: Option<&Value>, out: &mut Vec<PolicyViolation>) {
    let (Some(candidate), Some(current)) =
        (candidate.as_object(), current.and_then(Value::as_object))
    else {
        return;
    };
    for (key, value) in candidate {
        if !CAPPED_BUDGET_FIELDS.contains(&key.as_str()) {
            continue;
        }
        let (Some(new), Some(old)) = (value.as_f64(), current.get(key).and_then(Value::as_f64))
        else {
            continue;
        };
        if new > old {
            out.push(PolicyViolation::RaisedCeiling {
                field: key.clone(),
                from: old,
                to: new,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn current() -> Value {
        json!({
            "router_thresholds": { "min_graph_confidence": 0.7 },
            "detector_thresholds": { "repeated_action": 3 },
            "recovery_budgets": { "max_retries": 3, "max_turns": 40 },
            "approved_graph_templates": ["fanout-verify"],
        })
    }

    #[test]
    fn tuning_the_allowed_knobs_is_accepted() {
        let candidate = json!({
            "router_thresholds": { "min_graph_confidence": 0.85 },
            "detector_thresholds": { "repeated_action": 4 },
            "approved_graph_templates": ["fanout-verify", "chain-verify"],
        });
        assert!(validate_candidate(&candidate, &current()).is_ok());
    }

    #[test]
    fn a_ceiling_may_be_lowered() {
        let candidate = json!({ "recovery_budgets": { "max_retries": 1 } });
        assert!(validate_candidate(&candidate, &current()).is_ok());
    }

    /// The whole point of the guard: a policy that could raise its own limits
    /// could spend without bound.
    #[test]
    fn a_ceiling_may_never_be_raised() {
        let candidate = json!({ "recovery_budgets": { "max_turns": 4000 } });
        let violations = validate_candidate(&candidate, &current()).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, PolicyViolation::RaisedCeiling { field, .. } if field == "max_turns")),
            "{violations:?}"
        );
    }

    #[test]
    fn capability_boundaries_are_refused_at_the_top_level() {
        for field in [
            "sandbox_rules",
            "capabilities",
            "external_write_policy",
            "event_retention",
            "evaluators",
            "approval_requirements",
            "promotion_logic",
        ] {
            let candidate = json!({ field: "anything" });
            let violations = validate_candidate(&candidate, &current()).unwrap_err();
            assert!(
                violations
                    .iter()
                    .any(|v| matches!(v, PolicyViolation::ForbiddenField(_))),
                "{field} must be refused, got {violations:?}"
            );
        }
    }

    /// A forbidden setting must not be smuggled inside an allowed section.
    #[test]
    fn capability_boundaries_are_refused_when_nested() {
        let candidate = json!({
            "router_thresholds": {
                "min_graph_confidence": 0.8,
                "sandbox_profile": "permissive",
            }
        });
        let violations = validate_candidate(&candidate, &current()).unwrap_err();
        assert!(
            violations.iter().any(
                |v| matches!(v, PolicyViolation::ForbiddenField(f) if f.contains("sandbox_profile"))
            ),
            "{violations:?}"
        );
    }

    #[test]
    fn an_unknown_field_is_refused_rather_than_ignored() {
        let candidate = json!({ "some_new_knob": 1 });
        let violations = validate_candidate(&candidate, &current()).unwrap_err();
        assert_eq!(
            violations,
            vec![PolicyViolation::ImmutableField("some_new_knob".into())],
            "a new knob must be added to MUTABLE_FIELDS deliberately"
        );
    }

    #[test]
    fn a_non_object_candidate_is_refused() {
        assert_eq!(
            validate_candidate(&json!("permissive"), &current()).unwrap_err(),
            vec![PolicyViolation::NotAnObject]
        );
    }

    #[test]
    fn every_violation_explains_itself() {
        let candidate = json!({
            "sandbox": {},
            "unknown": 1,
            "recovery_budgets": { "max_turns": 9999 },
        });
        for violation in validate_candidate(&candidate, &current()).unwrap_err() {
            assert!(violation.describe().len() > 10, "{violation:?}");
        }
    }
}
