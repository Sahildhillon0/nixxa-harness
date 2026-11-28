//! The composition root: picks the concrete modules and plugs them into the controller.
//!
//! The brain is Claude ([`AnthropicBrain`]) with keys in the OS keychain
//! ([`KeychainCredentials`]), a local or OpenRouter model, or the Stage 2 scripted brain with
//! `--fake`. The sandbox comes from the start-up choice ([`Workplace`]); the store is
//! [`SqliteStore`] (or [`MemoryStore`] with `--fake`, or when the database can't be opened);
//! knowledge is still the Stage 2 fake.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use nixxa_accounts::{EnvKey, KeychainConfig, KeychainCredentials};
use nixxa_brain::fake::ScriptedBrain;
use nixxa_brain::{AnthropicBrain, AnthropicConfig, ModelInfo, OpenAiCompatBrain, OpenAiCompatConfig};
use nixxa_contracts::{
    AccountInfo, AccountRef, Brain, BrainError, Credential, Credentials, CredentialsError, ProviderId, Secret,
    SessionId, Store, StoreError, WorkspaceInfo,
};
use nixxa_controller::scrub::Scrubber;
use nixxa_controller::{Controller, ControllerConfig};
use nixxa_db::SqliteStore;
use nixxa_db::memory::MemoryStore;
use nixxa_knowledge::fake::FakeKnowledge;

use crate::ports::{ChannelSink, TuiApprovals, UiLink};
use crate::settings::Settings;
use crate::workplace::Workplace;

/// The provider id of Claude accounts.
pub(crate) const ANTHROPIC: &str = "anthropic";
/// The account a key set up in the TUI is stored as.
pub(crate) const SETUP_LABEL: &str = "default";

/// Describes the workspace at `root` (display name = the directory name).
pub(crate) fn workspace(root: &Path) -> WorkspaceInfo {
    let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
    WorkspaceInfo { name, root: root.to_path_buf() }
}

/// The scripted Stage 2 brain (`--fake`) and its pretend account.
pub(crate) fn scripted() -> (Arc<dyn Brain>, AccountRef) {
    (Arc::new(ScriptedBrain), AccountRef::new("scripted", "default"))
}

/// The store sessions are kept in, and a note for the conversation when history is off.
///
/// * `--fake`: [`MemoryStore`], so the demo never writes the user's history.
/// * Otherwise [`SqliteStore`] at `path` (`<data dir>/nixxa/nixxa.db`). If there is no data
///   directory or the database can't be opened, [`MemoryStore`] with a note saying why and that
///   history is off. This is the one fallback Nixxa makes (D-058): losing history must not stop
///   the user from working, and the note keeps it visible.
pub(crate) async fn store(fake: bool, path: Option<PathBuf>) -> (Arc<dyn Store>, Option<String>) {
    if fake {
        return (Arc::new(MemoryStore::default()), None);
    }
    let Some(path) = path else {
        tracing::warn!("no data directory: history is off");
        let note = "History is off: no data directory was found, so this session won't be saved.";
        return (Arc::new(MemoryStore::default()), Some(note.into()));
    };
    match SqliteStore::open(&path).await {
        Ok(store) => {
            tracing::info!(path = %path.display(), "history database open");
            (Arc::new(store), None)
        }
        Err(err) => {
            tracing::warn!("the history database can't be opened: history is off");
            let note = format!("History is off: {err}. This session won't be saved.");
            (Arc::new(MemoryStore::default()), Some(note))
        }
    }
}

/// The newest session started in `root`, if any (`--continue`).
pub(crate) async fn latest_session(store: &dyn Store, root: &Path) -> Result<Option<SessionId>, StoreError> {
    let sessions = store.list_sessions().await?;
    // Every launch starts a session; one nothing was said in isn't worth continuing.
    Ok(sessions.into_iter().find(|s| s.workspace.root == root && s.first_message.is_some()).map(|s| s.id))
}

/// A controller for `brain` and `account` working in `workplace`, recording to `store`, with the
/// Stage 2 knowledge, reporting to the UI through `ui`.
pub(crate) fn controller(
    brain: Arc<dyn Brain>,
    account: AccountRef,
    workspace: WorkspaceInfo,
    workplace: &Workplace,
    store: Arc<dyn Store>,
    ui: &UiLink,
    scrubber: Arc<Scrubber>,
) -> Controller {
    Controller::new(
        brain,
        workplace.sandbox.clone(),
        store,
        Arc::new(FakeKnowledge),
        Arc::new(TuiApprovals::new(ui.tx.clone(), ui.auto.clone())),
        Arc::new(ChannelSink::new(ui.tx.clone(), ui.generation.clone())),
        {
            let mut config = ControllerConfig::new(workspace, account, workplace.instructions(), scrubber);
            config.policy = workplace.policy();
            config
        },
    )
}

/// The provider id of local-model accounts.
pub(crate) const LOCAL: &str = "local";
/// The provider id of OpenRouter accounts (signed in through the browser).
pub(crate) const OPENROUTER: &str = nixxa_accounts::methods::openrouter::PROVIDER;
/// Setting: the account sessions start with.
const ACTIVE_ACCOUNT: &str = "active_account";

/// Setting: the model of a local account.
fn model_setting(account: &AccountRef) -> String {
    format!("model.{account}")
}

/// Makes a local-model brain (the network in production, recorded responses in tests).
type LocalFactory =
    dyn Fn(Arc<dyn Credentials>, OpenAiCompatConfig) -> Result<OpenAiCompatBrain, BrainError> + Send + Sync;

/// The brains Nixxa can think with, and the keychain their accounts live in.
pub(crate) struct Models {
    pub(crate) anthropic: Arc<AnthropicBrain>,
    pub(crate) credentials: Arc<KeychainCredentials>,
    local: Arc<LocalFactory>,
}

impl Models {
    /// The real thing: the OS keychain and the network.
    pub(crate) fn new(settings: &Settings) -> Result<Self, String> {
        let env_keys = settings
            .env_key
            .iter()
            .map(|secret| EnvKey { provider: ProviderId::new(ANTHROPIC), secret: secret.clone() })
            .collect();
        let credentials = Arc::new(KeychainCredentials::new(KeychainConfig { env_keys }));
        let anthropic = AnthropicBrain::new(credentials.clone(), config(settings))
            .map_err(|err| format!("could not set up the Anthropic client: {err}"))?;
        Ok(Self { anthropic: Arc::new(anthropic), credentials, local: Arc::new(OpenAiCompatBrain::new) })
    }

    /// Why the keychain can't be used, if it can't.
    pub(crate) async fn storage_problem(&self) -> Option<String> {
        self.credentials.check_storage().await.err().map(|err| err.to_string())
    }

    /// The account to start a session with: the one last set up, else the Anthropic default
    /// (a stored key or `ANTHROPIC_API_KEY`), else the local default.
    pub(crate) async fn start_account(&self) -> Option<AccountRef> {
        let accounts = self.credentials.list(None).await.unwrap_or_default();
        let exists = |account: &AccountRef| accounts.iter().any(|a| &a.account == account);
        if let Ok(Some(active)) = self.credentials.setting(ACTIVE_ACCOUNT).await {
            if let Ok(account) = active.parse::<AccountRef>() {
                if exists(&account) {
                    return Some(account);
                }
            }
        }
        for provider in [ANTHROPIC, LOCAL, OPENROUTER] {
            if let Some(account) = accounts.iter().find(|a| a.account.provider.as_str() == provider && a.is_default) {
                return Some(account.account.clone());
            }
        }
        None
    }

