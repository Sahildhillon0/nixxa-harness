//! The network settings (`ctrl+n`, D-065): Off, Allow (presets and the user's own hosts) or
//! Open, and the recent requests. Per session, never saved; every session starts Off. No I/O
//! here: a change comes back as the policy to apply, and the TUI loop applies it.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nixxa_contracts::{DomainRule, NetRequest, NetworkAccess, NetworkPolicy};
use nixxa_sandbox::network::{PRESETS, Preset};

use crate::app::types_text;

/// Requests kept for the panel.
const KEPT_REQUESTS: usize = 200;

/// The three modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    Allow,
    Open,
}

/// What a key in the panel asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelKey {
    Nothing,
    Close,
    /// Apply this policy.
    Apply(NetworkPolicy),
}

/// The panel's state. It lives for the whole session (closing it keeps the choices).
#[derive(Debug)]
pub(crate) struct NetworkPanel {
    pub(crate) open: bool,
    pub(crate) mode: Mode,
    /// Which presets are on, in [`PRESETS`] order.
    pub(crate) presets: Vec<bool>,
    /// Hosts the user added.
    pub(crate) hosts: Vec<DomainRule>,
    /// The selected row: presets first, then the user's hosts.
    pub(crate) selected: usize,
    /// The host being typed, while adding one.
    pub(crate) adding: Option<String>,
    /// Why the last change didn't happen.
    pub(crate) error: Option<String>,
    /// Waiting for a second `3` to confirm Open.
    pub(crate) confirm_open: bool,
    /// Newest last.
    pub(crate) requests: VecDeque<NetRequest>,
}

impl Default for NetworkPanel {
    fn default() -> Self {
        Self {
            open: false,
            mode: Mode::Off,
            presets: vec![false; PRESETS.len()],
            hosts: Vec::new(),
            selected: 0,
            adding: None,
            error: None,
            confirm_open: false,
            requests: VecDeque::new(),
        }
    }
}

impl NetworkPanel {
    /// The rows: each preset, then each added host.
    pub(crate) fn rows(&self) -> usize {
        PRESETS.len() + self.hosts.len()
    }

    /// The policy the panel describes.
    pub(crate) fn policy(&self) -> NetworkPolicy {
        match self.mode {
            Mode::Off => NetworkPolicy::Off,
            Mode::Open => NetworkPolicy::Open,
            Mode::Allow => NetworkPolicy::Allow(self.rules()),
        }
    }

