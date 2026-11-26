use crate::{
    conflict::{Answer, Decision},
    operations::{Progress, TransferError},
};
use std::path::Path;

pub(super) struct ConflictBatch {
    seen: usize,
    expected: usize,
    saved: Option<Decision>,
}

impl ConflictBatch {
    pub(super) fn new(expected: usize) -> Self {
        Self {
            seen: 0,
            expected,
            saved: None,
        }
    }

    pub(super) fn resolve(
        &mut self,
        progress: &mut Progress,
        source: &Path,
        can_replace: bool,
        prompt: impl FnOnce(usize, usize) -> Result<Answer, TransferError>,
    ) -> Result<Decision, TransferError> {
        self.seen += 1;
        if let Some(saved) = self.saved
            && (saved != Decision::Replace || can_replace)
        {
            return Ok(saved);
        }

        progress.waiting_for_conflict(source);
        // A destination may have appeared since the initial count.
        let answer = prompt(self.seen, self.expected.max(self.seen))?;
        // Stop takes precedence even if a reply arrived at the same time.
        progress.check_cancelled()?;
        progress.item(source);
        if answer.apply_to_group {
            self.saved = Some(answer.decision);
        }
        Ok(answer.decision)
    }
}
