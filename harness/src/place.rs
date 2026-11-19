//! The start-up question: where does the agent work? In the sandbox, or directly in the
//! directory Nixxa was started in (D-039). Asked on every launch, before a session starts.
//!
//! No I/O here: choosing produces an [`Effect::ChoosePlace`] the UI loop carries out.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::Effect;

/// Where the agent works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// The sandbox: nothing on the host changes.
    Sandbox,
    /// The real files of the current directory, with no isolation.
    Here,
}

/// The choices, in order: the place, its name and what it means.
pub(crate) const PLACES: [(Place, &str, &str); 2] = [
    (Place::Sandbox, "Sandbox", "kept apart from your machine; your files are never read or changed"),
    (Place::Here, "This directory", "the agent reads, edits and runs commands in your real files"),
];

/// The picker on screen.
#[derive(Debug, Default)]
pub(crate) struct PlacePicker {
    pub(crate) selected: usize,
}

impl PlacePicker {
    /// Handles one key; a choice closes the picker.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        let last = PLACES.len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::Char(c @ '1'..='9') => {
                let index = usize::from(c as u8 - b'1');
                return PLACES.get(index).map(|(place, ..)| Effect::ChoosePlace(*place));
            }
            KeyCode::Enter => return Some(Effect::ChoosePlace(PLACES[self.selected].0)),
            _ => {}
        }
        None
    }
}
