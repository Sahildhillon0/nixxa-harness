//! The setup screen: choose how Nixxa thinks, then set it up.
//!
//! 1. **Anthropic API key:** paste a key (masked, last four characters only), check it with
//!    Anthropic, store it in the keychain.
//! 2. **Local model:** the URL of an OpenAI-compatible server on this machine (LM Studio, Ollama),
//!    then a model it offers.
//!
//! No I/O here: keys produce [`Effect`]s the UI loop carries out, and results come back through
//! [`Setup::models_listed`] and [`Setup::failed`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nixxa_brain::ModelInfo;
use nixxa_contracts::Secret;
use zeroize::Zeroizing;

use crate::app::{Effect, types_text};

/// Where LM Studio serves by default.
pub(crate) const LM_STUDIO_URL: &str = "http://localhost:1234/v1";
/// Where Ollama serves by default.
pub(crate) const OLLAMA_URL: &str = "http://localhost:11434/v1";

/// The choices on the first step, in order.
pub(crate) const CHOICES: [&str; 2] = ["Anthropic API key", "Local model"];

/// Where a form is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupState {
    Editing,
    Checking,
    Failed(String),
}

/// The API-key form. The typed key is zeroised when the form goes away.
pub(crate) struct KeyForm {
    key: Zeroizing<String>,
    pub(crate) state: SetupState,
}

impl KeyForm {
    fn new() -> Self {
        Self { key: Zeroizing::new(String::new()), state: SetupState::Editing }
    }

    /// The key as shown: bullets, and the last four characters once it is long enough to be a
    /// key. Never more.
    pub(crate) fn masked(&self) -> String {
        let n = self.key.chars().count();
        if n < 8 {
            return "•".repeat(n);
        }
        let tail: String = self.key.chars().skip(n - 4).collect();
        format!("{}{tail}", "•".repeat((n - 4).min(32)))
    }
}

/// The local server URL form.
pub(crate) struct UrlForm {
    pub(crate) url: String,
    pub(crate) state: SetupState,
}

/// The local model picker: the server's chat models.
pub(crate) struct ModelPicker {
    pub(crate) url: String,
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) selected: usize,
    pub(crate) state: SetupState,
}

/// One step of the setup screen.
pub(crate) enum Step {
    Choose { selected: usize },
    Key(KeyForm),
    LocalUrl(UrlForm),
    LocalModel(ModelPicker),
}

/// The setup screen.
pub(crate) struct Setup {
    /// No account exists yet: the screen can't be dismissed, only completed (or quit).
    pub(crate) first_run: bool,
    pub(crate) step: Step,
}

/// What a key did to the setup screen.
pub(crate) struct Outcome {
    pub(crate) effects: Vec<Effect>,
    /// The screen should close.
    pub(crate) close: bool,
}

impl Outcome {
    fn none() -> Self {
        Self { effects: Vec::new(), close: false }
    }

    fn effect(effect: Effect) -> Self {
        Self { effects: vec![effect], close: false }
    }
}

impl Setup {
    /// The first step: choose how Nixxa thinks.
    pub(crate) fn new(first_run: bool) -> Self {
        Self { first_run, step: Step::Choose { selected: 0 } }
    }

    /// Straight to the local server form (e.g. a local account whose model is unknown).
    pub(crate) fn local(first_run: bool, url: &str) -> Self {
        Self { first_run, step: Step::LocalUrl(UrlForm { url: url.to_string(), state: SetupState::Editing }) }
    }

    /// The current form's state, if the step has one.
    pub(crate) fn state(&self) -> Option<&SetupState> {
        match &self.step {
            Step::Choose { .. } => None,
            Step::Key(form) => Some(&form.state),
            Step::LocalUrl(form) => Some(&form.state),
            Step::LocalModel(picker) => Some(&picker.state),
        }
    }

    fn checking(&self) -> bool {
        self.state() == Some(&SetupState::Checking)
    }

    /// Saving failed (or the server couldn't be read): show why on the current form.
    pub(crate) fn failed(&mut self, message: &str) {
        let failed = SetupState::Failed(message.to_string());
        match &mut self.step {
            Step::Choose { .. } => {}
            Step::Key(form) => form.state = failed,
            Step::LocalUrl(form) => form.state = failed,
            Step::LocalModel(picker) => picker.state = failed,
        }
    }

