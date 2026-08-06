//! Ledger-derived usage rows.
//!
//! Everything here is a fold over `engine.usage` events the daemon already
//! persisted. Nothing is estimated and nothing is priced: AutoHarness does not
//! know what a provider charges, so it does not pretend to.

use crate::client::UiState;
use autoharness_protocol::params::UsageBucket;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageRow {
    pub label: String,
    pub value: String,
}

fn row(label: impl Into<String>, value: impl Into<String>) -> UsageRow {
    UsageRow {
        label: label.into(),
        value: value.into(),
    }
}

fn bucket_value(bucket: &UsageBucket) -> String {
    format!(
        "{} tokens · {} in / {} out · {} runs",
        bucket.total_tokens, bucket.input_tokens, bucket.output_tokens, bucket.run_count
    )
}

pub(crate) fn usage_rows(state: &UiState) -> Vec<UsageRow> {
    let summary = &state.usage_summary;
    let mut rows = Vec::new();
    if summary.loading {
        rows.push(row("Usage", "Reading the ledger…"));
    }
    if let Some(error) = summary.error.as_deref() {
        rows.push(row("Usage error", error.to_string()));
    }
    for provider in &summary.providers {
        rows.push(row(
            format!("{} today", provider.provider),
            bucket_value(&provider.today),
        ));
        rows.push(row(
            format!("{} month", provider.provider),
            bucket_value(&provider.month),
        ));
        rows.push(row(
            format!("{} all time", provider.provider),
            bucket_value(&provider.all_time),
        ));
    }
    for run in &summary.runs {
        rows.push(row(
            run.run_id.clone(),
            format!(
                "{} · {} tokens · {} in / {} out",
                run.provider, run.total_tokens, run.input_tokens, run.output_tokens
            ),
        ));
    }
    if rows.is_empty() {
        rows.push(row("Usage", "No engine usage recorded yet"));
    }
    rows
}