    /// The allowed hosts: the presets that are on, then the added ones, without repeats.
    fn rules(&self) -> Vec<DomainRule> {
        let mut rules: Vec<DomainRule> = Vec::new();
        let chosen = PRESETS.iter().zip(&self.presets).filter(|(_, on)| **on).flat_map(|(preset, _)| preset.rules());
        for rule in chosen.chain(self.hosts.iter().cloned()) {
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
        rules
    }

    /// A request was made (allowed or blocked).
    pub(crate) fn record(&mut self, request: NetRequest) {
        if self.requests.len() == KEPT_REQUESTS {
            self.requests.pop_front();
        }
        self.requests.push_back(request);
    }

    /// A key while the panel is open. `access` is what the sandbox reports: a backend that
    /// can't restrict the network only closes.
    pub(crate) fn key(&mut self, key: KeyEvent, access: Option<&NetworkAccess>) -> PanelKey {
        if matches!(access, Some(NetworkAccess::Unrestricted) | None) {
            return if matches!(key.code, KeyCode::Esc) || ctrl(&key, 'n') { self.close() } else { PanelKey::Nothing };
        }
        if let Some(typed) = self.adding.as_mut() {
            match key.code {
                KeyCode::Esc => self.adding = None,
                KeyCode::Enter => return self.add_typed(),
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => typed.clear(),
                KeyCode::Char(c) if types_text(&key) && !c.is_whitespace() => typed.push(c),
                _ => {}
            }
            return PanelKey::Nothing;
        }
        let confirming = std::mem::take(&mut self.confirm_open);
        match key.code {
            KeyCode::Esc => return self.close(),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => return self.close(),
            KeyCode::Char('1') => return self.set_mode(Mode::Off),
            KeyCode::Char('2') => return self.set_mode(Mode::Allow),
            KeyCode::Char('3') if self.mode == Mode::Open => {}
            KeyCode::Char('3') if confirming => return self.set_mode(Mode::Open),
            KeyCode::Char('3') => self.confirm_open = true,
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows().saturating_sub(1))
            }
            KeyCode::Char(' ') | KeyCode::Enter => return self.toggle_selected(),
            KeyCode::Char('a') => {
                self.error = None;
                self.adding = Some(String::new());
            }
            KeyCode::Char('d') | KeyCode::Delete => return self.remove_selected(),
            _ => {}
        }
        PanelKey::Nothing
    }

    fn close(&mut self) -> PanelKey {
        self.open = false;
        self.adding = None;
        self.confirm_open = false;
        PanelKey::Close
    }

    fn set_mode(&mut self, mode: Mode) -> PanelKey {
        self.error = None;
        if self.mode == mode {
            return PanelKey::Nothing;
        }
        self.mode = mode;
        PanelKey::Apply(self.policy())
    }

    /// A change to the hosts takes effect at once when they are in use (Allow).
    fn changed(&self) -> PanelKey {
        if self.mode == Mode::Allow { PanelKey::Apply(self.policy()) } else { PanelKey::Nothing }
    }

    fn toggle_selected(&mut self) -> PanelKey {
        let Some(on) = self.presets.get_mut(self.selected) else { return PanelKey::Nothing };
        *on = !*on;
        self.changed()
    }

    fn remove_selected(&mut self) -> PanelKey {
        let Some(index) = self.selected.checked_sub(PRESETS.len()) else { return PanelKey::Nothing };
        if index >= self.hosts.len() {
            return PanelKey::Nothing;
        }
        self.hosts.remove(index);
        self.selected = self.selected.min(self.rows().saturating_sub(1));
        self.changed()
    }

    fn add_typed(&mut self) -> PanelKey {
        let typed = self.adding.take().unwrap_or_default();
        if typed.trim().is_empty() {
            return PanelKey::Nothing;
        }
        match DomainRule::parse(&typed) {
            Ok(rule) if self.hosts.contains(&rule) => PanelKey::Nothing,
            Ok(rule) => {
                self.error = None;
                self.hosts.push(rule);
                self.selected = self.rows() - 1;
                self.changed()
            }
            Err(why) => {
                self.error = Some(why.to_string());
                self.adding = Some(typed);
                PanelKey::Nothing
            }
        }
    }
}

