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
pub(crate) const CHOICES: [&str; 3] = ["Anthropic API key", "Local model", "Sign in with OpenRouter"];

/// What the user must accept before signing in with OpenRouter.
pub(crate) const PRIVACY_NOTICE: [&str; 3] = [
    "Everything in the conversation goes to OpenRouter and the model's host: files the agent reads from your workspace, your notes and memory.",
    "Free models may log your prompts or train on them.",
    "Don't use it with private code or anything you wouldn't share.",
];

/// The reminder on OpenRouter's model picker.
pub(crate) const PRIVACY_SHORT: &str =
    "Your conversation goes to OpenRouter and the model's host; free models may log or train on it.";

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

/// Whose models a picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Local,
    OpenRouter,
}

/// A model picker: a server's usable models, filtered by what's typed.
pub(crate) struct ModelPicker {
    pub(crate) source: Source,
    /// The local server's URL (local pickers).
    pub(crate) url: String,
    pub(crate) models: Vec<ModelInfo>,
    /// Typed text narrowing the list (case-insensitive substring).
    pub(crate) filter: String,
    /// Index into [`ModelPicker::visible`].
    pub(crate) selected: usize,
    pub(crate) state: SetupState,
}

impl ModelPicker {
    fn new(source: Source, url: String, models: Vec<ModelInfo>) -> Self {
        Self { source, url, models, filter: String::new(), selected: 0, state: SetupState::Editing }
    }

    /// The models matching the filter.
    pub(crate) fn visible(&self) -> Vec<&ModelInfo> {
        let filter = self.filter.to_lowercase();
        self.models.iter().filter(|m| m.id.to_lowercase().contains(&filter)).collect()
    }
}

/// Waiting for the browser to come back from OpenRouter.
pub(crate) struct SignInWait {
    /// The page to open; `None` until the listener is up.
    pub(crate) url: Option<String>,
    /// Whether the browser was opened for the user.
    pub(crate) browser_opened: bool,
    /// `Checking` while waiting for the browser or loading models.
    pub(crate) state: SetupState,
    /// Signed in; the models are loading.
    pub(crate) signed_in: bool,
}

impl SignInWait {
    fn new() -> Self {
        Self { url: None, browser_opened: false, state: SetupState::Checking, signed_in: false }
    }
}

