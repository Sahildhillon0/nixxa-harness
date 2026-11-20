//! The start-up question: where does the agent work?

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::Effect;

/// Where the agent works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// An empty sandbox.
    Sandbox,
    /// The real files of the current directory, with no isolation.
    Here,
    /// The launch directory mounted read-write into the sandbox.
    SandboxHere,
    /// A copy of the launch directory in a scratch directory.
    SandboxCopy,
}

/// Top-level choices.
pub(crate) const TOP_PLACES: [(Place, &str, &str); 3] = [
    (Place::Sandbox, "Sandbox (empty)", "isolated; your files are not visible to it"),
    (Place::Here, "This directory", "your real files, no isolation"),
    (Place::SandboxHere, "Sandbox with this directory", "choose next: work on your files, or a copy"),
];

/// Second question choices.
pub(crate) const SANDBOX_WITH: [(Place, &str, &str); 2] = [
    (Place::SandboxHere, "Work on my files", "changes land in your real files"),
    (Place::SandboxCopy, "Work on a copy", "your real directory is never changed"),
];

#[derive(Debug, Default)]
pub(crate) struct PlacePicker {
    pub(crate) selected: usize,
    pub(crate) stage: u8,
    pub(crate) disabled: Vec<(usize, String)>,
}

impl PlacePicker {
    pub(crate) fn choices(&self) -> &[(Place, &str, &str)] {
        if self.stage == 0 { &TOP_PLACES } else { &SANDBOX_WITH }
    }
    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        let choices = self.choices();
        let last = choices.len() - 1;
        match key.code {
            KeyCode::Esc if self.stage == 1 => {
                self.stage = 0;
                self.selected = 2;
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(last);
                None
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = usize::from(c as u8 - b'1');
                if index < choices.len() && !self.is_disabled(index) {
                    if self.stage == 0 && choices[index].0 == Place::SandboxHere {
                        self.stage = 1;
                        self.selected = 0;
                        return None;
                    }
                    return Some(Effect::ChoosePlace(choices[index].0));
                }
                None
            }
            KeyCode::Enter => {
                if self.is_disabled(self.selected) {
                    return None;
                }
                let choice = choices[self.selected].0;
                if self.stage == 0 && choice == Place::SandboxHere {
                    self.stage = 1;
                    self.selected = 0;
                    return None;
                }
                Some(Effect::ChoosePlace(choice))
            }
            _ => None,
        }
    }
    pub(crate) fn is_disabled(&self, index: usize) -> bool {
        self.disabled.iter().any(|(i, _)| *i == index)
    }
    pub(crate) fn reason(&self, index: usize) -> Option<&str> {
        self.disabled.iter().find(|(i, _)| *i == index).map(|(_, r)| r.as_str())
    }
}