    /// The brain for an account, from its provider.
    pub(crate) async fn brain_for(&self, account: &AccountRef) -> Result<Arc<dyn Brain>, String> {
        match account.provider.as_str() {
            ANTHROPIC => Ok(self.anthropic.clone()),
            LOCAL => {
                let model = self
                    .credentials
                    .setting(&model_setting(account))
                    .await
                    .map_err(|err| err.to_string())?
                    .ok_or_else(|| format!("no model is chosen for {account} yet"))?;
                let brain = (self.local)(self.credentials.clone(), OpenAiCompatConfig::new(LOCAL, model))
                    .map_err(|err| format!("could not set up the local model client: {err}"))?;
                Ok(Arc::new(brain))
            }
            OPENROUTER => {
                let model = self
                    .credentials
                    .setting(&model_setting(account))
                    .await
                    .map_err(|err| err.to_string())?
                    .ok_or_else(|| format!("no model is chosen for {account} yet"))?;
                let brain = (self.local)(self.credentials.clone(), OpenAiCompatConfig::openrouter(model))
                    .map_err(|err| format!("could not set up the OpenRouter client: {err}"))?;
                Ok(Arc::new(brain))
            }
            other => Err(format!("Nixxa can't use {other} accounts yet")),
        }
    }

    /// A brain for a signed-in key that isn't saved yet, through a throwaway in-memory account.
    async fn openrouter_probe(&self, key: &Secret, model: &str) -> Result<(OpenAiCompatBrain, AccountRef), String> {
        let account = AccountRef::new(OPENROUTER, SETUP_LABEL);
        let scratch = Arc::new(KeychainCredentials::in_memory(KeychainConfig::default()));
        let token = Credential::BearerToken { token: key.clone(), expires_at: None };
        scratch.add(account.clone(), token).await.map_err(|err| err.to_string())?;
        let brain = (self.local)(scratch, OpenAiCompatConfig::openrouter(model))
            .map_err(|err| format!("could not set up the OpenRouter client: {err}"))?;
        Ok((brain, account))
    }

    /// OpenRouter's models, for the picker (setup keeps the ones that can use tools).
    pub(crate) async fn list_openrouter_models(&self, key: &Secret) -> Result<Vec<ModelInfo>, String> {
        let (brain, account) = self.openrouter_probe(key, "").await?;
        brain.list_models(&account).await.map_err(|err| local_words(&err))
    }

    /// Checks the signed-in key and that OpenRouter offers `model`, then saves the key as
    /// `openrouter:default` with that model and makes it the account sessions start with.
    pub(crate) async fn save_openrouter(&self, key: Secret, model: &str) -> Result<AccountInfo, String> {
        let (brain, account) = self.openrouter_probe(&key, model).await?;
        if let Err(err) = brain.verify(&account).await {
            tracing::warn!("OpenRouter key or model check failed");
            return Err(local_words(&err));
        }
        let token = Credential::BearerToken { token: key, expires_at: None };
        let info = self.credentials.add(account.clone(), token).await.map_err(|err| match err {
            CredentialsError::Storage(message) => {
                format!("Signed in, but the OS keychain couldn't store the key ({message}).")
            }
            other => other.to_string(),
        })?;
        let saved = async {
            self.credentials.set_setting(&model_setting(&account), Some(model)).await?;
            self.credentials.set_setting(ACTIVE_ACCOUNT, Some(&account.to_string())).await
        };
        saved.await.map_err(|err| format!("Couldn't save the model choice: {err}"))?;
        tracing::info!(account = %account, "OpenRouter account saved");
        Ok(info)
    }

    /// A local account's server URL.
    pub(crate) async fn local_url(&self, account: &AccountRef) -> Option<String> {
        match self.credentials.get(account).await {
            Ok(Credential::Local { base_url }) => Some(base_url),
            _ => None,
        }
    }

    /// A brain for a server that isn't saved yet, through a throwaway in-memory account.
    async fn probe(&self, url: &str, model: &str) -> Result<(OpenAiCompatBrain, AccountRef), String> {
        let account = AccountRef::new(LOCAL, SETUP_LABEL);
        let scratch = Arc::new(KeychainCredentials::in_memory(KeychainConfig::default()));
        scratch
            .add(account.clone(), Credential::Local { base_url: url.to_string() })
            .await
            .map_err(|err| err.to_string())?;
        let brain = (self.local)(scratch, OpenAiCompatConfig::new(LOCAL, model))
            .map_err(|err| format!("could not set up the local model client: {err}"))?;
        Ok((brain, account))
    }

    /// The models a local server offers (setup's model picker).
    pub(crate) async fn list_local_models(&self, url: &str) -> Result<Vec<ModelInfo>, String> {
        let (brain, account) = self.probe(url, "").await?;
        brain.list_models(&account).await.map_err(|err| local_words(&err))
    }

    /// Checks the server offers `model`, then saves the server as `local:default` with that
    /// model and makes it the account sessions start with.
    pub(crate) async fn save_local(&self, url: &str, model: &str) -> Result<AccountInfo, String> {
        let (brain, account) = self.probe(url, model).await?;
        if let Err(err) = brain.verify(&account).await {
            tracing::warn!("local model check failed");
            return Err(local_words(&err));
        }
        let info = self
            .credentials
            .add(account.clone(), Credential::Local { base_url: url.to_string() })
            .await
            .map_err(|err| format!("Couldn't save the local server: {err}"))?;
        let saved = async {
            self.credentials.set_setting(&model_setting(&account), Some(model)).await?;
            self.credentials.set_setting(ACTIVE_ACCOUNT, Some(&account.to_string())).await
        };
        saved.await.map_err(|err| format!("Couldn't save the model choice: {err}"))?;
        tracing::info!(account = %account, "local model saved");
        Ok(info)
    }

    /// Checks a pasted key with Anthropic, then stores it in the keychain as
    /// `anthropic:default` and makes it the account sessions start with. Errors are in words for
    /// the setup screen and never contain the key.
    pub(crate) async fn save_key(&self, key: Secret) -> Result<AccountInfo, String> {
        if key.expose().is_empty() || key.expose().chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("That doesn't look like an API key: it is empty or contains spaces.".into());
        }
        if let Err(err) = self.anthropic.verify_key(&key).await {
            tracing::warn!(reason = kind(&err), "API key check failed");
            return Err(match err {
                BrainError::Auth(message) => format!("Anthropic rejected this key: {message}"),
                BrainError::Network(message) => format!("Couldn't reach Anthropic: {message}"),
                BrainError::RateLimited { .. } => "Anthropic is rate limiting; try again in a moment.".into(),
                other => format!("The key couldn't be checked: {other}"),
            });
        }
        tracing::info!("API key accepted by Anthropic");
        let account = AccountRef::new(ANTHROPIC, SETUP_LABEL);
        match self.credentials.add(account.clone(), Credential::ApiKey(key)).await {
            Ok(info) => {
                tracing::info!(account = %account, "API key saved to the OS keychain");
                if self.credentials.set_setting(ACTIVE_ACCOUNT, Some(&account.to_string())).await.is_err() {
                    tracing::warn!("could not record the active account");
                }
                Ok(info)
            }
            Err(err) => {
                tracing::warn!(account = %account, "API key could not be saved");
                Err(match err {
                    CredentialsError::Storage(message) => format!(
                        "The key works, but the OS keychain couldn't store it ({message}). \
                         You can set ANTHROPIC_API_KEY instead."
                    ),
                    other => other.to_string(),
                })
            }
        }
    }
}