    /// The server's models arrived (or why not): move on to the picker, chat models only.
    pub(crate) fn models_listed(&mut self, result: Result<Vec<ModelInfo>, String>) {
        let Step::LocalUrl(form) = &mut self.step else { return };
        let models = match result {
            Ok(models) => models,
            Err(message) => {
                form.state = SetupState::Failed(message);
                return;
            }
        };
        // Embedding models can't chat, and a model the server says can't use tools can't act.
        let usable: Vec<ModelInfo> = models
            .into_iter()
            .filter(|m| !m.id.to_lowercase().contains("embed") && m.supports_tools != Some(false))
            .collect();
        if usable.is_empty() {
            form.state = SetupState::Failed(
                "The server offers no chat model that can use tools. Load one (e.g. gpt-oss, qwen3) and try again."
                    .into(),
            );
            return;
        }
        let url = form.url.clone();
        self.step = Step::LocalModel(ModelPicker { url, models: usable, selected: 0, state: SetupState::Editing });
    }

    /// Pasted text goes into the current text field.
    pub(crate) fn paste(&mut self, text: &str) {
        if self.checking() {
            return;
        }
        match &mut self.step {
            Step::Key(form) => {
                // Keys are one line: whatever a paste brings, keep only printable characters.
                form.key.extend(text.chars().filter(|c| !c.is_control() && !c.is_whitespace()));
                form.state = SetupState::Editing;
            }
            Step::LocalUrl(form) => {
                form.url.extend(text.chars().filter(|c| !c.is_control() && !c.is_whitespace()));
                form.state = SetupState::Editing;
            }
            Step::Choose { .. } | Step::LocalModel(_) => {}
        }
    }

