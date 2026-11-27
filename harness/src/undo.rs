//! `/undo` (D-073): the checkpoints of this run, and the view that shows what going back to one
//! would change before it does.
//!
//! The view opens with the checkpoint before the agent's last action; `←`/`→` step back
//! further (earlier actions, the start of each turn) and forward again. It shows the diff from
//! that checkpoint to now (fetched from the sandbox), and what an undo can't bring back. `enter`
//! asks once more; a second `enter` sends [`Command::Undo`](nixxa_controller::Command::Undo),
//! which the controller runs between turns. Everything shown that came from the workspace or
//! the agent (paths, diffs, link targets, commands) is drawn through `safe::line`.
//!
//! It is a small, replaceable piece: `/undo` is the only slash command until Stage 11's command
//! system.

use crossterm::event::{KeyCode, KeyEvent};
use nixxa_contracts::{CheckpointId, WorkspaceDiff};

/// What the user typed to open the view.
pub(crate) const COMMAND: &str = "/undo";

/// One checkpoint the controller took, with what it was taken before.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Taken {
    id: CheckpointId,
    label: String,
}

/// An action or turn that has no checkpoint (the store was full, or it failed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoCheckpoint {
    /// How many checkpoints had been taken before it.
    after: usize,
    pub(crate) label: String,
    pub(crate) reason: String,
}

/// The checkpoints of this run, oldest first.
#[derive(Debug, Default)]
pub(crate) struct Checkpoints {
    taken: Vec<Taken>,
    missing: Vec<NoCheckpoint>,
}

/// A point the user can go back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) id: CheckpointId,
    /// What it comes before ("before `edit src/main.rs`"), not yet made safe to draw.
    pub(crate) label: String,
    /// Its position among the distinct checkpoints, oldest first.
    index: usize,
}

impl Checkpoints {
    /// The controller took `id` before what `label` says.
    pub(crate) fn taken(&mut self, id: CheckpointId, label: String) {
        self.taken.push(Taken { id, label });
    }

    /// No checkpoint could be taken before what `label` says.
    pub(crate) fn missing(&mut self, label: String, reason: String) {
        self.missing.push(NoCheckpoint { after: self.distinct().len(), label, reason });
    }

    /// The newest checkpoint taken: the user has seen everything up to it.
    pub(crate) fn newest(&self) -> Option<&CheckpointId> {
        self.taken.last().map(|taken| &taken.id)
    }

    /// The checkpoints, one per distinct id (the sandbox gives the same id again when nothing
    /// changed: the newest label is kept, and the others are added to it).
    fn distinct(&self) -> Vec<Choice> {
        let mut out: Vec<Choice> = Vec::new();
        for taken in &self.taken {
            match out.last_mut() {
                Some(last) if last.id == taken.id => {
                    last.label = format!("{}, and {}", taken.label, last.label);
                }
                _ => out.push(Choice { id: taken.id.clone(), label: taken.label.clone(), index: out.len() }),
            }
        }
        out
    }

    /// The points the user can go back to, newest first.
    pub(crate) fn choices(&self) -> Vec<Choice> {
        let mut choices = self.distinct();
        choices.reverse();
        choices
    }

    /// The actions after `choice` that had no checkpoint: going back to `choice` undoes them
    /// too, but nothing can go back to just before one of them.
    pub(crate) fn missing_after(&self, choice: &Choice) -> Vec<&NoCheckpoint> {
        self.missing.iter().filter(|missing| missing.after > choice.index).collect()
    }
}

/// The diff to now from the chosen checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Diff {
    Loading,
    Ready(WorkspaceDiff),
    Failed(String),
}

/// The open undo view.
#[derive(Debug)]
pub(crate) struct UndoView {
    pub(crate) choices: Vec<Choice>,
    pub(crate) selected: usize,
    pub(crate) diff: Diff,
    /// The newest checkpoint when the view opened: the controller refuses the undo if a newer
    /// one was taken since.
    pub(crate) seen: Option<CheckpointId>,
    /// The second `enter` restores.
    pub(crate) confirming: bool,
    /// Lines scrolled down in the view.
    pub(crate) scroll: usize,
}

/// What a key in the view asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ViewKey {
    Nothing,
    /// Show the diff from this checkpoint to now.
    Diff(CheckpointId),
    /// The user confirmed: go back to this checkpoint.
    Undo {
        to: CheckpointId,
        seen: Option<CheckpointId>,
    },
    Close,
}

impl UndoView {
    /// The view on `choices` (newest first, not empty), starting with the newest.
    pub(crate) fn new(choices: Vec<Choice>, seen: Option<CheckpointId>) -> Self {
        Self { choices, selected: 0, diff: Diff::Loading, seen, confirming: false, scroll: 0 }
    }

    pub(crate) fn chosen(&self) -> &Choice {
        &self.choices[self.selected]
    }