/// A local server problem, in words for the setup screen.
fn local_words(err: &BrainError) -> String {
    match err {
        BrainError::Network(message) | BrainError::Provider(message) | BrainError::Auth(message) => {
            let mut message = message.clone();
            if let Some(first) = message.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            message
        }
        other => other.to_string(),
    }
}

fn kind(err: &BrainError) -> &'static str {
    match err {
        BrainError::Auth(_) => "rejected",
        BrainError::Network(_) => "network",
        BrainError::RateLimited { .. } => "rate limited",
        BrainError::ContextTooLarge | BrainError::Provider(_) | BrainError::Other(_) => "other",
    }
}

/// The brain's settings, from the user's.
fn config(settings: &Settings) -> AnthropicConfig {
    let mut config = AnthropicConfig::default();
    if let Some(model) = &settings.model {
        config.model = model.clone();
    }
    config.effort = settings.effort;
    config
}

#[cfg(test)]
pub(crate) mod tests {
    use crossterm::event::KeyCode;
    use nixxa_brain::Replay;
    use nixxa_controller::Command;
    use tokio::time::{Duration, timeout};

    use super::*;

    /// A scrubber with a fixed key, for a controller under test.
    fn test_scrubber() -> Arc<Scrubber> {
        Arc::new(Scrubber::with_key([3; 32]))
    }
    use crate::app::tests::key;
    use crate::app::{ActivityState, App, Effect, Speaker};
    use crate::place::Place;
    use crate::ports::{AutoApprove, UiEvent};
    use tokio::sync::mpsc;

    /// The sandbox workplace (the in-memory fake).
    fn sandboxed() -> Workplace {
        Workplace::fake()
    }

    #[test]
    fn workspace_is_named_after_its_directory() {
        let root = std::env::temp_dir().join("nixxa-demo");
        assert_eq!(workspace(&root).name, "nixxa-demo");
    }

    /// Claude and local models answering from recorded responses, with an in-memory keychain.
    pub(crate) fn replaying(replay: &Replay) -> Models {
        let credentials = Arc::new(KeychainCredentials::in_memory(KeychainConfig::default()));
        let anthropic = AnthropicBrain::replaying(credentials.clone(), AnthropicConfig::default(), replay);
        let local_replay = replay.clone();
        let local: Arc<LocalFactory> =
            Arc::new(move |credentials, config| Ok(OpenAiCompatBrain::replaying(credentials, config, &local_replay)));
        Models { anthropic: Arc::new(anthropic), credentials, local }
    }

    /// Sends `text` and drives one turn to its end, approving every prompt with `y`. Returns the
    /// number of prompts.
    async fn drive(
        app: &mut App,
        ui: &mut mpsc::UnboundedReceiver<UiEvent>,
        commands: &mpsc::Sender<Command>,
        text: &str,
    ) -> usize {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        for effect in app.handle_key(key(KeyCode::Enter)) {
            let Effect::Send(command) = effect else { panic!("unexpected {effect:?}") };
            commands.send(command).await.unwrap();
        }
        let mut prompts = 0;
        loop {
            let event = timeout(Duration::from_secs(5), ui.recv()).await.expect("no stall").expect("open");
            match event {
                UiEvent::Controller(event) => {
                    let done = matches!(event, nixxa_contracts::Event::TurnDone { .. });
                    app.apply(event);
                    if done {
                        return prompts;
                    }
                }
                UiEvent::AutoApproved(id) => app.auto_approved(&id),
                UiEvent::Approval(prompt) => {
                    app.ask(prompt);
                    prompts += 1;
                    app.handle_key(key(KeyCode::Char('y')));
                }
                UiEvent::AccountSaved(_)
                | UiEvent::PolicyRead(_)
                | UiEvent::Protected { .. }
                | UiEvent::NetworkSet(_)
                | UiEvent::Sandbox(_)
                | UiEvent::ModelsListed(_)
                | UiEvent::SignInStarted(_)
                | UiEvent::SignedIn(_)
                | UiEvent::SessionsListed(_)
                | UiEvent::UndoDiff { .. }
                | UiEvent::SessionsFound { .. } => panic!("no setup or session list in this test"),
            }
        }
    }

