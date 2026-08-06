//! Client-side projection of the daemon-owned durable queue.

use autoharness_protocol::params::{QueueItem, QueueKind, QueueState};
use serde_json::Value;

const QUEUE_VIEW_CAP: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueAction {
    Refresh,
    MoveBefore {
        item_id: String,
        before_item_id: String,
    },
    Cancel(String),
}

pub fn apply_action(
    state: &mut crate::client::UiState,
    action: QueueAction,
) -> crate::client::Command {
    state.queue.loading = true;
    state.queue.error = None;
    match action {
        QueueAction::Refresh => crate::client::Command::RefreshQueue,
        QueueAction::MoveBefore {
            item_id,
            before_item_id,
        } => crate::client::Command::MoveQueueItem {
            item_id,
            before_item_id: Some(before_item_id),
        },
        QueueAction::Cancel(item_id) => crate::client::Command::CancelQueueItem { item_id },
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueueView {
    pub items: Vec<QueueItem>,
    pub loading: bool,
    pub error: Option<String>,
}

impl QueueView {
    pub fn replace(&mut self, items: Vec<QueueItem>) {
        self.items = items;
        self.sort_and_cap();
        self.loading = false;
        self.error = None;
    }

    pub fn pending_objectives(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.kind == QueueKind::Objective && item.state == QueueState::Pending)
            .count()
    }

    pub fn pending_steering_for(&self, run_id: &str) -> usize {
        self.items
            .iter()
            .filter(|item| {
                item.kind == QueueKind::Steering
                    && item.run_id == run_id
                    && item.state == QueueState::Pending
            })
            .count()
    }

    pub fn waiting(&self) -> impl Iterator<Item = &QueueItem> {
        self.items
            .iter()
            .filter(|item| item.state == QueueState::Pending)
    }

    fn upsert(&mut self, item: QueueItem) {
        match self
            .items
            .iter_mut()
            .find(|existing| existing.id == item.id)
        {
            Some(existing) => *existing = item,
            None => self.items.push(item),
        }
        self.sort_and_cap();
    }

    fn sort_and_cap(&mut self) {
        self.items.sort_by_key(|item| {
            let kind = match item.kind {
                QueueKind::Objective => 0u8,
                QueueKind::Steering => 1u8,
            };
            (kind, item.position, item.created_at_ms, item.id.clone())
        });
        if self.items.len() > QUEUE_VIEW_CAP {
            self.items.drain(0..self.items.len() - QUEUE_VIEW_CAP);
        }
    }
}

pub fn reduce_event(view: &mut QueueView, kind: &str, payload: &Value) {
    if !kind.starts_with("queue.") {
        return;
    }
    if let Ok(item) = serde_json::from_value::<QueueItem>(payload.clone()) {
        view.upsert(item);
        return;
    }
    let Some(id) = payload
        .get("queue_id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("id").and_then(Value::as_str))
    else {
        return;
    };
    let state = match kind {
        "queue.dispatching" | "queue.dispatched" => Some(QueueState::Dispatching),
        "queue.completed" => Some(QueueState::Completed),
        "queue.failed" => Some(QueueState::Failed),
        "queue.cancelled" => Some(QueueState::Cancelled),
        _ => None,
    };
    if let Some(state) = state
        && let Some(item) = view.items.iter_mut().find(|item| item.id == id)
    {
        item.state = state;
        item.error = payload
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
}