/// One step of the setup screen.
pub(crate) enum Step {
    Choose {
        selected: usize,
    },
    Key(KeyForm),
    LocalUrl(UrlForm),
    /// A picker of local or OpenRouter models.
    Pick(ModelPicker),
    /// OpenRouter's privacy notice, confirmed before sign-in.
    OpenRouterNotice,
    SigningIn(SignInWait),
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
            Step::Pick(picker) => Some(&picker.state),
            Step::OpenRouterNotice => None,
            Step::SigningIn(wait) => Some(&wait.state),
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
            Step::Pick(picker) => picker.state = failed,
            Step::OpenRouterNotice => {}
            Step::SigningIn(wait) => wait.state = failed,
        }
    }

    /// The sign-in listener is up: the page to open, and whether the browser opened it.
    pub(crate) fn sign_in_started(&mut self, url: String, browser_opened: bool) {
        if let Step::SigningIn(wait) = &mut self.step {
            wait.url = Some(url);
            wait.browser_opened = browser_opened;
        }
    }

    /// The browser came back and a key was issued; the models are loading next.
    pub(crate) fn signed_in(&mut self) {
        if let Step::SigningIn(wait) = &mut self.step {
            wait.signed_in = true;
        }
    }

    /// A server's models arrived (or why not): move on to the picker, usable models only.
    pub(crate) fn models_listed(&mut self, result: Result<Vec<ModelInfo>, String>) {
        if let Step::SigningIn(wait) = &mut self.step {
            let models = match result {
                Ok(models) => models,
                Err(message) => {
                    wait.state = SetupState::Failed(message);
                    return;
                }
            };
            // OpenRouter says which models can call tools; only those can act.
            let usable: Vec<ModelInfo> = models.into_iter().filter(|m| m.supports_tools == Some(true)).collect();
            if usable.is_empty() {
                wait.state = SetupState::Failed("OpenRouter listed no model that can use tools.".into());
                return;
            }
            self.step = Step::Pick(ModelPicker::new(Source::OpenRouter, String::new(), usable));
            return;
        }
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
        self.step = Step::Pick(ModelPicker::new(Source::Local, url, usable));
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
            Step::Pick(picker) => {
                picker.filter.extend(text.chars().filter(|c| !c.is_control()));
                picker.selected = 0;
            }
            Step::Choose { .. } | Step::OpenRouterNotice | Step::SigningIn(_) => {}
        }
    }

    /// Handles one key press.
    pub(crate) fn key(&mut self, key: KeyEvent) -> Outcome {
        let checking = self.checking();
        // Waiting for the browser can always be cancelled.
        if key.code == KeyCode::Esc && (!checking || matches!(self.step, Step::SigningIn(_))) {
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
                KeyCode::Char('3') => self.step = Step::OpenRouterNotice,
                KeyCode::Enter => {
                    self.step = match *selected {
                        0 => Step::Key(KeyForm::new()),
                        1 => Step::LocalUrl(UrlForm { url: LM_STUDIO_URL.into(), state: SetupState::Editing }),
                        _ => Step::OpenRouterNotice,
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
            Step::Pick(picker) => match key.code {
                KeyCode::Up => {
                    picker.selected = picker.selected.saturating_sub(1);
                    picker.state = SetupState::Editing;
                }
                KeyCode::Down => {
                    picker.selected = (picker.selected + 1).min(picker.visible().len().saturating_sub(1));
                    picker.state = SetupState::Editing;
                }
                KeyCode::Char(c) if types_text(&key) => {
                    picker.filter.push(c);
                    picker.selected = 0;
                    picker.state = SetupState::Editing;
                }
                KeyCode::Backspace => {
                    picker.filter.pop();
                    picker.selected = 0;
                    picker.state = SetupState::Editing;
                }
                KeyCode::Enter => {
                    let Some(model) = picker.visible().get(picker.selected).map(|m| m.id.clone()) else {
                        picker.state = SetupState::Failed("No model matches what you typed.".into());
                        return Outcome::none();
                    };
                    picker.state = SetupState::Checking;
                    let effect = match picker.source {
                        Source::Local => Effect::SaveLocal { url: picker.url.clone(), model },
                        Source::OpenRouter => Effect::SaveOpenRouter { model },
                    };
                    return Outcome::effect(effect);
                }
                _ => {}
            },
            Step::OpenRouterNotice => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('y' | 'Y')) {
                    self.step = Step::SigningIn(SignInWait::new());
                    return Outcome::effect(Effect::StartSignIn);
                }
            }
            Step::SigningIn(wait) => {
                // A failed sign-in can be tried again.
                if key.code == KeyCode::Enter && matches!(wait.state, SetupState::Failed(_)) {
                    *wait = SignInWait::new();
                    return Outcome::effect(Effect::StartSignIn);
                }
            }
        }
        Outcome::none()
    }

    /// `esc`: one step back; from the first step, close (unless nothing is set up yet).
    fn back(&mut self) -> Outcome {
        self.step = match &self.step {
            Step::Choose { .. } => return Outcome { effects: Vec::new(), close: !self.first_run },
            Step::Key(_) => Step::Choose { selected: 0 },
            Step::LocalUrl(_) => Step::Choose { selected: 1 },
            Step::Pick(picker) if picker.source == Source::Local => {
                Step::LocalUrl(UrlForm { url: picker.url.clone(), state: SetupState::Editing })
            }
            // Leaving OpenRouter's sign-in or picker forgets the sign-in (and the key held for it).
            Step::Pick(_) | Step::SigningIn(_) => {
                self.step = Step::Choose { selected: 2 };
                return Outcome::effect(Effect::CancelSignIn);
            }
            Step::OpenRouterNotice => Step::Choose { selected: 2 },
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
        let Step::Pick(picker) = &setup.step else { panic!("the picker") };
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
        assert!(matches!(&setup.step, Step::Pick(p) if p.state == SetupState::Failed("`m` can't use tools.".into())));
        setup.key(key(KeyCode::Esc));
        assert!(matches!(&setup.step, Step::LocalUrl(f) if f.url == LM_STUDIO_URL));
    }

    fn openrouter_model(id: &str, tools: bool, free: bool) -> ModelInfo {
        ModelInfo { id: id.into(), supports_tools: Some(tools), free: Some(free) }
    }

    #[test]
    fn openrouter_asks_for_the_privacy_confirmation_before_signing_in() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('3')));
        assert!(matches!(setup.step, Step::OpenRouterNotice));
        assert!(setup.key(key(KeyCode::Char('x'))).effects.is_empty(), "only an explicit yes starts it");
        assert_eq!(setup.key(key(KeyCode::Char('y'))).effects, vec![Effect::StartSignIn]);
        assert!(matches!(&setup.step, Step::SigningIn(wait) if wait.state == SetupState::Checking));
    }

    #[test]
    fn waiting_for_the_browser_can_be_cancelled_and_a_failure_retried() {
        let mut setup = Setup::new(false);
        setup.key(key(KeyCode::Char('3')));
        setup.key(key(KeyCode::Enter));
        setup.sign_in_started("https://openrouter.ai/auth?x".into(), false);
        assert!(matches!(&setup.step, Step::SigningIn(w) if w.url.is_some() && !w.browser_opened));
        let outcome = setup.key(key(KeyCode::Esc));
        assert_eq!(outcome.effects, vec![Effect::CancelSignIn], "esc works while waiting");
        assert!(matches!(setup.step, Step::Choose { selected: 2 }));

        setup.key(key(KeyCode::Enter));
        setup.key(key(KeyCode::Char('y')));
        setup.failed("Sign-in timed out; start it again");
        assert_eq!(setup.key(key(KeyCode::Enter)).effects, vec![Effect::StartSignIn], "enter tries again");
    }

    #[test]
    fn the_openrouter_picker_offers_only_tool_models_and_filters_as_you_type() {
        let mut setup = Setup::new(true);
        setup.key(key(KeyCode::Char('3')));
        setup.key(key(KeyCode::Char('y')));
        setup.signed_in();
        setup.models_listed(Ok(vec![
            openrouter_model("anthropic/claude-sonnet-5.5", true, false),
            openrouter_model("vendor/chat-only", false, false),
            openrouter_model("qwen/qwen3-coder:free", true, true),
        ]));
        let Step::Pick(picker) = &setup.step else { panic!("the picker") };
        assert_eq!(picker.source, Source::OpenRouter);
        assert_eq!(picker.models.len(), 2, "models without tools are left out");
        for c in "qwen".chars() {
            setup.key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            setup.key(key(KeyCode::Enter)).effects,
            vec![Effect::SaveOpenRouter { model: "qwen/qwen3-coder:free".into() }]
        );
    }

    #[test]
    fn leaving_the_openrouter_picker_forgets_the_sign_in() {
        let mut setup = Setup::new(false);
        setup.key(key(KeyCode::Char('3')));
        setup.key(key(KeyCode::Char('y')));
        setup.models_listed(Ok(vec![openrouter_model("m", true, false)]));
        assert_eq!(setup.key(key(KeyCode::Esc)).effects, vec![Effect::CancelSignIn]);
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let mut setup = Setup::local(false, LM_STUDIO_URL);
        setup.key(key(KeyCode::Enter));
        setup.models_listed(Ok(vec![model("qwen3-8b", None)]));
        setup.key(key(KeyCode::Char('z')));
        assert!(setup.key(key(KeyCode::Enter)).effects.is_empty());
        assert_eq!(setup.state(), Some(&SetupState::Failed("No model matches what you typed.".into())));
    }
}
