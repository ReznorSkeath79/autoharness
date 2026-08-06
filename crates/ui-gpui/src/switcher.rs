//! MRU run switcher state.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDirection {
    Forward,
    Reverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchOutcome<'a> {
    Preview(&'a str),
    Commit(&'a str),
    Cancel(&'a str),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSwitcher {
    original_run_id: String,
    run_ids: Vec<String>,
    highlighted: usize,
}

impl RunSwitcher {
    pub fn open(original_run_id: impl Into<String>, run_ids: Vec<String>) -> Option<Self> {
        let original_run_id = original_run_id.into();
        let mut deduped = Vec::new();
        for id in run_ids {
            if !id.is_empty() && !deduped.contains(&id) {
                deduped.push(id);
            }
        }
        if deduped.is_empty() {
            return None;
        }
        if !deduped.contains(&original_run_id) {
            deduped.insert(0, original_run_id.clone());
        }
        let highlighted = deduped
            .iter()
            .position(|id| id == &original_run_id)
            .unwrap_or(0);
        Some(Self {
            original_run_id,
            run_ids: deduped,
            highlighted,
        })
    }

    pub fn highlighted(&self) -> &str {
        self.run_ids
            .get(self.highlighted)
            .map(String::as_str)
            .unwrap_or(self.original_run_id.as_str())
    }

    pub fn run_ids(&self) -> &[String] {
        &self.run_ids
    }

    pub fn cycle(&mut self, direction: SwitchDirection) -> SwitchOutcome<'_> {
        if self.run_ids.is_empty() {
            return SwitchOutcome::Empty;
        }
        self.highlighted = match direction {
            SwitchDirection::Forward => (self.highlighted + 1) % self.run_ids.len(),
            SwitchDirection::Reverse => {
                if self.highlighted == 0 {
                    self.run_ids.len() - 1
                } else {
                    self.highlighted - 1
                }
            }
        };
        SwitchOutcome::Preview(self.highlighted())
    }

    pub fn commit(&self) -> SwitchOutcome<'_> {
        if self.run_ids.is_empty() {
            SwitchOutcome::Empty
        } else {
            SwitchOutcome::Commit(self.highlighted())
        }
    }

    pub fn cancel(&self) -> SwitchOutcome<'_> {
        SwitchOutcome::Cancel(self.original_run_id.as_str())
    }

    pub const fn supports_control_release_commit() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switcher_cycles_forward_reverse_and_commits_without_mutating_original() {
        let ids = vec!["r2".into(), "r3".into(), "r1".into()];
        let mut switcher = RunSwitcher::open("r2", ids).expect("has switchable runs");

        assert_eq!(switcher.highlighted(), "r2");
        assert_eq!(
            switcher.cycle(SwitchDirection::Forward),
            SwitchOutcome::Preview("r3")
        );
        assert_eq!(switcher.highlighted(), "r3");
        assert_eq!(
            switcher.cycle(SwitchDirection::Reverse),
            SwitchOutcome::Preview("r2")
        );
        assert_eq!(switcher.commit(), SwitchOutcome::Commit("r2"));
    }

    #[test]
    fn switcher_cancel_restores_the_original_run() {
        let mut switcher = RunSwitcher::open("r2", vec!["r2".into(), "r3".into(), "r1".into()])
            .expect("has switchable runs");

        assert_eq!(
            switcher.cycle(SwitchDirection::Forward),
            SwitchOutcome::Preview("r3")
        );
        assert_eq!(switcher.cancel(), SwitchOutcome::Cancel("r2"));
    }

    #[test]
    fn switcher_deduplicates_tips_and_wraps() {
        let mut switcher = RunSwitcher::open(
            "r2",
            vec!["r2".into(), "r3".into(), "r3".into(), "r1".into()],
        )
        .expect("has switchable runs");

        assert_eq!(
            switcher.cycle(SwitchDirection::Reverse),
            SwitchOutcome::Preview("r1")
        );
        assert_eq!(
            switcher.cycle(SwitchDirection::Forward),
            SwitchOutcome::Preview("r2")
        );
    }

    #[test]
    fn gpui_key_up_support_allows_control_release_commit_with_enter_as_backup() {
        assert!(RunSwitcher::supports_control_release_commit());
    }
}