fn ctrl(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

/// The status bar's network part: `net: off`, `net: N domains`, `net: OPEN`.
pub(crate) fn status(access: &NetworkAccess) -> String {
    match access {
        NetworkAccess::Unrestricted => "net: not restricted".into(),
        NetworkAccess::Controlled(NetworkPolicy::Off) => "net: off".into(),
        NetworkAccess::Controlled(NetworkPolicy::Open) => "net: OPEN".into(),
        NetworkAccess::Controlled(NetworkPolicy::Allow(rules)) => match rules.len() {
            1 => "net: 1 domain".into(),
            n => format!("net: {n} domains"),
        },
    }
}

/// The session's record of a change, for the agent and for a resumed session.
pub(crate) fn note(policy: &NetworkPolicy) -> String {
    match policy {
        NetworkPolicy::Off => "The user turned the network off.".into(),
        NetworkPolicy::Open => "The user opened the network to every public host.".into(),
        NetworkPolicy::Allow(rules) if rules.is_empty() => {
            "The user set the network to allow-list mode, with no hosts allowed yet.".into()
        }
        NetworkPolicy::Allow(rules) => {
            let hosts: Vec<String> = rules.iter().map(ToString::to_string).collect();
            format!("The user set the network to allow only: {}.", hosts.join(", "))
        }
    }
}

/// What a preset row shows.
pub(crate) fn preset_hosts(preset: &Preset) -> String {
    preset.hosts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::key;

    fn controlled() -> NetworkAccess {
        NetworkAccess::Controlled(NetworkPolicy::Off)
    }

    fn press(panel: &mut NetworkPanel, code: KeyCode) -> PanelKey {
        panel.key(key(code), Some(&controlled()))
    }

    fn crates() -> Vec<DomainRule> {
        PRESETS[0].rules()
    }

    #[test]
    fn modes_apply_at_once_and_open_needs_a_second_press() {
        let mut panel = NetworkPanel::default();
        assert_eq!(press(&mut panel, KeyCode::Char('2')), PanelKey::Apply(NetworkPolicy::Allow(Vec::new())));
        assert_eq!(press(&mut panel, KeyCode::Char('3')), PanelKey::Nothing, "open waits for confirmation");
        assert!(panel.confirm_open);
        assert_eq!(panel.mode, Mode::Allow);
        assert_eq!(press(&mut panel, KeyCode::Char('3')), PanelKey::Apply(NetworkPolicy::Open));
        assert_eq!(press(&mut panel, KeyCode::Char('1')), PanelKey::Apply(NetworkPolicy::Off));
        // Any other key in between cancels the confirmation.
        press(&mut panel, KeyCode::Char('3'));
        press(&mut panel, KeyCode::Down);
        assert_eq!(press(&mut panel, KeyCode::Char('3')), PanelKey::Nothing);
    }

    #[test]
    fn presets_toggle_and_apply_while_allowing() {
        let mut panel = NetworkPanel::default();
        assert_eq!(press(&mut panel, KeyCode::Char(' ')), PanelKey::Nothing, "off: only remembered");
        assert_eq!(press(&mut panel, KeyCode::Char('2')), PanelKey::Apply(NetworkPolicy::Allow(crates())));
        assert_eq!(press(&mut panel, KeyCode::Char(' ')), PanelKey::Apply(NetworkPolicy::Allow(Vec::new())));
    }

    #[test]
    fn hosts_are_added_checked_and_removed() {
        let mut panel = NetworkPanel::default();
        press(&mut panel, KeyCode::Char('2'));
        press(&mut panel, KeyCode::Char('a'));
        for c in "exa mple".chars() {
            press(&mut panel, KeyCode::Char(c));
        }
        let example = DomainRule::parse("example.org").unwrap();
        assert_eq!(panel.adding.as_deref(), Some("example"), "spaces aren't typed");
        for c in ".org".chars() {
            press(&mut panel, KeyCode::Char(c));
        }
        assert_eq!(press(&mut panel, KeyCode::Enter), PanelKey::Apply(NetworkPolicy::Allow(vec![example])));
        assert_eq!(panel.selected, PRESETS.len());
        assert_eq!(press(&mut panel, KeyCode::Char('d')), PanelKey::Apply(NetworkPolicy::Allow(Vec::new())));
        press(&mut panel, KeyCode::Char('a'));
        for c in "*.com".chars() {
            press(&mut panel, KeyCode::Char(c));
        }
        assert_eq!(press(&mut panel, KeyCode::Enter), PanelKey::Nothing);
        assert!(panel.error.as_deref().is_some_and(|e| e.contains("top-level")), "{:?}", panel.error);
        assert_eq!(panel.adding.as_deref(), Some("*.com"), "kept for fixing");
    }

    #[test]
    fn an_unrestricted_sandbox_only_closes() {
        let mut panel = NetworkPanel { open: true, ..NetworkPanel::default() };
        let host = NetworkAccess::Unrestricted;
        assert_eq!(panel.key(key(KeyCode::Char('3')), Some(&host)), PanelKey::Nothing);
        assert_eq!(panel.key(key(KeyCode::Esc), Some(&host)), PanelKey::Close);
        assert!(!panel.open);
    }

    #[test]
    fn the_status_and_the_note_name_the_policy() {
        assert_eq!(status(&controlled()), "net: off");
        assert_eq!(status(&NetworkAccess::Controlled(NetworkPolicy::Allow(crates()))), "net: 3 domains");
        assert_eq!(status(&NetworkAccess::Controlled(NetworkPolicy::Open)), "net: OPEN");
        assert_eq!(status(&NetworkAccess::Unrestricted), "net: not restricted");
        assert_eq!(
            note(&NetworkPolicy::Allow(crates())),
            "The user set the network to allow only: crates.io, index.crates.io, static.crates.io."
        );
    }
}