    fn ui_kind(activity: &crate::app::Activity) -> &'static str {
        match &activity.request.action {
            nixxa_contracts::Action::Sandbox { .. } => "exec",
            nixxa_contracts::Action::Knowledge { .. } => "notes",
        }
    }

    /// Stage 2 exit criterion, with `--fake`: the fake agent queries fake knowledge, proposes a
    /// sandbox action, it is approved and its observation is shown.
    #[tokio::test]
    async fn stage_two_end_to_end() {
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (brain, account) = scripted();
        let mut controller = controller(
            brain,
            account,
            workspace(&std::env::temp_dir().join("nixxa-e2e")),
            &sandboxed(),
            Arc::new(MemoryStore::default()),
            &UiLink::new(ui_tx, AutoApprove::default()),
            test_scrubber(),
        );
        controller.start_session().await.expect("session starts");
        let task = tokio::spawn(async move { controller.run(cmd_rx).await });

        let mut app = App::new(false);
        let prompts = drive(&mut app, &mut ui_rx, &cmd_tx, "hello").await;

        assert_eq!(prompts, 2, "one approval for the knowledge query, one for the sandbox action");
        let summary: Vec<_> = app.activity.iter().map(|a| (a.state, ui_kind(a))).collect();
        assert_eq!(summary, vec![(ActivityState::Done, "notes"), (ActivityState::Done, "exec")]);
        let said: Vec<_> = app.conversation.iter().map(|e| (e.speaker, e.text.as_str())).collect();
        assert_eq!(
            said,
            vec![
                (Speaker::User, "hello"),
                (Speaker::Agent, "I looked into that."),
                (Speaker::Agent, "The command finished."),
            ]
        );
        cmd_tx.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
    }

    /// The scripted brain, remembering every context it is given.
    #[derive(Default)]
    struct Recording {
        contexts: std::sync::Mutex<Vec<nixxa_contracts::Context>>,
    }

    impl Recording {
        fn seen(&self) -> Vec<nixxa_contracts::Context> {
            self.contexts.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Brain for Recording {
        fn describe(&self) -> nixxa_contracts::BrainInfo {
            ScriptedBrain.describe()
        }

        async fn think(&self, context: nixxa_contracts::Context) -> Result<nixxa_contracts::BrainStream, BrainError> {
            self.contexts.lock().unwrap().push(context.clone());
            ScriptedBrain.think(context).await
        }
    }

    /// A folder for one test's database and project, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nixxa-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("project")).unwrap();
            Self(dir)
        }

        fn db(&self) -> PathBuf {
            self.0.join("data").join("nixxa").join("nixxa.db")
        }

        fn workspace(&self) -> WorkspaceInfo {
            workspace(&self.0.join("project"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Reads the planted `.env`, prints it with `cat`, then is done; keeps every request.
    #[derive(Default)]
    struct PlantedReader {
        step: std::sync::Mutex<usize>,
        contexts: std::sync::Mutex<Vec<nixxa_contracts::Context>>,
    }

    #[async_trait::async_trait]
    impl Brain for PlantedReader {
        fn describe(&self) -> nixxa_contracts::BrainInfo {
            ScriptedBrain.describe()
        }

        async fn think(&self, context: nixxa_contracts::Context) -> Result<nixxa_contracts::BrainStream, BrainError> {
            use nixxa_contracts::{Action, ActionId, ActionRequest, BrainEvent, Intent, SandboxAction};
            self.contexts.lock().unwrap().push(context);
            let mut step = self.step.lock().unwrap();
            *step += 1;
            let act = |action| Intent::Act {
                request: ActionRequest {
                    id: ActionId::new(format!("a{step}")),
                    action: Action::Sandbox { action },
                    reason: None,
                },
            };
            let intent = match *step {
                1 => act(SandboxAction::ReadFile { path: ".env".into() }),
                2 => act(SandboxAction::Exec {
                    command: "cat .env".into(),
                    cwd: None,
                    timeout_ms: None,
                    background: false,
                }),
                _ => Intent::Done { summary: "read it".into() },
            };
            Ok(Box::pin(futures::stream::iter([Ok(BrainEvent::Intent { intent })])))
        }
    }

    /// Stage 7 exit criterion (#3): a fake key planted in a file never reaches the provider (a
    /// recording brain gets every request) nor the history database, and the user is warned.
    #[tokio::test(flavor = "multi_thread")]
    async fn stage_seven_a_planted_key_never_reaches_the_provider_or_the_database() {
        let scratch = Scratch::new("e2e7-secret");
        let (id, secret) = ("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        let planted = format!("AWS_ACCESS_KEY_ID={id}\nAWS_SECRET_ACCESS_KEY={secret}\n");
        std::fs::write(scratch.0.join("project").join(".env"), planted).unwrap();
        let (store, history_off) = store(false, Some(scratch.db())).await;
        assert!(history_off.is_none(), "{history_off:?}");
        let workspace = scratch.workspace();
        let place = Workplace::new(crate::place::Place::Here, &workspace).unwrap();
        let brain = Arc::new(PlantedReader::default());
        let (ui_tx, mut ui) = mpsc::unbounded_channel();
        let link = UiLink::new(ui_tx, AutoApprove::default());
        let scrubber = test_scrubber();
        let (_, account) = scripted();
        let mut controller =
            controller(brain.clone(), account, workspace, &place, store.clone(), &link, scrubber.clone());
        controller.start_session().await.unwrap();
        let (commands, rx) = mpsc::channel(8);
        let task = tokio::spawn(async move { controller.run(rx).await });
        let mut app = App::new(false);
        app.scrubber = Some(scrubber);
        drive(&mut app, &mut ui, &commands, &format!("deploy with {id}")).await;
        commands.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
        drop(store);

        let leaks = |text: &str| text.contains("IOSFODNN7") || text.contains("K7MDENG");
        let requests = brain.contexts.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert!(!leaks(&serde_json::to_string(request).unwrap()), "the provider got the key: {request:#?}");
        }
        let last = serde_json::to_string(requests.last().unwrap()).unwrap();
        assert!(last.contains("‹secret:aws-secret-key#") && last.contains("‹secret:aws-access-key#"), "{last}");
        // Nor the database, in any of its files.
        for entry in std::fs::read_dir(scratch.db().parent().unwrap()).unwrap().flatten() {
            // SQLite removes `-wal` and `-shm` when its last connection closes, which may be now.
            let Ok(bytes) = std::fs::read(entry.path()) else { continue };
            assert!(!leaks(&String::from_utf8_lossy(&bytes)), "{} holds the key", entry.path().display());
        }
        // And the user was told what was replaced, and where from.
        let warned: Vec<&str> =
            app.conversation.iter().map(|e| e.text.as_str()).filter(|t| t.starts_with("Redacted")).collect();
        for from in ["in your message", "in `.env`", "in the output of command a2"] {
            assert!(warned.iter().any(|w| w.contains(from)), "no warning {from:?}: {warned:#?}");
        }
        assert!(app.happenings.iter().any(|(_, h)| matches!(h, crate::app::Happening::Redacted { .. })));
    }

    /// Proposes one queued action per step (`None`: done), and keeps every request.
    #[derive(Default)]
    struct Prober {
        steps: std::sync::Mutex<std::collections::VecDeque<Option<nixxa_contracts::SandboxAction>>>,
        contexts: std::sync::Mutex<Vec<nixxa_contracts::Context>>,
        n: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Brain for Prober {
        fn describe(&self) -> nixxa_contracts::BrainInfo {
            ScriptedBrain.describe()
        }

        async fn think(&self, context: nixxa_contracts::Context) -> Result<nixxa_contracts::BrainStream, BrainError> {
            use nixxa_contracts::{Action, ActionId, ActionRequest, BrainEvent, Intent};
            self.contexts.lock().unwrap().push(context);
            let n = self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let intent = match self.steps.lock().unwrap().pop_front().flatten() {
                Some(action) => Intent::Act {
                    request: ActionRequest {
                        id: ActionId::new(format!("p{n}")),
                        action: Action::Sandbox { action },
                        reason: None,
                    },
                },
                None => Intent::Done { summary: "done".into() },
            };
            Ok(Box::pin(futures::stream::iter([Ok(BrainEvent::Intent { intent })])))
        }
    }

    /// Stage 7 exit criterion (#5): with `protect = ["migrations/**"]` in AGENTS.md, a patch, an
    /// `rm -rf` and an `mv` all fail to change `migrations/`, in the real sandbox, and the rule
    /// is shown; the prose reaches the agent; the agent editing AGENTS.md changes nothing until
    /// the user accepts.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn stage_seven_protected_migrations_survive_a_patch_rm_and_mv() {
        use nixxa_contracts::{Edit, SandboxAction};
        let exe = std::env::current_exe().unwrap();
        let daemon = exe.parent().and_then(Path::parent).map(|dir| dir.join("nixxa-sandboxd"));
        let bwrap =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|d| d.join("bwrap").is_file());
        let Some(daemon) = daemon.filter(|d| d.is_file() && bwrap) else {
            eprintln!(
                "SKIPPED: the Stage 7 policy exit test needs bwrap and a built nixxa-sandboxd (`cargo test --workspace`)"
            );
            return;
        };
        let scratch = Scratch::new("e2e7-policy");
        let root = scratch.0.join("project");
        let agents = "# Rules\n\nNever edit migrations by hand; add a new one with `make migration`.\n\n```nixxa-policy\nprotect = [\"migrations/**\"]\n```\n";
        std::fs::write(root.join("AGENTS.md"), agents).unwrap();
        std::fs::create_dir_all(root.join("migrations")).unwrap();
        let sql = "create table users (id int);\n";
        std::fs::write(root.join("migrations/001_init.sql"), sql).unwrap();
        let workspace = scratch.workspace();
        let place =
            crate::workplace::sandboxed_with(Place::SandboxHere, &workspace, &daemon, &scratch.0.join("scratch"), None)
                .expect("the sandbox");
        let (sources, protection) = place.apply_policy(&workspace, None).await;
        let protection = protection.expect("rules").expect("kept");
        assert_eq!(protection.how, nixxa_contracts::ProtectionKind::ReadOnlyMounts);
        assert_eq!(protection.kept, ["migrations"]);
        let patch = SandboxAction::Patch {
            path: "migrations/001_init.sql".into(),
            edits: vec![Edit { search: "int".into(), replace: "bigint".into() }],
            expected_hash: "0".repeat(64),
        };
        let exec = |command: &str| SandboxAction::Exec {
            command: command.into(),
            cwd: None,
            timeout_ms: None,
            background: false,
        };
        let new_agents = agents.replace("protect = [\"migrations/**\"]", "protect = []");
        let brain = Arc::new(Prober::default());
        brain.steps.lock().unwrap().extend([
            Some(patch.clone()),
            Some(exec("rm -rf migrations")),
            Some(exec("mv migrations x")),
            None,
            Some(SandboxAction::WriteFile { path: "AGENTS.md".into(), content: new_agents }),
            None,
            Some(patch),
            None,
        ]);
        let (ui_tx, mut ui) = mpsc::unbounded_channel();
        let link = UiLink::new(ui_tx, AutoApprove::default());
        let (store, _) = store(true, None).await;
        let (_, account) = scripted();
        let mut controller =
            controller(brain.clone(), account, workspace.clone(), &place, store, &link, test_scrubber());
        controller.start_session().await.unwrap();
        let (commands, rx) = mpsc::channel(8);
        let task = tokio::spawn(async move { controller.run(rx).await });
        let mut app = App::new(false);
        app.scrubber = Some(test_scrubber());
        app.policy_in_force(sources, Some(Ok(protection)));
        let unchanged = || {
            assert_eq!(std::fs::read_to_string(root.join("migrations/001_init.sql")).unwrap(), sql);
            assert!(!root.join("x").exists(), "migrations/ was moved");
        };

        // The patch is refused at the gate, unasked; rm and mv are asked (Ask mode), run, and fail.
        let prompts = drive(&mut app, &mut ui, &commands, "change the users table").await;
        assert_eq!(prompts, 2, "only the two commands were put to the user");
        unchanged();
        let first = serde_json::to_string(&brain.contexts.lock().unwrap()[0]).unwrap();
        assert!(first.contains("Never edit migrations by hand"), "the prose reaches the agent: {first}");
        assert!(first.contains("not the user's instructions"), "{first}");
        let told = serde_json::to_string(brain.contexts.lock().unwrap().last().unwrap()).unwrap();
        assert!(
            told.contains("refused by the project policy") && told.contains("`migrations/**` (AGENTS.md:6)"),
            "{told}"
        );
        let shown = format!("{:?}", app.activity);
        assert!(shown.contains("`migrations/**` (AGENTS.md:6)"), "the user sees the rule: {shown}");

        // The agent loosens AGENTS.md: the change waits for the user.
        drive(&mut app, &mut ui, &commands, "update the rules").await;
        app.policy_read(crate::policy::Sources::read(Some(&root), None));
        assert!(app.policy.pending.is_some(), "the change is offered");
        assert!(app.conversation.iter().any(|e| e.text.contains("A policy file changed")));
        assert_eq!(place.policy().protect.len(), 1, "not in force until accepted");
        drive(&mut app, &mut ui, &commands, "try again").await;
        unchanged();
        let told = serde_json::to_string(brain.contexts.lock().unwrap().last().unwrap()).unwrap();
        assert!(told.contains("refused by the project policy"), "still refused: {told}");
        commands.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
    }

    /// One run of Nixxa against `store`: a controller (new or resumed) and a fresh screen.
    struct Run {
        app: App,
        ui: mpsc::UnboundedReceiver<UiEvent>,
        commands: mpsc::Sender<Command>,
        task: tokio::task::JoinHandle<()>,
        brain: Arc<Recording>,
    }

    impl Run {
        async fn start(store: Arc<dyn Store>, workspace: WorkspaceInfo, resume: Option<&SessionId>) -> Self {
            let (ui_tx, ui) = mpsc::unbounded_channel();
            let brain = Arc::new(Recording::default());
            let link = UiLink::new(ui_tx, AutoApprove::default());
            let (_, account) = scripted();
            let mut controller =
                controller(brain.clone(), account, workspace, &sandboxed(), store, &link, test_scrubber());
            match resume {
                Some(id) => controller.resume_session(id).await.expect("resumes"),
                None => controller.start_session().await.expect("starts"),
            };
            let (commands, rx) = mpsc::channel(8);
            let task = tokio::spawn(async move { controller.run(rx).await });
            Self { app: App::new(false), ui, commands, task, brain }
        }

        /// Applies events until `stop` matches one (approval prompts are queued, not answered).
        async fn until(&mut self, stop: impl Fn(&UiEvent) -> bool) {
            loop {
                let event = timeout(Duration::from_secs(5), self.ui.recv()).await.expect("no stall").expect("open");
                let done = stop(&event);
                match event {
                    UiEvent::Controller(event) => self.app.apply(event),
                    UiEvent::Approval(prompt) => self.app.ask(prompt),
                    _ => {}
                }
                if done {
                    return;
                }
            }
        }

        /// Presses a key and does what the TUI loop does with its effects.
        async fn press(&mut self, key: crossterm::event::KeyEvent) {
            for effect in self.app.handle_key(key) {
                match effect {
                    Effect::Send(command) => self.commands.send(command).await.unwrap(),
                    Effect::Quit => {
                        self.commands.send(Command::Shutdown).await.unwrap();
                        timeout(Duration::from_secs(5), &mut self.task).await.expect("ends in time").unwrap();
                        // What the session said while it wound down (the screen is gone in the
                        // real app, but it is all in the database).
                        while let Ok(UiEvent::Controller(event)) = self.ui.try_recv() {
                            self.app.apply(event);
                        }
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        }

        fn said(&self) -> Vec<(Speaker, String)> {
            self.app.conversation.iter().map(|e| (e.speaker, e.text.clone())).collect()
        }

        fn activity(&self) -> Vec<(ActivityState, &'static str)> {
            self.app.activity.iter().map(|a| (a.state, ui_kind(a))).collect()
        }
    }

    fn ctrl_c() -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(KeyCode::Char('c'), crossterm::event::KeyModifiers::CONTROL)
    }

    fn is_resumed_notice(event: &UiEvent) -> bool {
        matches!(event, UiEvent::Controller(nixxa_contracts::Event::Message { message, .. })
            if message.role == nixxa_contracts::Role::System
                && matches!(&message.content, nixxa_contracts::Content::Text { text } if text.contains("resumed")))
    }

    /// Stage 5 exit criterion: quit mid-session (ctrl-c twice during a turn), relaunch on the
    /// same database file, resume, and everything is back: the conversation, the activity pane
    /// and the brain's history. New messages go to the same session.
    #[tokio::test(flavor = "multi_thread")]
    async fn stage_five_quit_mid_turn_relaunch_and_resume_where_you_left_off() {
        let scratch = Scratch::new("e2e5");
        let (store_one, history_off) = store(false, Some(scratch.db())).await;
        assert!(history_off.is_none(), "{history_off:?}");
        let mut first = Run::start(store_one.clone(), scratch.workspace(), None).await;
        let commands = first.commands.clone();
        drive(&mut first.app, &mut first.ui, &commands, "hello").await;
        for c in "list it again".chars() {
            first.press(key(KeyCode::Char(c))).await;
        }
        first.press(key(KeyCode::Enter)).await;
        first.until(|e| matches!(e, UiEvent::Approval(_))).await;
        first.press(ctrl_c()).await; // interrupts the turn
        first.press(ctrl_c()).await; // and quits
        let before = (first.said(), first.activity());
        drop(first);
        drop(store_one);

        // Relaunch: a new store on the same file.
        let (store_two, _) = store(false, Some(scratch.db())).await;
        let id = latest_session(&*store_two, &scratch.workspace().root).await.unwrap().expect("a session to continue");
        let mut second = Run::start(store_two.clone(), scratch.workspace(), Some(&id)).await;
        second.until(is_resumed_notice).await;

        let mut said = second.said();
        let notice = said.pop().expect("the resumed notice");
        assert_eq!(notice.0, Speaker::System);
        assert!(notice.1.contains("workspace may have changed"), "{notice:?}");
        assert_eq!(said, before.0, "the conversation is back as it was");
        assert_eq!(
            said.iter().map(|(who, text)| (*who, text.as_str())).collect::<Vec<_>>(),
            vec![
                (Speaker::User, "hello"),
                (Speaker::Agent, "I looked into that."),
                (Speaker::Agent, "The command finished."),
                (Speaker::User, "list it again"),
                (Speaker::System, "interrupted by the user"),
            ]
        );
        assert_eq!(second.activity(), before.1, "the activity pane is back as it was");
        assert_eq!(
            second.activity(),
            vec![(ActivityState::Done, "notes"), (ActivityState::Done, "exec"), (ActivityState::Denied, "notes")]
        );
        assert!(!second.app.busy, "no turn runs on resume");

        // The next message continues the same session, with the whole history behind it.
        let commands = second.commands.clone();
        let prompts = drive(&mut second.app, &mut second.ui, &commands, "and now?").await;
        assert_eq!(prompts, 2);
        let history = &second.brain.seen()[0].messages;
        let texts: Vec<_> = history
            .iter()
            .filter_map(|m| match &m.content {
                nixxa_contracts::Content::Text { text } => Some((m.role, text.as_str())),
                _ => None,
            })
            .collect();
        use nixxa_contracts::Role;
        assert_eq!(
            texts,
            vec![
                (Role::User, "hello"),
                (Role::Agent, "I looked into that."),
                (Role::Agent, "The command finished."),
                (Role::User, "list it again"),
                (Role::System, "interrupted by the user"),
                (Role::System, notice.1.as_str()),
                (Role::User, "and now?"),
            ],
            "the brain gets the history back"
        );
        assert!(
            !history.iter().any(|m| matches!(m.content, nixxa_contracts::Content::Summary { .. })),
            "never a Done summary (D-029)"
        );
        let sessions = store_two.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1, "no new session");
        assert_eq!(sessions[0].first_message.as_deref(), Some("hello"));
        let stored = store_two.load_session(&id).await.unwrap();
        let appended = stored.messages.iter().any(|m| {
            m.role == Role::User && matches!(&m.content, nixxa_contracts::Content::Text { text } if text == "and now?")
        });
        assert!(appended, "appended to the same session");
        assert_eq!(stored.actions.len(), 5, "three before the quit, two after");
        second.commands.send(Command::Shutdown).await.unwrap();
        second.task.await.unwrap();
    }

    /// Nixxa crashed while an approval was open: the action has no result in the file. Resuming
    /// closes it, the activity pane shows it failed, and the brain gets a result for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_crash_mid_approval_is_closed_on_resume() {
        let scratch = Scratch::new("e2e5-crash");
        let (store_one, _) = store(false, Some(scratch.db())).await;
        let mut first = Run::start(store_one.clone(), scratch.workspace(), None).await;
        for c in "hello".chars() {
            first.press(key(KeyCode::Char(c))).await;
        }
        first.press(key(KeyCode::Enter)).await;
        first.until(|e| matches!(e, UiEvent::Approval(_))).await;
        first.task.abort(); // the process died
        let _ = (&mut first.task).await;
        drop(first);
        drop(store_one);

        let (store_two, _) = store(false, Some(scratch.db())).await;
        let id = latest_session(&*store_two, &scratch.workspace().root).await.unwrap().unwrap();
        let mut second = Run::start(store_two, scratch.workspace(), Some(&id)).await;
        second.until(is_resumed_notice).await;
        assert_eq!(second.activity(), vec![(ActivityState::Failed, "notes")]);
        let failed = second.app.activity[0].observation.clone().unwrap();
        assert!(
            matches!(&failed.outcome, nixxa_contracts::Outcome::Failed { message } if message.contains("Nixxa quit")),
            "{failed:?}"
        );
        let commands = second.commands.clone();
        drive(&mut second.app, &mut second.ui, &commands, "again").await;
        let history = &second.brain.seen()[0].messages;
        let proposal =
            history.iter().position(|m| matches!(m.content, nixxa_contracts::Content::Action { .. })).unwrap();
        assert!(
            matches!(&history[proposal + 1].content, nixxa_contracts::Content::Observation { observation } if observation.outcome == failed.outcome),
            "the dangling call is answered right after it"
        );
        second.commands.send(Command::Shutdown).await.unwrap();
        second.task.await.unwrap();
    }

    /// A database that can't be opened turns history off, says why, and Nixxa carries on.
    /// `--continue` skips sessions nothing was said in: launching and quitting at once must not
    /// hide the last real conversation.
    #[tokio::test]
    async fn continue_skips_sessions_nothing_was_said_in() {
        use nixxa_contracts::{Message, NewSession, Role, Timestamp};

        let store = MemoryStore::default();
        let here = workspace(&std::env::temp_dir().join("nixxa-continue-here"));
        let new = |at| NewSession { workspace: here.clone(), account: scripted().1, started_at: Timestamp(at) };
        let real = store.create_session(new(10)).await.unwrap();
        store.append_message(&real.id, Message::text(Role::User, "fix the build", Timestamp(11))).await.unwrap();
        let empty = store.create_session(new(20)).await.unwrap();
        assert_eq!(latest_session(&store, &here.root).await.unwrap(), Some(real.id));
        assert_ne!(Some(empty.id), latest_session(&store, &here.root).await.unwrap());
        let elsewhere = std::env::temp_dir().join("nixxa-continue-elsewhere");
        assert_eq!(latest_session(&store, &elsewhere).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_unopenable_database_turns_history_off_visibly() {
        let scratch = Scratch::new("e2e5-corrupt");
        std::fs::create_dir_all(scratch.db().parent().unwrap()).unwrap();
        std::fs::write(scratch.db(), vec![7u8; 4096]).unwrap();
        let (store, note) = store(false, Some(scratch.db())).await;
        let note = note.expect("a note");
        assert!(note.starts_with("History is off:") && note.contains("nixxa.db"), "{note}");
        assert!(
            store
                .create_session(nixxa_contracts::NewSession {
                    workspace: scratch.workspace(),
                    account: scripted().1,
                    started_at: nixxa_contracts::Timestamp(0),
                })
                .await
                .is_ok(),
            "the in-memory store works"
        );
        let (_, none) = super::store(false, None).await;
        assert!(none.unwrap().contains("no data directory"));
        let (fake, quiet) = super::store(true, Some(scratch.db())).await;
        assert!(quiet.is_none());
        assert!(fake.list_sessions().await.unwrap().is_empty(), "--fake never reads the history");
        assert_eq!(std::fs::read(scratch.db()).unwrap(), vec![7u8; 4096], "the file is left alone");
    }

    /// Working in this directory: the same scripted turn runs its command in a real shell in the
    /// workspace, and the status says it isn't isolated.
    #[tokio::test]
    async fn working_here_runs_commands_for_real() {
        let root = std::env::temp_dir().join(format!("nixxa-e2e-here-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = workspace(&root);
        let here = Workplace::new(Place::Here, &workspace).expect("an existing directory");
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (brain, account) = scripted();
        let mut controller = controller(
            brain,
            account,
            workspace,
            &here,
            Arc::new(MemoryStore::default()),
            &UiLink::new(ui_tx, AutoApprove::default()),
            test_scrubber(),
        );
        controller.start_session().await.expect("session starts");
        let task = tokio::spawn(async move { controller.run(cmd_rx).await });

        let mut app = App::new(false);
        drive(&mut app, &mut ui_rx, &cmd_tx, "hello").await;
        let sandbox = app.sandbox.as_ref().expect("status");
        assert_eq!((sandbox.backend.as_str(), sandbox.isolated), ("host", false));
        let exec = app.activity.iter().find(|a| ui_kind(a) == "exec").expect("the command ran");
        let Some(nixxa_contracts::Outcome::Exec { stdout, .. }) = exec.observation.as_ref().map(|o| &o.outcome) else {
            panic!("expected exec output, got {:?}", exec.observation)
        };
        // The fake echoes the command line back; a real shell runs `echo`.
        assert_eq!(stdout.trim(), "hello from the sandbox");
        cmd_tx.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Stage 3, offline: Claude (from recorded responses) through the real wiring proposes a
    /// sandbox action, it is approved, and Claude's answer streams in.
    #[tokio::test]
    async fn stage_three_end_to_end_with_recorded_claude() {
        let replay = Replay::new();
        replay.push_fixture("anthropic/fixtures/tool_use.sse").push_fixture("anthropic/fixtures/text.sse");
        let anthropic = replaying(&replay);
        let account = AccountRef::new(ANTHROPIC, SETUP_LABEL);
        anthropic.credentials.add(account.clone(), Credential::ApiKey(Secret::new("sk-ant-test-0000"))).await.unwrap();

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let default = anthropic.start_account().await.expect("the stored account is the default");
        let brain = anthropic.brain_for(&default).await.unwrap();
        let mut controller = controller(
            brain,
            default,
            workspace(&std::env::temp_dir().join("nixxa-e2e3")),
            &sandboxed(),
            Arc::new(MemoryStore::default()),
            &UiLink::new(ui_tx, AutoApprove::default()),
            test_scrubber(),
        );
        controller.start_session().await.expect("session starts");
        let task = tokio::spawn(async move { controller.run(cmd_rx).await });

        let mut app = App::new(false);
        while app.brain.is_none() {
            let Some(UiEvent::Controller(event)) = ui_rx.recv().await else { panic!("session events") };
            app.apply(event);
        }
        assert_eq!(app.account, Some(account));
        assert_eq!(app.brain.as_ref().unwrap().model, "claude-sonnet-5-5");

        let prompts = drive(&mut app, &mut ui_rx, &cmd_tx, "list the project").await;
        assert_eq!(prompts, 1);
        assert_eq!(app.activity.len(), 1);
        assert_eq!(app.activity[0].state, ActivityState::Done);
        let said: Vec<_> = app.conversation.iter().map(|e| (e.speaker, e.text.as_str())).collect();
        assert_eq!(
            said,
            vec![
                (Speaker::User, "list the project"),
                (Speaker::Agent, "Let me look."),
                (Speaker::Agent, "Hello there.")
            ]
        );
        assert!(app.cost_micro_usd > 0, "real cost from the price table");
        assert_eq!(replay.request_bodies().len(), 2);
        cmd_tx.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_verified_key_is_stored_as_the_default_account() {
        let replay = Replay::new();
        replay.push(200, r#"{"id":"claude-sonnet-5-5"}"#);
        let anthropic = replaying(&replay);
        assert_eq!(anthropic.start_account().await, None);
        let info = anthropic.save_key(Secret::new("sk-ant-test-key-abcd")).await.expect("saved");
        assert_eq!(info.account, AccountRef::new(ANTHROPIC, SETUP_LABEL));
        assert_eq!(info.hint.as_deref(), Some("abcd"));
        assert_eq!(anthropic.start_account().await, Some(AccountRef::new(ANTHROPIC, SETUP_LABEL)));
    }

    #[tokio::test]
    async fn a_rejected_key_is_not_stored_and_the_error_is_clear() {
        let replay = Replay::new();
        replay.push(401, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#);
        let anthropic = replaying(&replay);
        let err = anthropic.save_key(Secret::new("sk-ant-wrong-key-0000")).await.unwrap_err();
        assert_eq!(err, "Anthropic rejected this key: invalid x-api-key");
        assert!(!err.contains("sk-ant-wrong"));
        assert_eq!(anthropic.start_account().await, None);
    }

    #[tokio::test]
    async fn a_key_with_spaces_is_refused_before_any_request() {
        let replay = Replay::new();
        let anthropic = replaying(&replay);
        let err = anthropic.save_key(Secret::new("sk-ant two words")).await.unwrap_err();
        assert!(err.contains("doesn't look like an API key"), "{err}");
        assert!(replay.request_bodies().is_empty());
    }

    const LOCAL_URL: &str = "http://localhost:1234/v1";

    fn local_account() -> AccountRef {
        AccountRef::new(LOCAL, SETUP_LABEL)
    }

    #[tokio::test]
    async fn a_local_server_and_model_are_saved_and_become_the_start_account() {
        let replay = Replay::new();
        replay.push_fixture("openai_compat/fixtures/models_local.json");
        let models = replaying(&replay);
        let info = models.save_local(LOCAL_URL, "qwen3-8b").await.expect("saved");
        assert_eq!(info.account, local_account());
        assert_eq!(info.method, nixxa_contracts::AuthMethod::Local);
        assert_eq!(models.start_account().await, Some(local_account()));
        assert_eq!(models.credentials.setting("model.local:default").await.unwrap().as_deref(), Some("qwen3-8b"));
        assert_eq!(models.local_url(&local_account()).await.as_deref(), Some(LOCAL_URL));
        let brain = models.brain_for(&local_account()).await.unwrap();
        assert_eq!(
            brain.describe(),
            nixxa_contracts::BrainInfo { provider: ProviderId::new(LOCAL), model: "qwen3-8b".into() }
        );
    }

    #[tokio::test]
    async fn a_model_the_server_does_not_offer_is_not_saved() {
        let replay = Replay::new();
        replay.push_fixture("openai_compat/fixtures/models_local.json");
        let models = replaying(&replay);
        let err = models.save_local(LOCAL_URL, "gpt-oss-120b").await.unwrap_err();
        assert!(err.starts_with("The server doesn't offer the model `gpt-oss-120b`"), "{err}");
        assert_eq!(models.start_account().await, None);
    }

    #[tokio::test]
    async fn an_unreachable_server_and_a_bad_url_are_explained() {
        let models = replaying(&Replay::new());
        let err = models.list_local_models(LOCAL_URL).await.unwrap_err();
        assert!(err.starts_with("Couldn't reach http://localhost:1234/v1") && err.contains("running"), "{err}");
        let err = models.list_local_models("localhost:1234").await.unwrap_err();
        assert!(err.contains("must start with http:// or https://"), "{err}");
    }

    #[tokio::test]
    async fn the_start_account_is_the_one_last_set_up() {
        let replay = Replay::new();
        replay.push_fixture("openai_compat/fixtures/models_local.json").push(200, r#"{"id":"claude-sonnet-5-5"}"#);
        let models = replaying(&replay);
        models.save_local(LOCAL_URL, "qwen3-8b").await.unwrap();
        assert_eq!(models.start_account().await, Some(local_account()));
        models.save_key(Secret::new("sk-ant-test-key-abcd")).await.unwrap();
        assert_eq!(
            models.start_account().await,
            Some(AccountRef::new(ANTHROPIC, SETUP_LABEL)),
            "the key set up last wins"
        );
    }

    #[tokio::test]
    async fn a_local_account_without_a_model_needs_setup() {
        let models = replaying(&Replay::new());
        models.credentials.add(local_account(), Credential::Local { base_url: LOCAL_URL.into() }).await.unwrap();
        assert_eq!(models.start_account().await, Some(local_account()), "the local default");
        let err = models.brain_for(&local_account()).await.err().unwrap();
        assert_eq!(err, "no model is chosen for local:default yet");
    }

    /// Stage 3, offline, with a local model: the real wiring picks the local brain for the saved
    /// account, it proposes a sandbox action, it is approved, and the answer streams in, at no cost.
    #[tokio::test]
    async fn stage_three_end_to_end_with_a_recorded_local_model() {
        let replay = Replay::new();
        replay
            .push_fixture("openai_compat/fixtures/models_local.json")
            .push_fixture("openai_compat/fixtures/tool_call.sse")
            .push_fixture("openai_compat/fixtures/text.sse");
        let models = replaying(&replay);
        models.save_local(LOCAL_URL, "qwen3-8b").await.unwrap();
        let account = models.start_account().await.unwrap();
        let brain = models.brain_for(&account).await.unwrap();

        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let mut controller = controller(
            brain,
            account.clone(),
            workspace(&std::env::temp_dir().join("nixxa-e2e-local")),
            &sandboxed(),
            Arc::new(MemoryStore::default()),
            &UiLink::new(ui_tx, AutoApprove::default()),
            test_scrubber(),
        );
        controller.start_session().await.expect("session starts");
        let task = tokio::spawn(async move { controller.run(cmd_rx).await });

        let mut app = App::new(false);
        let prompts = drive(&mut app, &mut ui_rx, &cmd_tx, "list the project").await;
        assert_eq!(app.account, Some(account));
        assert_eq!(app.brain.as_ref().unwrap().model, "qwen3-8b");
        assert_eq!(prompts, 1);
        assert_eq!(app.activity[0].state, ActivityState::Done);
        let said: Vec<_> = app.conversation.iter().map(|e| (e.speaker, e.text.as_str())).collect();
        assert_eq!(
            said,
            vec![
                (Speaker::User, "list the project"),
                (Speaker::Agent, "Let me look."),
                (Speaker::Agent, "Hello there.")
            ]
        );
        assert_eq!(app.cost_micro_usd, 0, "a local model costs nothing");
        cmd_tx.send(Command::Shutdown).await.unwrap();
        task.await.unwrap();
    }

    const OPENROUTER_KEY: &str = "sk-or-v1-signed-in-key-for-tests-5e5e";

    #[tokio::test]
    async fn a_signed_in_key_and_model_are_saved_and_become_the_start_account() {
        let replay = Replay::new();
        replay
            .push(200, r#"{"data":{"label":"nixxa","limit":null,"usage":0}}"#)
            .push_fixture("openai_compat/fixtures/models_rich.json");
        let models = replaying(&replay);
        let info = models.save_openrouter(Secret::new(OPENROUTER_KEY), "vendor/tool-model").await.expect("saved");
        let account = AccountRef::new(OPENROUTER, SETUP_LABEL);
        assert_eq!(info.account, account);
        assert_eq!(info.method, nixxa_contracts::AuthMethod::SignIn);
        assert_eq!(info.hint.as_deref(), Some("5e5e"));
        assert_eq!(models.start_account().await, Some(account.clone()));
        assert_eq!(
            models.credentials.get(&account).await.unwrap(),
            Credential::BearerToken { token: Secret::new(OPENROUTER_KEY), expires_at: None }
        );
        let brain = models.brain_for(&account).await.unwrap();
        assert_eq!(brain.describe().model, "vendor/tool-model");
    }

    #[tokio::test]
    async fn a_rejected_openrouter_key_is_not_saved() {
        let replay = Replay::new();
        replay.push(401, r#"{"error":{"message":"No auth credentials found","code":401}}"#);
        let models = replaying(&replay);
        let err = models.save_openrouter(Secret::new(OPENROUTER_KEY), "vendor/tool-model").await.unwrap_err();
        assert_eq!(err, "No auth credentials found");
        assert!(!err.contains(OPENROUTER_KEY));
        assert_eq!(models.start_account().await, None);
    }

    #[tokio::test]
    async fn openrouters_models_are_listed_with_the_signed_in_key() {
        let replay = Replay::new();
        replay.push_fixture("openai_compat/fixtures/models_rich.json");
        let models = replaying(&replay);
        let listed = models.list_openrouter_models(&Secret::new(OPENROUTER_KEY)).await.unwrap();
        assert_eq!(listed.iter().filter(|m| m.supports_tools == Some(true)).count(), 2);
    }

    /// Every file under `dir`, recursively.
    fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    /// The whole first-run setup, with logs going to a fresh data directory: the key reaches the
    /// keychain (in memory here) and nowhere else: not the screen, not a log, not any file.
    #[tokio::test]
    async fn setup_never_writes_the_key_to_the_screen_logs_or_disk() {
        const KEY: &str = "sk-ant-api03-DISKCHECK-SECRET-9q8r";
        let data = std::env::temp_dir().join(format!("nixxa-setup-disk-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        crate::logging::tests::enable_every_callsite();
        let appender = crate::logging::appender(&data.join("nixxa").join("logs")).unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(appender)
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let replay = Replay::new();
        replay.push(200, r#"{"id":"claude-sonnet-5-5"}"#);
        let anthropic = replaying(&replay);
        let mut app = App::new(false);
        app.setup_available = true;
        app.open_setup(true);
        app.handle_key(key(KeyCode::Char('1')));
        app.paste(KEY);
        let mut screens = vec![crate::ui::tests::screen(&mut app, 120, 24)];
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::SaveKey(secret)] = effects.as_slice() else { panic!("expected SaveKey, got {effects:?}") };
        screens.push(crate::ui::tests::screen(&mut app, 120, 24));
        let result = anthropic.save_key(secret.clone()).await;
        app.account_saved(&result);
        screens.push(crate::ui::tests::screen(&mut app, 120, 24));
        drop(guard);

        let account = AccountRef::new(ANTHROPIC, SETUP_LABEL);
        assert_eq!(anthropic.credentials.get(&account).await.unwrap(), Credential::ApiKey(Secret::new(KEY)), "stored");
        for screen in &screens {
            assert!(!screen.contains("DISKCHECK") && !screen.contains("sk-ant-api03"), "on screen:\n{screen}");
        }
        let files = files_under(&data);
        assert!(!files.is_empty(), "the log was written under the data dir");
        let mut log = String::new();
        for file in &files {
            let bytes = std::fs::read(file).unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("DISKCHECK"), "the key is in {}", file.display());
            log.push_str(&String::from_utf8_lossy(&bytes));
        }
        assert!(log.contains("API key saved to the OS keychain"), "{log}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[test]
    fn settings_reach_the_brain_config() {
        let settings = Settings {
            model: Some("claude-opus-5-5".into()),
            effort: Some(nixxa_brain::Effort::Medium),
            ..Settings::default()
        };
        let config = config(&settings);
        assert_eq!(config.model, "claude-opus-5-5");
        assert_eq!(config.effort, Some(nixxa_brain::Effort::Medium));
    }
}
