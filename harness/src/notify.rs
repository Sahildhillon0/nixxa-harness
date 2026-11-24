//! Notifications (D-069): when a background job or a turn finishes, the status bar's badge
//! counts it, and if the user is away, the terminal bell rings and, where there is one, a
//! desktop notification says what finished in one line.
//!
//! **Away** means any of: the host terminal window lost focus (focus reporting, where the
//! terminal supports it), the user is typing in the `ctrl+t` terminal pane, or no key was
//! pressed for [`IDLE`]. Otherwise only the badge changes. The badge clears at the next key
//! the user presses with the window in focus.
//!
//! The text never holds command output or model text: a job's id, exit and a cut-down command,
//! or that a turn finished, every part through `safe::line`. A command line can still carry
//! escape sequences or bidi characters; they are shown escaped, never passed on.
//!
//! `NIXXA_NOTIFY` chooses the channels for when the user is away: `desktop` (the default: a
//! desktop notification and the bell), `bell`, or `off` (the badge only).

use std::time::{Duration, Instant};

use crate::safe;

/// How long without a key press counts as away.
pub(crate) const IDLE: Duration = Duration::from_secs(60);
/// The longest command shown in a notification.
const COMMAND_CHARS: usize = 48;

/// Which channels fire when the user is away. The badge always counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Channels {
    /// The badge only.
    Off,
    /// The terminal bell.
    Bell,
    /// A desktop notification and the bell.
    #[default]
    Desktop,
}

impl Channels {
    /// From `NIXXA_NOTIFY`.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Ok(Self::Off),
            "bell" => Ok(Self::Bell),
            "desktop" | "on" => Ok(Self::Desktop),
            other => Err(format!("NIXXA_NOTIFY must be off, bell or desktop, not `{}`", safe::line(other))),
        }
    }
}

/// Something worth telling the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finished {
    /// A background job of the agent's ended.
    Job { job: String, exit_code: Option<i32>, command: String },
    /// A turn ended; `woken` when the controller had started it for ended jobs.
    Turn { woken: bool },
}

/// What to do about it, for the notifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Alert {
    Bell,
    Desktop { title: String, body: String },
}

/// Delivers alerts: the real one rings the terminal and asks the desktop; tests record them.
pub(crate) trait Notifier {
    fn bell(&mut self);
    fn desktop(&mut self, title: &str, body: &str);
}

/// Sends `alerts` to `notifier`.
pub(crate) fn deliver(alerts: Vec<Alert>, notifier: &mut dyn Notifier) {
    for alert in alerts {
        match alert {
            Alert::Bell => notifier.bell(),
            Alert::Desktop { title, body } => notifier.desktop(&title, &body),
        }
    }
}

/// Whether the user is here, and what they haven't seen.
#[derive(Debug)]
pub(crate) struct Notifications {
    pub(crate) channels: Channels,
    /// Whether the host terminal window has focus (true until told otherwise).
    window_focused: bool,
    last_key: Instant,
    /// Finishes since the user last pressed a key with the window in focus.
    pub(crate) unseen: u32,
    alerts: Vec<Alert>,
}

impl Notifications {
    pub(crate) fn new(channels: Channels, now: Instant) -> Self {
        Self { channels, window_focused: true, last_key: now, unseen: 0, alerts: Vec::new() }
    }

