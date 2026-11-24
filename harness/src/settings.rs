//! What the user asked for on the command line and in the environment. Only the harness reads
//! these (D-007); modules get typed values.

use nixxa_brain::Effort;
use nixxa_contracts::Secret;

pub(crate) const USAGE: &str = "\
Usage: nixxa [--continue] [--fake]

  --continue   Resume the latest conversation started in this directory (a new one if there
               is none).
               ctrl-s lists every session.
  --fake       Use the scripted Stage 2 brain: no API key, no network. Nothing is saved to
               your history.
  -h, --help   Show this help.
  -V, --version

Environment:
  ANTHROPIC_API_KEY   Use this key (read-only; never saved).
  NIXXA_MODEL         Model to use (default: claude-sonnet-5-5).
  NIXXA_EFFORT        low | medium | high | xhigh | max (default: the model's own).
  NIXXA_LOG           Log level: error | warn | info | debug | trace (default: info).
  NIXXA_NOTIFY        When a job or a turn finishes while you're away: desktop (a desktop
                      notification and the bell; the default) | bell | off (the status bar's
                      badge only).
  NO_COLOR            Turn colour off.";

/// Settings for one run.
#[derive(Debug, Default)]
pub(crate) struct Settings {
    /// Use the scripted brain instead of a real model.
    pub(crate) fake: bool,
    /// Resume the latest session for the current directory (`--continue`).
    pub(crate) resume_latest: bool,
    /// Model override.
    pub(crate) model: Option<String>,
    /// Effort override.
    pub(crate) effort: Option<Effort>,
    /// `ANTHROPIC_API_KEY`, trimmed (a pasted trailing newline is common).
    pub(crate) env_key: Option<Secret>,
    /// What fires when a job or turn finishes while the user is away (`NIXXA_NOTIFY`).
    pub(crate) notify: crate::notify::Channels,
}

/// What the command line asks for.
#[derive(Debug)]
pub(crate) enum Parsed {
    Run(Settings),
    Help,
    Version,
}

/// Reads the arguments (without the program name) and the environment.
pub(crate) fn parse(
    args: impl IntoIterator<Item = String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Parsed, String> {
    let mut settings = Settings::default();
    for arg in args {
        match arg.as_str() {
            "--fake" => settings.fake = true,
            "--continue" | "-c" => settings.resume_latest = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    let var = |name: &str| env(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    settings.model = var("NIXXA_MODEL");
    settings.effort = match var("NIXXA_EFFORT").as_deref() {
        None => None,
        Some("low") => Some(Effort::Low),
        Some("medium") => Some(Effort::Medium),
        Some("high") => Some(Effort::High),
        Some("xhigh") => Some(Effort::XHigh),
        Some("max") => Some(Effort::Max),
        Some(other) => return Err(format!("NIXXA_EFFORT must be low, medium, high, xhigh or max, not `{other}`")),
    };
    settings.env_key = var("ANTHROPIC_API_KEY").map(Secret::new);
    if let Some(notify) = var("NIXXA_NOTIFY") {
        settings.notify = crate::notify::Channels::parse(&notify)?;
    }
    Ok(Parsed::Run(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    fn run(args: &[&str], vars: &[(&str, &str)]) -> Result<Settings, String> {
        match parse(args.iter().map(|a| (*a).to_string()), &env(vars))? {
            Parsed::Run(settings) => Ok(settings),
            other => Err(format!("{other:?}")),
        }
    }

    #[test]
    fn notifications_default_to_desktop_and_can_be_turned_down() {
        use crate::notify::Channels;
        assert_eq!(run(&[], &[]).unwrap().notify, Channels::Desktop);
        assert_eq!(run(&[], &[("NIXXA_NOTIFY", "bell")]).unwrap().notify, Channels::Bell);
        assert_eq!(run(&[], &[("NIXXA_NOTIFY", "off")]).unwrap().notify, Channels::Off);
        assert!(run(&[], &[("NIXXA_NOTIFY", "loud")]).unwrap_err().contains("off, bell or desktop"));
    }

    #[test]
    fn defaults_are_the_real_brain_with_no_overrides() {
        let settings = run(&[], &[]).unwrap();
        assert!(!settings.resume_latest);
        assert!(!settings.fake && settings.model.is_none() && settings.effort.is_none() && settings.env_key.is_none());
    }

    #[test]
    fn the_env_key_is_trimmed_and_blank_values_ignored() {
        let settings = run(&["--fake"], &[("ANTHROPIC_API_KEY", "  sk-ant-abc-1234\n"), ("NIXXA_MODEL", " ")]).unwrap();
        assert!(settings.fake);
        assert_eq!(settings.env_key.unwrap().expose(), "sk-ant-abc-1234");
        assert!(settings.model.is_none());
    }

    #[test]
    fn continue_asks_to_resume_the_latest_session() {
        assert!(run(&["--continue"], &[]).unwrap().resume_latest);
        let both = run(&["-c", "--fake"], &[]).unwrap();
        assert!(both.resume_latest && both.fake);
        assert!(USAGE.contains("--continue"), "--help lists it");
    }

    #[test]
    fn model_and_effort_are_read() {
        let settings = run(&[], &[("NIXXA_MODEL", "claude-opus-5-5"), ("NIXXA_EFFORT", "medium")]).unwrap();
        assert_eq!(settings.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(settings.effort, Some(Effort::Medium));
        assert!(run(&[], &[("NIXXA_EFFORT", "turbo")]).is_err());
    }

    #[test]
    fn help_version_and_unknown_arguments() {
        assert!(matches!(parse(["--help".to_string()], &env(&[])), Ok(Parsed::Help)));
        assert!(matches!(parse(["-V".to_string()], &env(&[])), Ok(Parsed::Version)));
        assert!(parse(["--turbo".to_string()], &env(&[])).unwrap_err().contains("unknown argument"));
    }

    #[test]
    fn debug_never_shows_the_key() {
        let settings = run(&[], &[("ANTHROPIC_API_KEY", "sk-ant-very-secret-1234")]).unwrap();
        assert!(!format!("{settings:?}").contains("very-secret"));
    }
}