    /// Handles one key press.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Outcome {
        let checking = self.checking();
        if key.code == KeyCode::Esc && !checking {
            return self.back();
        }
        if checking {
            return Outcome::none();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.step {
            Step::Choose { selected } => match key.code {
                KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(CHOICES.len() - 1),
                KeyCode::Char('1') => self.step = Step::Key(KeyForm::new()),
                KeyCode::Char('2') => {
                    self.step = Step::LocalUrl(UrlForm { url: LM_STUDIO_URL.into(), state: SetupState::Editing })
                }
                KeyCode::Enter => {
                    self.step = if *selected == 0 {
                        Step::Key(KeyForm::new())
                    } else {
                        Step::LocalUrl(UrlForm { url: LM_STUDIO_URL.into(), state: SetupState::Editing })
                    };
                }
                _ => {}
            },
            Step::Key(form) => match key.code {
                KeyCode::Char('u') if ctrl => {
                    form.key = Zeroizing::new(String::new());
                    form.state = SetupState::Editing;
                }
                KeyCode::Char(c) if types_text(&key) => {
                    if !c.is_whitespace() {
                        form.key.push(c);
                    }
                    form.state = SetupState::Editing;
                }
                KeyCode::Backspace => {
                    form.key.pop();
                    form.state = SetupState::Editing;
                }
                KeyCode::Enter if form.key.is_empty() => form.state = SetupState::Failed("Paste a key first.".into()),
                KeyCode::Enter => {
                    form.state = SetupState::Checking;
                    return Outcome::effect(Effect::SaveKey(Secret::new(form.key.as_str())));
                }
                _ => {}
            },
            Step::LocalUrl(form) => match key.code {
                KeyCode::Char('u') if ctrl => {
                    form.url.clear();
                    form.state = SetupState::Editing;
                }
                KeyCode::Char(c) if types_text(&key) => {
                    if !c.is_whitespace() {
                        form.url.push(c);
                    }
                    form.state = SetupState::Editing;
                }
                KeyCode::Backspace => {
                    form.url.pop();
                    form.state = SetupState::Editing;
                }
                KeyCode::Enter if form.url.is_empty() => {
                    form.state = SetupState::Failed(format!("Enter the server's URL, e.g. {LM_STUDIO_URL}."));
                }
                KeyCode::Enter => {
                    form.state = SetupState::Checking;
                    return Outcome::effect(Effect::ListModels(form.url.clone()));
                }
                _ => {}
            },
            Step::LocalModel(picker) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1);
                    picker.state = SetupState::Editing;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected = (picker.selected + 1).min(picker.models.len().saturating_sub(1));
                    picker.state = SetupState::Editing;
                }
                KeyCode::Enter => {
                    if let Some(model) = picker.models.get(picker.selected) {
                        picker.state = SetupState::Checking;
                        return Outcome::effect(Effect::SaveLocal { url: picker.url.clone(), model: model.id.clone() });
                    }
                }
                _ => {}
            },
        }
        Outcome::none()
    }

    /// `esc`: one step back; from the first step, close (unless nothing is set up yet).
    fn back(&mut self) -> Outcome {
        self.step = match &self.step {
            Step::Choose { .. } => return Outcome { effects: Vec::new(), close: !self.first_run },
            Step::Key(_) => Step::Choose { selected: 0 },
            Step::LocalUrl(_) => Step::Choose { selected: 1 },
            Step::LocalModel(picker) => Step::LocalUrl(UrlForm { url: picker.url.clone(), state: SetupState::Editing }),
        };
        Outcome::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::key;

    fn model(id: &str, supports_tools: Option<bool>) -> ModelInfo {
        ModelInfo { id: id.into(), supports_tools, free: None }
    }

    fn type_text(setup: &mut Setup, text: &str) {
        for c in text.chars() {
            setup.key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn the_first_step_offers_both_choices() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Down));
        assert!(matches!(setup.step, Step::Choose { selected: 1 }));
        setup.key(key(KeyCode::Enter));
        let Step::LocalUrl(form) = &setup.step else { panic!("the local form") };
        assert_eq!(form.url, LM_STUDIO_URL, "LM Studio's address is filled in");
        setup.key(key(KeyCode::Esc));
        setup.key(key(KeyCode::Char('1')));
        assert!(matches!(setup.step, Step::Key(_)));
    }

    #[test]
    fn esc_goes_back_a_step_and_closes_only_when_something_is_set_up() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('2')));
        assert!(!setup.key(key(KeyCode::Esc)).close);
        assert!(matches!(setup.step, Step::Choose { .. }));
        assert!(!setup.key(key(KeyCode::Esc)).close, "first run: nothing to go back to");
        assert!(Setup::new(false).key(key(KeyCode::Esc)).close);
    }

    #[test]
    fn the_url_form_lists_the_servers_models() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('2')));
        setup.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        type_text(&mut setup, "http://localhost:11434/v1");
        let outcome = setup.key(key(KeyCode::Enter));
        assert_eq!(outcome.effects, vec![Effect::ListModels(OLLAMA_URL.into())]);
        assert_eq!(setup.state(), Some(&SetupState::Checking));
        type_text(&mut setup, "zz");
        let Step::LocalUrl(form) = &setup.step else { panic!() };
        assert_eq!(form.url, OLLAMA_URL, "no typing while checking");
    }

    #[test]
    fn the_picker_shows_chat_models_that_can_use_tools() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('2')));
        setup.key(key(KeyCode::Enter));
        setup.models_listed(Ok(vec![
            model("openai/gpt-oss-20b", None),
            model("text-embedding-nomic-embed-text-v1.5", None),
            model("no-tools-model", Some(false)),
            model("qwen3-8b", Some(true)),
        ]));
        let Step::LocalModel(picker) = &setup.step else { panic!("the picker") };
        let ids: Vec<_> = picker.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["openai/gpt-oss-20b", "qwen3-8b"]);
        setup.key(key(KeyCode::Down));
        let outcome = setup.key(key(KeyCode::Enter));
        assert_eq!(outcome.effects, vec![Effect::SaveLocal { url: LM_STUDIO_URL.into(), model: "qwen3-8b".into() }]);
    }

    #[test]
    fn an_unreachable_server_or_no_usable_model_is_explained() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('2')));
        setup.key(key(KeyCode::Enter));
        setup.models_listed(Err("couldn't reach http://localhost:1234/v1".into()));
        assert_eq!(setup.state(), Some(&SetupState::Failed("couldn't reach http://localhost:1234/v1".into())));
        setup.key(key(KeyCode::Enter));
        setup.models_listed(Ok(vec![model("nomic-embed-text", None)]));
        assert!(matches!(setup.state(), Some(SetupState::Failed(m)) if m.contains("no chat model")));
    }

    #[test]
    fn a_failed_save_stays_on_the_picker_and_esc_returns_to_the_url() {
        let mut setup = Setup::local(false, LM_STUDIO_URL);
        setup.key(key(KeyCode::Enter));
        setup.models_listed(Ok(vec![model("m", None)]));
        setup.key(key(KeyCode::Enter));
        setup.failed("`m` can't use tools.");
        assert!(
            matches!(&setup.step, Step::LocalModel(p) if p.state == SetupState::Failed("`m` can't use tools.".into()))
        );
        setup.key(key(KeyCode::Esc));
        assert!(matches!(&setup.step, Step::LocalUrl(f) if f.url == LM_STUDIO_URL));
    }
}