    /// The host terminal window gained or lost focus.
    pub(crate) fn focus(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// The user pressed a key: they are here, and have seen the badge.
    pub(crate) fn key(&mut self, now: Instant) {
        self.last_key = now;
        if self.window_focused {
            self.unseen = 0;
        }
    }

    /// Whether the user is away: the window is out of focus, they are typing in the terminal
    /// pane, or no key came for [`IDLE`].
    pub(crate) fn away(&self, now: Instant, in_terminal: bool) -> bool {
        !self.window_focused || in_terminal || now.duration_since(self.last_key) >= IDLE
    }

    /// Something finished: the badge counts it, and if the user is away the channels fire.
    pub(crate) fn finished(&mut self, what: &Finished, now: Instant, in_terminal: bool) {
        self.unseen = self.unseen.saturating_add(1);
        if !self.away(now, in_terminal) {
            return;
        }
        match self.channels {
            Channels::Off => {}
            Channels::Bell => self.alerts.push(Alert::Bell),
            Channels::Desktop => {
                self.alerts.push(Alert::Desktop { title: "Nixxa".into(), body: text(what) });
                self.alerts.push(Alert::Bell);
            }
        }
    }

    /// The alerts to deliver now.
    pub(crate) fn take_alerts(&mut self) -> Vec<Alert> {
        std::mem::take(&mut self.alerts)
    }
}

/// One safe line about what finished: never output or model text.
pub(crate) fn text(what: &Finished) -> String {
    match what {
        Finished::Job { job, exit_code, command } => {
            let how = match exit_code {
                Some(0) => "finished".to_string(),
                Some(code) => format!("failed (exit {code})"),
                None => "ended without exiting".to_string(),
            };
            let command = safe::line(command);
            let command: String = if command.chars().count() > COMMAND_CHARS {
                command.chars().take(COMMAND_CHARS - 1).chain(['…']).collect()
            } else {
                command
            };
            format!("job {} {how}: {command}", safe::line(job))
        }
        Finished::Turn { woken: false } => "the agent finished its turn".into(),
        Finished::Turn { woken: true } => "the agent finished the turn a job woke it for".into(),
    }
}

/// `text` with the markup characters notification servers interpret written as entities.
fn markup_escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The real notifier: the terminal bell, and the desktop's own notifications (`notify-send` on
/// Linux and the BSDs, `osascript` on macOS; Windows has the bell only). The text goes as
/// separate arguments, never through a shell.
pub(crate) struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn bell(&mut self) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x07").and_then(|()| out.flush());
    }

    fn desktop(&mut self, title: &str, body: &str) {
        let mut command = if cfg!(target_os = "macos") {
            let mut command = std::process::Command::new("osascript");
            command.args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
                title,
                body,
            ]);
            command
        } else if cfg!(windows) {
            return;
        } else {
            let mut command = std::process::Command::new("notify-send");
            // Many notification servers read the body as markup (`<b>`, `<a href>`).
            command.args(["--app-name=Nixxa", "--", &markup_escaped(title), &markup_escaped(body)]);
            command
        };
        let spawned = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            // Reaped on a thread of its own, so the UI never waits for the desktop.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(err) => tracing::debug!(%err, "no desktop notification"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Records what it was asked to do.
    #[derive(Default)]
    pub(crate) struct FakeNotifier {
        pub(crate) bells: u32,
        pub(crate) desktop: Vec<(String, String)>,
    }

    impl Notifier for FakeNotifier {
        fn bell(&mut self) {
            self.bells += 1;
        }
        fn desktop(&mut self, title: &str, body: &str) {
            self.desktop.push((title.into(), body.into()));
        }
    }

    fn job(command: &str) -> Finished {
        Finished::Job { job: "j1".into(), exit_code: Some(0), command: command.into() }
    }

    fn fire(notes: &mut Notifications, what: &Finished, at: Instant, in_terminal: bool) -> FakeNotifier {
        notes.finished(what, at, in_terminal);
        let mut fake = FakeNotifier::default();
        deliver(notes.take_alerts(), &mut fake);
        fake
    }

    #[test]
    fn a_present_user_only_gets_the_badge_and_an_away_one_gets_the_channels() {
        let t0 = Instant::now();
        let mut notes = Notifications::new(Channels::Desktop, t0);
        let fake = fire(&mut notes, &job("cargo build"), t0 + Duration::from_secs(5), false);
        assert_eq!((fake.bells, fake.desktop.len(), notes.unseen), (0, 0, 1), "present: the badge only");
        // Away: the window lost focus.
        notes.focus(false);
        let fake = fire(&mut notes, &job("cargo build"), t0 + Duration::from_secs(6), false);
        assert_eq!(fake.desktop, vec![("Nixxa".to_string(), "job j1 finished: cargo build".to_string())]);
        assert_eq!((fake.bells, notes.unseen), (1, 2));
        // Away: typing in the terminal pane.
        notes.focus(true);
        let fake = fire(&mut notes, &Finished::Turn { woken: false }, t0 + Duration::from_secs(7), true);
        assert_eq!(fake.desktop.len(), 1);
        // Away: no key for a minute.
        let fake = fire(&mut notes, &Finished::Turn { woken: true }, t0 + IDLE, false);
        assert_eq!(fake.desktop[0].1, "the agent finished the turn a job woke it for");
    }

    #[test]
    fn each_channel_setting_fires_what_it_says() {
        let t0 = Instant::now();
        for (channels, bells, desktop) in [(Channels::Off, 0, 0), (Channels::Bell, 1, 0), (Channels::Desktop, 1, 1)] {
            let mut notes = Notifications::new(channels, t0);
            notes.focus(false);
            let fake = fire(&mut notes, &job("make"), t0, false);
            assert_eq!((fake.bells, fake.desktop.len()), (bells, desktop), "{channels:?}");
            assert_eq!(notes.unseen, 1, "the badge counts whatever the setting");
        }
        assert_eq!(Channels::parse(" Bell ").unwrap(), Channels::Bell);
        assert_eq!(Channels::parse("off").unwrap(), Channels::Off);
        assert!(Channels::parse("\u{1b}]52;loud").unwrap_err().contains("^["), "the error is safe to show too");
    }

    #[test]
    fn notification_text_is_one_safe_line_without_output() {
        let hostile =
            "cargo build \u{1b}]52;c;cHduZWQ=\u{7} \u{202E}exe.txt\nrm -rf ~ and a very long tail that goes on";
        let what = Finished::Job { job: "j1\u{1b}[2J".into(), exit_code: Some(101), command: hostile.into() };
        let body = text(&what);
        assert!(body.starts_with("job j1^[[2J failed (exit 101): cargo build ^[]52;"), "{body}");
        assert!(!body.chars().any(|c| c.is_control() || ('\u{202A}'..='\u{202E}').contains(&c)), "{body:?}");
        assert!(body.ends_with('…') && body.chars().count() < 100, "cut down: {body}");
    }

    #[test]
    fn desktop_markup_is_escaped() {
        assert_eq!(
            markup_escaped("job j1 finished: <a href=\"x\">a & b</a>"),
            "job j1 finished: &lt;a href=\"x\"&gt;a &amp; b&lt;/a&gt;"
        );
    }

    #[test]
    fn the_badge_clears_when_the_user_is_back() {
        let t0 = Instant::now();
        let mut notes = Notifications::new(Channels::Off, t0);
        notes.focus(false);
        notes.finished(&job("a"), t0, false);
        notes.finished(&Finished::Turn { woken: false }, t0, false);
        notes.key(t0);
        assert_eq!(notes.unseen, 2, "a key with the window away isn't seeing it");
        notes.focus(true);
        notes.key(t0);
        assert_eq!(notes.unseen, 0);
    }
}
