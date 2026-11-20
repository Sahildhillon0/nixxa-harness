//! The start-up question: where does the agent work? Asked on every launch, before a session
//! starts (D-039, amended by D-047).
//!
//! Three choices: an empty sandbox, this directory without isolation, or this directory inside
//! the sandbox, which asks a second question: work on the real files, or on a copy.
//!
//! No I/O here: choosing produces an [`Effect::ChoosePlace`] the UI loop carries out. A place
//! that can't be used is shown with the reason and can't be picked.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::Effect;

/// Where the agent works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// The sandbox, on an empty scratch directory: the user's files aren't visible.
    Sandbox,
    /// The real files of the current directory, with no isolation.
    Here,
    /// The current directory mounted read-write into the sandbox.
    SandboxHere,
    /// A copy of the current directory, in the sandbox; the real directory never changes.
    SandboxCopy,
}

impl Place {
    /// Whether the place runs in the isolated sandbox.
    pub(crate) fn sandboxed(self) -> bool {
        !matches!(self, Self::Here)
    }
}

/// One line of the picker: what picking it does, its name and what it means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Row {
    /// Picks this place.
    Pick(Place),
    /// Opens the second question.
    WithThisDirectory,
}

/// The first question.
pub(crate) const FIRST: [(Row, &str, &str); 3] = [
    (Row::Pick(Place::Sandbox), "Sandbox (empty)", "isolated, on an empty folder; your files aren't visible"),
    (Row::Pick(Place::Here), "This directory", "your real files, with no isolation"),
    (Row::WithThisDirectory, "Sandbox with this directory", "isolated; next, choose your files or a copy"),
];

/// The second question, after "Sandbox with this directory".
pub(crate) const SECOND: [(Row, &str, &str); 2] = [
    (Row::Pick(Place::SandboxHere), "Work on my files", "changes land in your real files; nothing else is reachable"),
    (Row::Pick(Place::SandboxCopy), "Work on a copy", "build and test freely; your real directory never changes"),
];

/// The picker on screen.
#[derive(Debug, Default)]
pub(crate) struct PlacePicker {
    pub(crate) selected: usize,
    /// Whether the second question is showing.
    pub(crate) second: bool,
    /// Places that can't be used, and why.
    blocked: Vec<(Place, String)>,
}

impl PlacePicker {
    /// A picker where the places in `blocked` can't be picked. The first usable line starts
    /// selected.
    pub(crate) fn new(blocked: Vec<(Place, String)>) -> Self {
        let mut picker = Self { selected: 0, second: false, blocked };
        picker.selected = picker.first_usable();
        picker
    }

    /// The lines of the question showing.
    pub(crate) fn rows(&self) -> &'static [(Row, &'static str, &'static str)] {
        if self.second { &SECOND } else { &FIRST }
    }

    /// Why the line at `index` can't be picked, or `None` when it can.
    pub(crate) fn reason(&self, index: usize) -> Option<&str> {
        match self.rows().get(index)?.0 {
            Row::Pick(place) => self.blocked_reason(place),
            // Usable while either of its places is.
            Row::WithThisDirectory => match self.blocked_reason(Place::SandboxHere) {
                Some(why) if self.blocked_reason(Place::SandboxCopy).is_some() => Some(why),
                _ => None,
            },
        }
    }

    fn blocked_reason(&self, place: Place) -> Option<&str> {
        self.blocked.iter().find(|(p, _)| *p == place).map(|(_, why)| why.as_str())
    }

    fn first_usable(&self) -> usize {
        (0..self.rows().len()).find(|&i| self.reason(i).is_none()).unwrap_or(0)
    }

    /// Handles one key; picking a place closes the picker.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        let last = self.rows().len() - 1;
        match key.code {
            KeyCode::Esc if self.second => {
                self.second = false;
                self.selected = 2;
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1).min(last),
            KeyCode::Char(c @ '1'..='9') => {
                let index = usize::from(c as u8 - b'1');
                if index <= last {
                    self.selected = index;
                    return self.choose(index);
                }
            }
            KeyCode::Enter => return self.choose(self.selected),
            _ => {}
        }
        None
    }

    fn choose(&mut self, index: usize) -> Option<Effect> {
        if self.reason(index).is_some() {
            return None;
        }
        match self.rows()[index].0 {
            Row::Pick(place) => Some(Effect::ChoosePlace(place)),
            Row::WithThisDirectory => {
                self.second = true;
                self.selected = self.first_usable();
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;

    fn press(picker: &mut PlacePicker, code: KeyCode) -> Option<Effect> {
        picker.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn the_second_question_opens_and_esc_goes_back() {
        let mut picker = PlacePicker::new(Vec::new());
        assert_eq!(press(&mut picker, KeyCode::Char('3')), None);
        assert!(picker.second);
        assert_eq!(press(&mut picker, KeyCode::Esc), None);
        assert!(!picker.second);
        assert_eq!(picker.selected, 2, "back on the line that opened it");
        press(&mut picker, KeyCode::Enter);
        assert_eq!(press(&mut picker, KeyCode::Down), None);
        assert_eq!(press(&mut picker, KeyCode::Enter), Some(Effect::ChoosePlace(Place::SandboxCopy)));
    }

    #[test]
    fn every_place_can_be_picked_by_number() {
        let pick = |keys: &[char]| {
            let mut picker = PlacePicker::new(Vec::new());
            keys.iter().map(|c| press(&mut picker, KeyCode::Char(*c))).last().flatten()
        };
        assert_eq!(pick(&['1']), Some(Effect::ChoosePlace(Place::Sandbox)));
        assert_eq!(pick(&['2']), Some(Effect::ChoosePlace(Place::Here)));
        assert_eq!(pick(&['3', '1']), Some(Effect::ChoosePlace(Place::SandboxHere)));
        assert_eq!(pick(&['3', '2']), Some(Effect::ChoosePlace(Place::SandboxCopy)));
        assert_eq!(pick(&['4']), None);
    }

    #[test]
    fn blocked_places_show_why_and_cannot_be_picked() {
        let blocked = vec![
            (Place::Sandbox, "bubblewrap is missing".to_string()),
            (Place::SandboxHere, "bubblewrap is missing".to_string()),
            (Place::SandboxCopy, "bubblewrap is missing".to_string()),
        ];
        let mut picker = PlacePicker::new(blocked);
        assert_eq!(picker.selected, 1, "starts on the first usable line");
        assert_eq!(picker.reason(0), Some("bubblewrap is missing"));
        assert_eq!(picker.reason(1), None);
        assert_eq!(picker.reason(2), Some("bubblewrap is missing"), "both of its places are blocked");
        assert_eq!(press(&mut picker, KeyCode::Char('1')), None);
        assert_eq!(press(&mut picker, KeyCode::Char('3')), None);
        assert!(!picker.second);
        assert_eq!(press(&mut picker, KeyCode::Char('2')), Some(Effect::ChoosePlace(Place::Here)));
    }

    #[test]
    fn one_blocked_sub_choice_leaves_the_other() {
        let mut picker = PlacePicker::new(vec![(Place::SandboxCopy, "too large to copy".to_string())]);
        assert_eq!(picker.reason(2), None);
        press(&mut picker, KeyCode::Char('3'));
        assert_eq!(picker.reason(1), Some("too large to copy"));
        assert_eq!(press(&mut picker, KeyCode::Char('2')), None);
        assert_eq!(press(&mut picker, KeyCode::Char('1')), Some(Effect::ChoosePlace(Place::SandboxHere)));
    }
}