    /// The diff for checkpoint `id` arrived; it is shown if that is still the one chosen.
    pub(crate) fn diff_ready(&mut self, id: &CheckpointId, result: Result<WorkspaceDiff, String>) {
        if &self.chosen().id == id {
            self.diff = match result {
                Ok(diff) => Diff::Ready(diff),
                Err(why) => Diff::Failed(why),
            };
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> ViewKey {
        if self.confirming {
            return match key.code {
                KeyCode::Enter => ViewKey::Undo { to: self.chosen().id.clone(), seen: self.seen.clone() },
                KeyCode::Esc => {
                    self.confirming = false;
                    ViewKey::Nothing
                }
                _ => ViewKey::Nothing,
            };
        }
        match key.code {
            KeyCode::Esc => ViewKey::Close,
            KeyCode::Left | KeyCode::Char('h') if self.selected + 1 < self.choices.len() => {
                self.select(self.selected + 1)
            }
            KeyCode::Right | KeyCode::Char('l') if self.selected > 0 => self.select(self.selected - 1),
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                ViewKey::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll += 1;
                ViewKey::Nothing
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                ViewKey::Nothing
            }
            KeyCode::PageDown => {
                self.scroll += 10;
                ViewKey::Nothing
            }
            // Only once there is something to confirm.
            KeyCode::Enter if matches!(&self.diff, Diff::Ready(diff) if !diff.is_empty()) => {
                self.confirming = true;
                ViewKey::Nothing
            }
            _ => ViewKey::Nothing,
        }
    }

    fn select(&mut self, index: usize) -> ViewKey {
        self.selected = index;
        self.diff = Diff::Loading;
        self.scroll = 0;
        ViewKey::Diff(self.chosen().id.clone())
    }
}

/// How a change reads in the view and in the undo note, from the checkpoint's side: what
/// going back does to the path.
pub(crate) fn what_undo_does(change: &nixxa_contracts::FileChange) -> &'static str {
    match (&change.before, &change.after) {
        (None, _) => "remove",
        (_, None) => "restore",
        _ => "revert",
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;
    use nixxa_contracts::{EntryState, FileChange};

    use super::*;

    fn id(n: &str) -> CheckpointId {
        CheckpointId::new(n)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn diff() -> WorkspaceDiff {
        let file = Some(EntryState::File { size: 1, mode: 0o644, hash: "h".into() });
        WorkspaceDiff {
            changes: vec![FileChange { path: "a".into(), before: file.clone(), after: None, diff: None }],
            not_covered: Vec::new(),
        }
    }

    #[test]
    fn choices_are_newest_first_with_repeated_ids_merged() {
        let mut checkpoints = Checkpoints::default();
        checkpoints.taken(id("c1"), "the start of the turn".into());
        checkpoints.taken(id("c1"), "before `rm -rf src`".into());
        checkpoints.taken(id("c2"), "before `edit a.rs`".into());
        let choices = checkpoints.choices();
        assert_eq!(choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c2", "c1"]);
        assert_eq!(choices[1].label, "before `rm -rf src`, and the start of the turn");
        assert_eq!(checkpoints.newest(), Some(&id("c2")));
    }

    #[test]
    fn actions_without_a_checkpoint_are_named_for_the_points_before_them() {
        let mut checkpoints = Checkpoints::default();
        checkpoints.taken(id("c1"), "one".into());
        checkpoints.missing("before `big`".into(), "the store is full".into());
        checkpoints.taken(id("c2"), "two".into());
        let choices = checkpoints.choices();
        assert!(checkpoints.missing_after(&choices[0]).is_empty(), "c2 came after it");
        assert_eq!(checkpoints.missing_after(&choices[1])[0].reason, "the store is full");
    }

    #[test]
    fn the_view_steps_back_and_forth_and_asks_before_it_restores() {
        let mut checkpoints = Checkpoints::default();
        checkpoints.taken(id("c1"), "one".into());
        checkpoints.taken(id("c2"), "two".into());
        let mut view = UndoView::new(checkpoints.choices(), checkpoints.newest().cloned());
        assert_eq!(view.chosen().id, id("c2"));
        assert_eq!(view.key(key(KeyCode::Enter)), ViewKey::Nothing, "nothing to confirm while loading");
        assert!(!view.confirming);
        assert_eq!(view.key(key(KeyCode::Left)), ViewKey::Diff(id("c1")));
        assert_eq!(view.key(key(KeyCode::Left)), ViewKey::Nothing, "no further back");
        // A diff for a checkpoint no longer chosen is dropped.
        view.diff_ready(&id("c2"), Ok(diff()));
        assert_eq!(view.diff, Diff::Loading);
        view.diff_ready(&id("c1"), Ok(diff()));
        assert_eq!(view.key(key(KeyCode::Enter)), ViewKey::Nothing);
        assert!(view.confirming, "the first enter asks");
        assert_eq!(view.key(key(KeyCode::Esc)), ViewKey::Nothing, "esc backs out of the question");
        view.key(key(KeyCode::Enter));
        assert_eq!(view.key(key(KeyCode::Enter)), ViewKey::Undo { to: id("c1"), seen: Some(id("c2")) });
        let mut view = UndoView::new(checkpoints.choices(), None);
        view.diff_ready(&id("c2"), Ok(WorkspaceDiff::default()));
        view.key(key(KeyCode::Enter));
        assert!(!view.confirming, "an empty diff has nothing to undo");
        assert_eq!(view.key(key(KeyCode::Esc)), ViewKey::Close);
    }
}
